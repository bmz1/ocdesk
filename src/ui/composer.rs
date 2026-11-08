//! The prompt box: Enter sends, Shift+Enter inserts a newline, and the send
//! button turns into Stop while the selected session is busy.
//!
//! The footer holds the agent, model and variant pickers. Typing `@` opens a
//! popup of matching files (`/api/fs/find`) and subagents; picking one turns
//! it into an inline token that's sent as a prompt attachment. Up/Down at the
//! start of an empty or recalled prompt walk through earlier prompts.
//!
//! While the selected session runs, Enter steers the running turn (delivered
//! at its next step) and Ctrl/Cmd+Enter queues the prompt for when the turn
//! ends; pending items are listed above the text box. Esc twice stops the
//! turn. Each session keeps its own draft.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Enter, Escape, IndentInline, InlineToken, InputEvent, MoveDown, MoveLeft, MoveRight, MoveUp,
    Textarea, TextareaState, ToggleCodeActions,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{Delivery, FsEntry, InboxItem, Message, ModelRef, PermissionDecision};
use crate::controller::Controller;
use crate::store::{Connection, Store};
use crate::ui::mention::{self, AGENT_PREFIX, FILE_PREFIX, History, SKILL_PREFIX};

/// Wait this long after the last keystroke before searching files.
const FIND_DEBOUNCE: Duration = Duration::from_millis(120);
const MAX_ITEMS: usize = 8;
const SLASH_MAX_ITEMS: usize = 12;
/// A second Esc within this window stops the running turn.
const ESC_WINDOW: Duration = Duration::from_millis(1200);
const PLACEHOLDER_IDLE: &str = "Ask opencode anything… @ to mention files, / for commands";
const PLACEHOLDER_BUSY: &str = "Steer the running turn… (Ctrl+Enter queues it for later)";

#[derive(Clone, Debug, PartialEq)]
enum MentionItem {
    File(FsEntry),
    Agent {
        id: String,
        name: String,
    },
    // `/` menu entries.
    /// A client action (`compact`).
    Action {
        id: &'static str,
        detail: &'static str,
    },
    Command {
        name: String,
        detail: String,
    },
    Skill {
        id: String,
        name: String,
        detail: String,
    },
    Mcp {
        name: String,
        status: String,
        connected: bool,
    },
}

impl MentionItem {
    /// The inline token a pick becomes, for items that attach something.
    fn token(&self) -> Option<InlineToken> {
        Some(match self {
            MentionItem::File(f) => {
                InlineToken::new(format!("{FILE_PREFIX}{}", f.path), format!("@{}", f.path))
            }
            MentionItem::Agent { id, .. } => {
                InlineToken::new(format!("{AGENT_PREFIX}{id}"), format!("@{id}"))
            }
            MentionItem::Skill { id, name, .. } => {
                InlineToken::new(format!("{SKILL_PREFIX}{id}"), format!("/{name}"))
            }
            _ => return None,
        })
    }
}

/// The open `@` popup.
struct MentionPopup {
    /// Byte offset of the `@` in the text.
    at: usize,
    query: String,
    /// `/` menu entries (empty for `@`).
    slash: Vec<MentionItem>,
    agents: Vec<MentionItem>,
    files: Vec<MentionItem>,
    selected: usize,
    /// Bumped per query; stale searches are dropped.
    seq: u64,
}

impl MentionPopup {
    fn items(&self) -> impl Iterator<Item = &MentionItem> {
        let max = if self.slash.is_empty() {
            MAX_ITEMS
        } else {
            SLASH_MAX_ITEMS
        };
        self.slash
            .iter()
            .chain(&self.agents)
            .chain(&self.files)
            .take(max)
    }

    fn len(&self) -> usize {
        self.items().count()
    }
}

pub struct Composer {
    attachments: Vec<std::path::PathBuf>,
    attachment_drafts: HashMap<Option<String>, Vec<std::path::PathBuf>>,
    attachment_writes: usize,
    input: Entity<TextareaState>,
    controller: Entity<Controller>,
    store: Entity<Store>,
    mention: Option<MentionPopup>,
    mention_seq: u64,
    history: History,
    /// Prompts sent from this window, for history in new sessions.
    sent: Vec<String>,
    /// The text history last put in the box. `Change` is delivered after the
    /// update that caused it, so a flag can't tell our own edits apart; the
    /// text itself can.
    recalled: Option<String>,
    /// Unsent text per session (`None` = the start screen).
    drafts: HashMap<Option<String>, String>,
    /// The session the box's current text belongs to.
    draft_key: Option<String>,
    /// First Esc of a double-Esc stop.
    esc_armed: Option<Instant>,
    placeholder_busy: bool,
    /// The next paste goes in as text, however large (Cmd/Ctrl+Shift+V).
    paste_inline: bool,
    _subs: Vec<Subscription>,
}

fn draft_scope(store: &Store) -> Option<String> {
    store.selected.clone().or_else(|| {
        store
            .target_folder()
            .map(|f| format!("draft:{}", f.display()))
    })
}
mod attachments;

impl Composer {
    pub fn new(
        controller: Entity<Controller>,
        store: Entity<Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(PLACEHOLDER_IDLE)
                .auto_grow(1, 10)
                .submit_on_enter(true)
        });
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
                match ev {
                    InputEvent::PressEnter {
                        secondary,
                        shift: false,
                    } => this.submit(*secondary, window, cx),
                    InputEvent::Change => {
                        let value = this.input.read(cx).value();
                        if this.recalled.as_deref() != Some(value.as_ref()) {
                            this.recalled = None;
                            this.history.reset();
                        }
                        this.update_mention(cx);
                    }
                    InputEvent::Blur => this.close_mention(cx),
                    _ => {}
                }
                cx.notify();
            }),
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.swap_draft(window, cx);
                cx.notify();
            }),
        ];
        let draft_key = draft_scope(store.read(cx));
        Self {
            attachments: Vec::new(),
            attachment_drafts: HashMap::new(),
            attachment_writes: 0,
            input,
            controller,
            store,
            mention: None,
            mention_seq: 0,
            history: History::default(),
            sent: Vec::new(),
            recalled: None,
            drafts: HashMap::new(),
            draft_key,
            esc_armed: None,
            placeholder_busy: false,
            paste_inline: false,
            _subs: subs,
        }
    }

    /// On selection change, park the box's text under the old session and
    /// bring back the new one's, so a draft never goes to the wrong session.
    fn swap_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = draft_scope(self.store.read(cx));
        if selected == self.draft_key {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let old = std::mem::replace(&mut self.draft_key, selected.clone());
        self.attachment_drafts
            .insert(old.clone(), std::mem::take(&mut self.attachments));
        self.attachments = self.attachment_drafts.remove(&selected).unwrap_or_default();
        if text.trim().is_empty() {
            self.drafts.remove(&old);
        } else {
            self.drafts.insert(old, text);
        }
        let restored = self.drafts.remove(&selected).unwrap_or_default();
        self.close_mention(cx);
        self.history.reset();
        self.recalled = None;
        self.esc_armed = None;
        self.set_text(&restored, window, cx);
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn busy(&self, cx: &App) -> bool {
        let s = self.store.read(cx);
        s.selected.as_deref().is_some_and(|id| s.is_busy(id))
    }

    /// Send the box. While the session runs, `secondary` (Ctrl/Cmd+Enter)
    /// queues the prompt for after the turn; plain Enter steers it.
    fn submit(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let delivery = self.busy(cx).then_some(if secondary {
            Delivery::Queue
        } else {
            Delivery::Steer
        });
        let content = self.input.read(cx).content();
        if self.attachment_writes > 0 {
            self.attachment_notice("Wait for the pasted attachment to finish saving", cx);
            return;
        }
        if content.text().trim().is_empty() && self.attachments.is_empty() {
            return;
        }
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        // `/name args` runs a server command; `/compact` compacts.
        let text = content.text().to_string();
        if let Some((name, rest)) = mention::parse_command(&text) {
            let store = self.store.read(cx);
            let known = store
                .catalog_folder()
                .and_then(|f| store.tooling.get(&f))
                .is_some_and(|t| t.commands.iter().any(|c| c.name == name));
            let (name, rest) = (name.to_string(), rest.to_string());
            if (known || (name == "compact" && rest.is_empty())) && self.attachments.is_empty() {
                self.sent.push(text.trim().to_string());
                self.history.reset();
                self.close_mention(cx);
                self.set_text("", window, cx);
                self.controller.update(cx, |c, cx| {
                    if name == "compact" && !known {
                        c.compact(cx)
                    } else {
                        c.run_command(name, rest, delivery, cx)
                    }
                });
                return;
            }
        }
        let mut input = mention::build_prompt(
            content.text(),
            content
                .tokens()
                .iter()
                .map(|t| (t.range(), t.token().id().as_ref())),
            &folder,
        );
        if input.files.len() + self.attachments.len() > 100 {
            self.attachment_notice(
                "A prompt can contain at most 100 files, including @ mentions",
                cx,
            );
            return;
        }
        if let Some(missing) = self.attachments.iter().find(|p| !p.is_file()) {
            self.attachment_notice(
                format!("Attachment is no longer readable: {}", missing.display()),
                cx,
            );
            return;
        }
        input.attachments = std::mem::take(&mut self.attachments);
        input.delivery = delivery;
        self.sent.push(input.text.trim().to_string());
        self.history.reset();
        self.close_mention(cx);
        self.set_text("", window, cx);
        self.controller.update(cx, |c, cx| c.send_prompt(input, cx));
    }

    /// With an approval pending, Enter answers it instead of sending: an
    /// empty box allows once, typed text denies with that text as feedback.
    /// Returns whether it handled the key.
    fn answer_permission(
        &mut self,
        decision: Option<PermissionDecision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let first = {
            let store = self.store.read(cx);
            let Some(sid) = store.selected.as_deref() else {
                return false;
            };
            crate::ui::permission::pending(store, sid)
                .first()
                .map(|(r, _)| (r.session_id.clone(), r.id.clone()))
        };
        let Some((sid, rid)) = first else {
            return false;
        };
        let text = self.input.read(cx).value().trim().to_string();
        let (decision, message) = match decision {
            Some(d) => (d, None),
            None if text.is_empty() && self.attachments.is_empty() => {
                (PermissionDecision::Once, None)
            }
            None => (PermissionDecision::Reject, Some(text)),
        };
        if message.is_some() {
            self.set_text("", window, cx);
        }
        self.controller.update(cx, |c, cx| {
            c.reply_permission(sid, rid, decision, message, cx)
        });
        true
    }

    /// Esc with no popup: the first arms, a second within [`ESC_WINDOW`]
    /// stops the turn, so one stray Esc can't kill a long run.
    fn escape_to_stop(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.busy(cx) {
            self.esc_armed = None;
            return false;
        }
        match self.esc_armed {
            Some(t) if t.elapsed() < ESC_WINDOW => {
                self.esc_armed = None;
                self.controller.update(cx, |c, cx| c.abort_if_busy(cx));
            }
            _ => {
                self.esc_armed = Some(Instant::now());
                // Drop the hint when the window passes.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(ESC_WINDOW).await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
        }
        cx.notify();
        true
    }

    /// Withdraw a pending prompt and put its text back in the box to edit.
    fn edit_pending(&mut self, item: InboxItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        let current = self.input.read(cx).value().to_string();
        let text = if current.trim().is_empty() {
            item.text.clone()
        } else {
            format!("{}\n{current}", item.text)
        };
        let paths: Vec<_> = item.attachments.iter().filter_map(|a| a.path()).collect();
        if paths.len() != item.attachments.len() {
            self.attachment_notice(
                "Cannot edit this queued prompt: some attachments have no local file URI",
                cx,
            );
            return;
        }
        if paths.iter().any(|p| !p.is_file()) || paths.len() + self.attachments.len() > 100 {
            self.attachment_notice("Cannot edit this queued prompt: files are missing or the draft would exceed 100 attachments",cx);
            return;
        }
        self.add_attachments(paths, cx);
        self.set_text(&text, window, cx);
        self.controller
            .update(cx, |c, cx| c.cancel_inbox(sid, item.id, cx));
    }

    /// Pending (queued or steering) prompts of the selected session.
    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let sid = store.selected.clone()?;
        let items: Vec<InboxItem> = store
            .pending_prompts(&sid)
            .iter()
            .filter(|i| i.kind == "user")
            .cloned()
            .collect();
        if items.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let rows =
            items.into_iter().enumerate().map(|(ix, item)| {
                let queued = item.delivery == Delivery::Queue;
                let (sid_s, sid_r) = (sid.clone(), sid.clone());
                let (id_s, id_r) = (item.id.clone(), item.id.clone());
                let edit_item = item.clone();
                h_flex()
                    .id(SharedString::from(format!("pending-{ix}")))
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_lg()
                    .bg(theme.secondary.opacity(0.6))
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if queued {
                                theme.muted_foreground
                            } else {
                                theme.primary
                            })
                            .child(if queued { "Queued" } else { "Steering" }),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(
                        if item.attachments.is_empty() {
                            item.text.clone()
                        } else {
                            format!("{} · {} attachments", item.text, item.attachments.len())
                        },
                    ))
                    .when(queued, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("pending-steer-{ix}")))
                                .label("Steer now")
                                .ghost()
                                .xsmall()
                                .tooltip("Deliver at the next step instead of after the turn")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let (sid, id) = (sid_s.clone(), id_s.clone());
                                    this.controller.update(cx, |c, cx| c.steer_now(sid, id, cx));
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("pending-edit-{ix}")))
                            .icon(crate::ui::icons::pencil())
                            .ghost()
                            .xsmall()
                            .tooltip("Edit (takes it back)")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_pending(edit_item.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("pending-remove-{ix}")))
                            .icon(IconName::Close)
                            .ghost()
                            .xsmall()
                            .tooltip("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let (sid, id) = (sid_r.clone(), id_r.clone());
                                this.controller
                                    .update(cx, |c, cx| c.cancel_inbox(sid, id, cx));
                            })),
                    )
            });
        Some(v_flex().gap_1().children(rows).into_any_element())
    }

    fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| {
            i.set_value(text.to_string(), window, cx);
            let end = text.len();
            i.set_selected_range(end..end, cx);
        });
    }

    // ── @ mentions ───────────────────────────────────────────────────────

    fn close_mention(&mut self, cx: &mut Context<Self>) {
        if self.mention.take().is_some() {
            cx.notify();
        }
    }

    /// Re-evaluate the `@query` under the caret after an edit.
    fn update_mention(&mut self, cx: &mut Context<Self>) {
        let (text, caret, tokens) = {
            let i = self.input.read(cx);
            let tokens: Vec<_> = i.tokens().iter().map(|t| t.range()).collect();
            (i.value().to_string(), i.cursor(), tokens)
        };
        if tokens.is_empty()
            && let Some(query) = mention::slash_query(&text, caret)
        {
            self.open_slash(query.to_string(), cx);
            return;
        }
        let Some((at, query)) = mention::active_query(&text, caret, &tokens) else {
            self.close_mention(cx);
            return;
        };
        if self
            .mention
            .as_ref()
            .is_some_and(|m| m.at == at && m.query == query)
        {
            return;
        }
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        let catalog_folder = self.store.read(cx).catalog_folder();
        let agents: Vec<MentionItem> = catalog_folder
            .and_then(|f| self.store.read(cx).catalogs.get(&f))
            .map(|c| {
                c.agents
                    .iter()
                    .filter(|a| a.is_subagent())
                    .filter(|a| {
                        mention::fuzzy_match(&a.id, query) || mention::fuzzy_match(&a.name, query)
                    })
                    .map(|a| MentionItem::Agent {
                        id: a.id.clone(),
                        name: a.name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.mention_seq += 1;
        let seq = self.mention_seq;
        // Keep the previous file hits while the new search runs, so the list
        // doesn't flicker on every keystroke.
        let files = self
            .mention
            .take()
            .filter(|m| m.at == at)
            .map(|m| m.files)
            .unwrap_or_default();
        self.mention = Some(MentionPopup {
            at,
            query: query.to_string(),
            slash: Vec::new(),
            agents,
            files,
            selected: 0,
            seq,
        });
        cx.notify();

        let query = query.to_string();
        let controller = self.controller.clone();
        let exec = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            exec.timer(FIND_DEBOUNCE).await;
            let stale = this
                .update(cx, |this, _| {
                    this.mention.as_ref().map(|m| m.seq) != Some(seq)
                })
                .unwrap_or(true);
            if stale {
                return;
            }
            let Some(rx) = controller.update(cx, |c, _| c.find_files(folder, query)) else {
                return;
            };
            let found = match rx.await {
                Ok(Ok(found)) => found,
                Ok(Err(e)) => {
                    tracing::warn!("file search failed: {e:#}");
                    return;
                }
                Err(_) => return,
            };
            let _ = this.update(cx, |this, cx| {
                let Some(m) = this.mention.as_mut().filter(|m| m.seq == seq) else {
                    return;
                };
                // Directories can't be attached as prompt files.
                m.files = found
                    .into_iter()
                    .filter(|f| !f.is_dir)
                    .map(MentionItem::File)
                    .collect();
                m.selected = m.selected.min(m.len().saturating_sub(1));
                cx.notify();
            });
        })
        .detach();
    }

    /// Replace `@query` with the picked item's token and a trailing space.
    fn accept_mention(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.mention.take() else { return };
        let Some(item) = m.items().nth(ix).cloned() else {
            return;
        };
        let caret = self.input.read(cx).cursor();
        let range = m.at..caret;
        if let Some(token) = item.token() {
            self.input.update(cx, |i, cx| {
                if let Err(e) = i.replace_range_with_token(range, token, window, cx) {
                    tracing::warn!("could not insert mention: {e:?}");
                    return;
                }
                i.insert(" ", window, cx);
            });
            cx.notify();
            return;
        }
        // `/` entries that act instead of attaching.
        let rest = self.input.read(cx).value()[caret..].to_string();
        match item {
            MentionItem::Command { name, .. } => {
                // Arguments follow; Enter runs it (see `submit`).
                self.set_text(&format!("/{name} {}", rest.trim_start()), window, cx);
            }
            MentionItem::Action { id: "compact", .. } => {
                self.set_text(rest.trim_start(), window, cx);
                self.controller.update(cx, |c, cx| c.compact(cx));
            }
            MentionItem::Mcp {
                name, connected, ..
            } => {
                self.set_text(rest.trim_start(), window, cx);
                self.controller
                    .update(cx, |c, cx| c.set_mcp_connected(name, !connected, cx));
            }
            _ => {}
        }
        cx.notify();
    }

    /// Whether the box is in `/` menu mode (first word starts with `/`).
    fn slash_mode(&self, cx: &App) -> bool {
        let i = self.input.read(cx);
        mention::slash_query(&i.value(), i.cursor()).is_some()
    }

    /// The `/` menu: client actions, the folder's server commands, skills
    /// and MCP servers (picking one toggles its connection).
    fn open_slash(&mut self, query: String, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let tooling = store
            .catalog_folder()
            .and_then(|f| store.tooling.get(&f).cloned())
            .unwrap_or_default();
        let has_session = store.selected.is_some();
        let q = query.as_str();
        let hit = |s: &str| mention::fuzzy_match(s, q);
        let mut items = Vec::new();
        if has_session && hit("compact") {
            items.push(MentionItem::Action {
                id: "compact",
                detail: "Summarize the conversation to free up context",
            });
        }
        items.extend(tooling.commands.iter().filter(|c| hit(&c.name)).map(|c| {
            MentionItem::Command {
                name: c.name.clone(),
                detail: c.description.clone(),
            }
        }));
        items.extend(
            tooling
                .skills
                .iter()
                .filter(|sk| hit(&sk.id) || hit(&sk.name))
                .map(|sk| MentionItem::Skill {
                    id: sk.id.clone(),
                    name: sk.id.clone(),
                    detail: sk.description.clone(),
                }),
        );
        items.extend(
            tooling
                .mcp
                .iter()
                .filter(|m| hit(&m.name) || hit("mcp"))
                .map(|m| MentionItem::Mcp {
                    name: m.name.clone(),
                    status: m.error.clone().unwrap_or_else(|| m.status.clone()),
                    connected: m.connected(),
                }),
        );
        self.mention_seq += 1;
        let selected = self
            .mention
            .as_ref()
            .filter(|m| !m.slash.is_empty() && m.query == query)
            .map_or(0, |m| m.selected);
        self.mention = Some(MentionPopup {
            at: 0,
            query,
            slash: items,
            agents: Vec::new(),
            files: Vec::new(),
            selected,
            seq: self.mention_seq,
        });
        cx.notify();
    }

    fn move_mention(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(m) = self.mention.as_mut() {
            let n = m.len() as isize;
            if n > 0 {
                m.selected = (m.selected as isize + delta).rem_euclid(n) as usize;
                cx.notify();
            }
        }
    }

    // ── history ──────────────────────────────────────────────────────────

    /// Earlier prompts: the selected session's, else this window's.
    fn history_entries(&self, cx: &App) -> Vec<String> {
        let store = self.store.read(cx);
        match store.selected_conversation() {
            Some(conv) if store.selected.is_some() => conv
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::User(u) if !u.text.trim().is_empty() => Some(u.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => self.sent.clone(),
        }
    }

    /// Up/Down only recall history from an empty prompt or one that was
    /// itself recalled, and only with the caret at the start (Up) or end
    /// (Down), so moving through a multi-line draft still works.
    fn recall(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (text, caret) = {
            let i = self.input.read(cx);
            (i.value().to_string(), i.cursor())
        };
        if !(text.is_empty() || self.history.browsing()) {
            return false;
        }
        if (older && caret != 0) || (!older && caret != text.len()) {
            return false;
        }
        let entries = self.history_entries(cx);
        let next = if older {
            self.history.older(&entries, &text)
        } else {
            self.history.newer(&entries)
        };
        let Some(next) = next else { return false };
        self.recalled = self.history.browsing().then(|| next.clone());
        self.set_text(&next, window, cx);
        if older {
            self.input
                .update(cx, |i, cx| i.set_selected_range(0..0, cx));
        }
        cx.notify();
        true
    }

    // ── rendering ────────────────────────────────────────────────────────

    fn render_mention(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let m = self.mention.as_ref()?;
        let theme = cx.theme().clone();
        let rows: Vec<AnyElement> = m
            .items()
            .enumerate()
            .map(|(ix, item)| {
                use crate::ui::icons;
                let (icon, label, detail): (Icon, String, String) = match item {
                    MentionItem::File(f) => {
                        let (dir, name) = match f.path.rsplit_once('/') {
                            Some((d, n)) => (d.to_string(), n.to_string()),
                            None => (String::new(), f.path.clone()),
                        };
                        (Icon::new(IconName::File), name, dir)
                    }
                    MentionItem::Agent { id, name } => (
                        Icon::new(IconName::Bot),
                        id.clone(),
                        format!("{name} agent"),
                    ),
                    MentionItem::Action { id, detail } => {
                        (icons::layers(), format!("/{id}"), detail.to_string())
                    }
                    MentionItem::Command { name, detail } => {
                        (icons::slash(), format!("/{name}"), detail.clone())
                    }
                    MentionItem::Skill { name, detail, .. } => (
                        icons::sparkles(),
                        format!("/{name}"),
                        format!("skill · {detail}"),
                    ),
                    MentionItem::Mcp {
                        name,
                        status,
                        connected,
                    } => (
                        icons::plug(),
                        name.clone(),
                        format!(
                            "MCP · {status} · {}",
                            if *connected { "disconnect" } else { "connect" }
                        ),
                    ),
                };
                let selected = ix == m.selected;
                h_flex()
                    .id(("mention", ix))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.accent))
                    .hover(|d| d.bg(theme.accent))
                    .child(icon.small().text_color(theme.muted_foreground))
                    .child(div().text_sm().child(label))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accept_mention(ix, window, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let empty_text = if m.at == 0 && m.agents.is_empty() && self.slash_mode(cx) {
            "No matching commands"
        } else {
            "No matching files"
        };
        let popup = v_flex()
            .absolute()
            .bottom_full()
            .left_0()
            .right_0()
            .mb_2()
            .p_1()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .text_color(theme.popover_foreground)
            .when(empty, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(empty_text),
                )
            })
            .children(rows);
        Some(deferred(popup).with_priority(1))
    }

    fn render_pickers(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let store = self.store.read(cx);
        let catalog = store.target_catalog().cloned().unwrap_or_default();
        let model = store.effective_model();
        let agent = store.effective_agent();
        let controller = self.controller.clone();
        let mut row = h_flex().gap_0p5().items_center().min_w_0();

        let selected = store.selected.clone();
        let mode = selected
            .as_ref()
            .and_then(|sid| store.session_modes.get(sid).copied().flatten());
        let label = if let Some(sid) = &selected {
            if store.mode_sending.contains(sid) {
                "Saving permissions…"
            } else if !store.session_modes.contains_key(sid) {
                "Permissions…"
            } else {
                mode.map(|m| m.label()).unwrap_or("Custom permissions")
            }
        } else {
            store
                .draft_permission
                .unwrap_or_else(|| {
                    store
                        .target_folder()
                        .map(|f| store.preferences.permission_for(&f))
                        .unwrap_or_default()
                })
                .label()
        };
        // Permission mode: a quiet icon on the default (Supervised), the
        // mode's name otherwise, so a risky mode is always visible.
        let non_default = !matches!(label, "Supervised" | "Permissions…" | "Saving permissions…");
        let full = label == crate::settings::PermissionMode::FullAccess.label();
        let theme = cx.theme().clone();
        let c = controller.clone();
        let mut perm = Button::new("permission-mode")
            .icon(crate::ui::icons::shield())
            .ghost()
            .xsmall();
        if non_default {
            perm = perm.label(label);
        }
        if full {
            perm = perm.text_color(theme.warning);
        }
        let perm = perm
            .tooltip(format!(
                "Permissions: {label}. Saved project grants still apply (Settings). Cycle with {}.",
                if cfg!(target_os = "macos") {
                    "Cmd+Shift+M"
                } else {
                    "Ctrl+Shift+M"
                }
            ))
            .disabled(
                selected
                    .as_ref()
                    .is_some_and(|sid| store.mode_sending.contains(sid))
                    || store.connection() != Connection::Connected,
            )
            .dropdown_menu(move |mut menu, _, _| {
                for mode in crate::settings::PermissionMode::ALL {
                    let c = c.clone();
                    menu = menu.item(PopupMenuItem::new(mode.label()).on_click(move |_, _, cx| {
                        c.update(cx, |c, cx| c.set_permission_mode(mode, cx))
                    }));
                }
                menu
            });
        row = row.child(perm);
        // Agent.
        let primary: Vec<_> = catalog
            .agents
            .iter()
            .filter(|a| a.is_primary())
            .cloned()
            .collect();
        if !primary.is_empty() {
            let label = agent
                .as_ref()
                .and_then(|id| primary.iter().find(|a| &a.id == id))
                .map(|a| a.name.clone())
                .or_else(|| agent.clone())
                .unwrap_or_else(|| "Agent".into());
            let c = controller.clone();
            let current = agent.clone();
            row = row.child(
                crate::ui::kit::quiet_select("agent-picker", label)
                    .tooltip("Agent")
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                        for a in &primary {
                            let (c, id) = (c.clone(), a.id.clone());
                            menu = menu.item(
                                PopupMenuItem::new(a.name.clone())
                                    .checked(current.as_ref() == Some(&a.id))
                                    .on_click(move |_, _, cx| {
                                        let id = id.clone();
                                        c.update(cx, |c, cx| c.choose_agent(id, cx));
                                    }),
                            );
                        }
                        menu
                    }),
            );
        }

        // Model, grouped by provider.
        if !catalog.models.is_empty() {
            let info = model.as_ref().and_then(|m| catalog.model(m)).cloned();
            let label = info
                .as_ref()
                .map(|m| m.name.clone())
                .or_else(|| model.as_ref().map(|m| m.id.clone()))
                .unwrap_or_else(|| "Model".into());
            // The server lists models in no provider order; group each
            // provider's models together (first-appearance order, stable).
            let mut models = catalog.models.clone();
            let mut providers: Vec<String> = Vec::new();
            for m in &models {
                if !providers.contains(&m.provider_id) {
                    providers.push(m.provider_id.clone());
                }
            }
            models.sort_by_key(|m| providers.iter().position(|p| *p == m.provider_id));
            let c = controller.clone();
            let current = model.clone();
            row = row.child(
                crate::ui::kit::quiet_select("model-picker", label)
                    .tooltip("Model")
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                        menu = menu.scrollable(true).max_h(px(360.)).min_w(px(220.));
                        let mut provider: Option<&str> = None;
                        for m in &models {
                            if provider != Some(m.provider_id.as_str()) {
                                if provider.is_some() {
                                    menu = menu.separator();
                                }
                                menu = menu.label(m.provider_id.clone());
                                provider = Some(&m.provider_id);
                            }
                            let r = m.as_ref();
                            let checked = current.as_ref().is_some_and(|cur| cur.same_model(&r));
                            let c = c.clone();
                            menu = menu.item(
                                PopupMenuItem::new(m.name.clone())
                                    .checked(checked)
                                    .on_click(move |_, _, cx| {
                                        let r = r.clone();
                                        c.update(cx, |c, cx| c.choose_model(r, cx));
                                    }),
                            );
                        }
                        menu
                    }),
            );

            // Variant (reasoning effort), when the model has any.
            if let (Some(info), Some(current)) = (info, model)
                && !info.variants.is_empty()
            {
                let label = current
                    .variant
                    .clone()
                    .map(|v| {
                        let mut c = v.chars();
                        c.next()
                            .map(|f| f.to_uppercase().chain(c).collect::<String>())
                            .unwrap_or_default()
                    })
                    .unwrap_or_else(|| "Default effort".into());
                let c = controller.clone();
                row = row.child(
                    crate::ui::kit::quiet_select("variant-picker", label)
                        .tooltip("Reasoning effort")
                        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                            let options = std::iter::once(None)
                                .chain(info.variants.iter().cloned().map(Some));
                            for v in options {
                                let pick = ModelRef {
                                    variant: v.clone(),
                                    ..current.clone()
                                };
                                let c = c.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(
                                        v.clone().unwrap_or_else(|| "default".into()),
                                    )
                                    .checked(current.variant == v)
                                    .on_click(
                                        move |_, _, cx| {
                                            let pick = pick.clone();
                                            c.update(cx, |c, cx| c.choose_model(pick, cx));
                                        },
                                    ),
                                );
                            }
                            menu
                        }),
                );
            }
        }
        row
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy(cx);
        if busy != self.placeholder_busy {
            self.placeholder_busy = busy;
            let text = if busy {
                PLACEHOLDER_BUSY
            } else {
                PLACEHOLDER_IDLE
            };
            self.input
                .update(cx, |i, cx| i.set_placeholder(text, window, cx));
        }
        let connected = self.store.read(cx).connection() == Connection::Connected;
        let empty = self.input.read(cx).value().trim().is_empty() && self.attachments.is_empty();
        let esc_armed = self.esc_armed.is_some_and(|t| t.elapsed() < ESC_WINDOW);
        let popup = self.render_mention(cx);
        let pending = self.render_pending(cx);
        let pickers = self.render_pickers(cx);
        let theme = cx.theme().clone();

        let send = Button::new("send")
            .icon(IconName::ArrowUp)
            .small()
            .rounded(px(999.))
            .primary()
            .tooltip(if busy {
                "Steer the running turn (Enter) · queue (Ctrl+Enter)"
            } else {
                "Send (Enter)"
            })
            .disabled(empty || !connected || self.attachment_writes > 0)
            .on_click(cx.listener(|this, _, window, cx| this.submit(false, window, cx)));
        let stop = busy.then(|| {
            Button::new("stop")
                .icon(IconName::Square)
                .small()
                .rounded(px(999.))
                .when(empty, |b| b.primary())
                .when(!empty, |b| b.outline())
                .tooltip("Stop (Esc Esc)")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.update(cx, |c, cx| c.abort(cx));
                }))
        });
        let status = if esc_armed {
            Some(("Press Esc again to stop", theme.warning))
        } else if busy {
            Some(("Working…", theme.muted_foreground))
        } else {
            None
        };

        let paste_target = cx.weak_entity();
        let attachment_chips = self.render_attachments(cx);
        v_flex()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_attachments(paths.paths().to_vec(), cx);
            }))
            .relative()
            .w_full()
            .rounded_3xl()
            .border_1()
            .border_color(theme.border)
            // A raised surface, so the box stands out from the page.
            .bg(theme.secondary.opacity(0.35))
            .shadow_sm()
            .px_3()
            .pt_2()
            .pb_2()
            .gap_1()
            .children(popup)
            // Capture phase: these run before the textarea's own bindings,
            // so the popup and history can claim the keys.
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.mention.is_some() {
                    this.move_mention(-1, cx);
                    cx.stop_propagation();
                } else if this.recall(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.mention.is_some() {
                    this.move_mention(1, cx);
                    cx.stop_propagation();
                } else if this.recall(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, e: &Enter, window, cx| {
                if !e.shift && !e.secondary && this.mention.as_ref().is_some_and(|m| m.len() > 0) {
                    let ix = this.mention.as_ref().map_or(0, |m| m.selected);
                    this.accept_mention(ix, window, cx);
                    cx.stop_propagation();
                } else if !e.shift
                    && !e.secondary
                    && this.mention.is_none()
                    && this.answer_permission(None, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.mention.as_ref().is_some_and(|m| m.len() > 0) {
                    let ix = this.mention.as_ref().map_or(0, |m| m.selected);
                    this.accept_mention(ix, window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.mention.is_some() {
                    this.close_mention(cx);
                    cx.stop_propagation();
                } else if (this.input.read(cx).value().trim().is_empty()
                    && this.answer_permission(Some(PermissionDecision::Reject), window, cx))
                    || this.escape_to_stop(cx)
                {
                    cx.stop_propagation();
                }
            }))
            // The textarea binds secondary-. to code actions (unused here);
            // claim it for the app-wide Stop shortcut.
            .capture_action(cx.listener(|this, _: &ToggleCodeActions, _, cx| {
                let prefs = &this.store.read(cx).preferences;
                if prefs
                    .keybindings
                    .get("ocdesk::StopTurn")
                    .is_none_or(|k| k == "secondary-.")
                {
                    this.controller.update(cx, |c, cx| c.abort_if_busy(cx));
                    cx.stop_propagation();
                }
            }))
            .capture_action(
                cx.listener(|this, _: &crate::ui::keys::PasteAsText, window, cx| {
                    this.paste_inline = true;
                    window.dispatch_action(Box::new(gpui_kit::component::input::Paste), cx);
                    cx.stop_propagation();
                }),
            )
            // The text box binds Cmd/Ctrl+F to its own search; the app's
            // find-in-conversation is more useful here.
            .capture_action(
                cx.listener(|_, _: &gpui_kit::component::input::Search, window, cx| {
                    window.dispatch_action(Box::new(crate::ui::keys::FindInThread), cx);
                    cx.stop_propagation();
                }),
            )
            .capture_action(cx.listener(|this, _: &MoveLeft, _, cx| this.close_mention(cx)))
            .capture_action(cx.listener(|this, _: &MoveRight, _, cx| this.close_mention(cx)))
            .children(pending)
            .child(attachment_chips)
            .child(
                Textarea::new(&self.input)
                    .appearance(false)
                    .on_paste(move |item, _, cx| {
                        paste_target
                            .update(cx, |this, cx| this.paste_attachments(item, cx))
                            .unwrap_or(false)
                    }),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("attach-files")
                                    .label("+")
                                    .ghost()
                                    .small()
                                    .tooltip("Attach files or images (up to 100)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_attachments(window, cx)
                                    })),
                            )
                            .child(pickers),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when_some(status, |d, (text, color)| {
                                d.child(div().text_xs().text_color(color).child(text))
                            })
                            .children(stop)
                            .when(!busy || !empty, |d| d.child(send)),
                    ),
            )
    }
}
