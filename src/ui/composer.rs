//! The prompt box: Enter sends, Shift+Enter inserts a newline, and the send
//! button turns into Stop while the selected session is busy.
//!
//! The footer holds the agent, model and variant pickers. Typing `@` opens a
//! popup of matching files (`/api/fs/find`) and subagents; picking one turns
//! it into an inline token that's sent as a prompt attachment. Up/Down at the
//! start of an empty or recalled prompt walk through earlier prompts.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Enter, Escape, IndentInline, InlineToken, InputEvent, MoveDown, MoveLeft, MoveRight, MoveUp,
    Textarea, TextareaState,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{FsEntry, Message, ModelRef};
use crate::controller::Controller;
use crate::store::{Connection, Store};
use crate::ui::mention::{self, AGENT_PREFIX, FILE_PREFIX, History};

/// Wait this long after the last keystroke before searching files.
const FIND_DEBOUNCE: Duration = Duration::from_millis(120);
const MAX_ITEMS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
enum MentionItem {
    File(FsEntry),
    Agent { id: String, name: String },
}

impl MentionItem {
    fn token(&self) -> InlineToken {
        match self {
            MentionItem::File(f) => {
                InlineToken::new(format!("{FILE_PREFIX}{}", f.path), format!("@{}", f.path))
            }
            MentionItem::Agent { id, .. } => {
                InlineToken::new(format!("{AGENT_PREFIX}{id}"), format!("@{id}"))
            }
        }
    }
}

/// The open `@` popup.
struct MentionPopup {
    /// Byte offset of the `@` in the text.
    at: usize,
    query: String,
    agents: Vec<MentionItem>,
    files: Vec<MentionItem>,
    selected: usize,
    /// Bumped per query; stale searches are dropped.
    seq: u64,
}

impl MentionPopup {
    fn items(&self) -> impl Iterator<Item = &MentionItem> {
        self.agents.iter().chain(&self.files).take(MAX_ITEMS)
    }

    fn len(&self) -> usize {
        self.items().count()
    }
}

pub struct Composer {
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
    _subs: Vec<Subscription>,
}

impl Composer {
    pub fn new(
        controller: Entity<Controller>,
        store: Entity<Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask opencode anything… @ to mention files")
                .auto_grow(1, 10)
                .submit_on_enter(true)
        });
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
                match ev {
                    InputEvent::PressEnter {
                        secondary: false,
                        shift: false,
                    } => this.submit(window, cx),
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
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            input,
            controller,
            store,
            mention: None,
            mention_seq: 0,
            history: History::default(),
            sent: Vec::new(),
            recalled: None,
            _subs: subs,
        }
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn busy(&self, cx: &App) -> bool {
        let s = self.store.read(cx);
        s.selected.as_deref().is_some_and(|id| s.is_busy(id))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy(cx) {
            return;
        }
        let content = self.input.read(cx).content();
        if content.text().trim().is_empty() {
            return;
        }
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        let input = mention::build_prompt(
            content.text(),
            content
                .tokens()
                .iter()
                .map(|t| (t.range(), t.token().id().as_ref())),
            &folder,
        );
        self.sent.push(input.text.trim().to_string());
        self.history.reset();
        self.close_mention(cx);
        self.set_text("", window, cx);
        self.controller.update(cx, |c, cx| c.send_prompt(input, cx));
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
        let agents: Vec<MentionItem> = self
            .store
            .read(cx)
            .catalogs
            .get(&folder)
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
        self.input.update(cx, |i, cx| {
            if let Err(e) = i.replace_range_with_token(m.at..caret, item.token(), window, cx) {
                tracing::warn!("could not insert mention: {e:?}");
                return;
            }
            i.insert(" ", window, cx);
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
        let theme = cx.theme();
        let rows: Vec<AnyElement> = m
            .items()
            .enumerate()
            .map(|(ix, item)| {
                let (icon, label, detail) = match item {
                    MentionItem::File(f) => {
                        let (dir, name) = match f.path.rsplit_once('/') {
                            Some((d, n)) => (d.to_string(), n.to_string()),
                            None => (String::new(), f.path.clone()),
                        };
                        (IconName::File, name, dir)
                    }
                    MentionItem::Agent { id, name } => {
                        (IconName::Bot, id.clone(), format!("{name} agent"))
                    }
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
                    .child(Icon::new(icon).small().text_color(theme.muted_foreground))
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
                        .child("No matching files"),
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
                Button::new("agent-picker")
                    .label(label)
                    .icon(IconName::Bot)
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
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
            let models = catalog.models.clone();
            let c = controller.clone();
            let current = model.clone();
            row = row.child(
                Button::new("model-picker")
                    .label(label)
                    .icon(IconName::Cpu)
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
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
                let label = current.variant.clone().unwrap_or_else(|| "default".into());
                let c = controller.clone();
                row = row.child(
                    Button::new("variant-picker")
                        .label(label)
                        .ghost()
                        .xsmall()
                        .dropdown_caret(true)
                        .tooltip("Variant")
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy(cx);
        let connected = self.store.read(cx).connection() == Connection::Connected;
        let empty = self.input.read(cx).value().trim().is_empty();
        let popup = self.render_mention(cx);
        let pickers = self.render_pickers(cx);
        let theme = cx.theme();

        let action = if busy {
            Button::new("stop")
                .icon(IconName::Square)
                .small()
                .rounded(px(999.))
                .primary()
                .tooltip("Stop")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.update(cx, |c, cx| c.abort(cx));
                }))
        } else {
            Button::new("send")
                .icon(IconName::ArrowUp)
                .small()
                .rounded(px(999.))
                .primary()
                .tooltip("Send (Enter)")
                .disabled(empty || !connected)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        v_flex()
            .relative()
            .w_full()
            .rounded_3xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
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
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.mention.as_ref().is_some_and(|m| m.len() > 0) {
                    let ix = this.mention.as_ref().map_or(0, |m| m.selected);
                    this.accept_mention(ix, window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.mention.is_some() {
                    this.close_mention(cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveLeft, _, cx| this.close_mention(cx)))
            .capture_action(cx.listener(|this, _: &MoveRight, _, cx| this.close_mention(cx)))
            .child(Textarea::new(&self.input).appearance(false))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(pickers)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(busy, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Working…"),
                                )
                            })
                            .child(action),
                    ),
            )
    }
}
