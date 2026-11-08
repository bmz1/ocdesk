//! The Changes tab of the right panel: the diff for a scope (latest turn,
//! working tree, branch, commits), file by file, with inline comments.
//!
//! Hover a line and press `+` to comment on it. Comments collect at the top;
//! "Send to agent" turns them into one prompt with `path:line` references,
//! like Codex's and Claude's review panes. Nothing is staged or committed.

use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::model::PromptInput;
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};
use crate::ui::changes::{FileHooks, OnLineComment, render_changes};

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewComment {
    pub file: String,
    pub line: u32,
    pub snippet: String,
    pub text: String,
}

struct Editing {
    file: String,
    line: u32,
    snippet: String,
    input: Entity<InputState>,
}

pub struct ChangesPanel {
    store: Entity<Store>,
    controller: Entity<Controller>,
    comments: Vec<ReviewComment>,
    editing: Option<Editing>,
    collapsed: HashSet<String>,
    /// [`fingerprint`] of the store state last drawn.
    seen: u64,
    _subs: Vec<Subscription>,
}

/// What the Changes tab depends on in the store, hashed.
fn fingerprint(store: &Store) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let sid = store.selected.as_deref().unwrap_or_default();
    sid.hash(&mut h);
    if let Some(files) = store.diffs.get(sid) {
        for f in files {
            (
                &f.file,
                &f.status,
                &f.patch,
                f.additions,
                f.deletions,
                f.more,
            )
                .hash(&mut h);
        }
    } else {
        u64::MAX.hash(&mut h);
    }
    store.diff_loading.contains(sid).hash(&mut h);
    store.diff_errors.get(sid).hash(&mut h);
    store.preferences.viewed.len().hash(&mut h);
    let mut viewed: Vec<_> = store.preferences.viewed.iter().collect();
    viewed.sort_unstable_by_key(|(key, _)| *key);
    viewed.hash(&mut h);
    h.finish()
}

/// The prompt a batch of review comments becomes.
pub fn comments_prompt(comments: &[ReviewComment]) -> String {
    let mut s = String::from("Please address these review comments on the changes:\n");
    for c in comments {
        s.push_str(&format!("\n- `{}:{}`: {}", c.file, c.line, c.text.trim()));
        let snippet = c.snippet.trim();
        if !snippet.is_empty() {
            s.push_str(&format!("\n  > `{snippet}`"));
        }
    }
    s
}

impl ChangesPanel {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![
            cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| {
                if matches!(ev, StoreEvent::SelectionChanged) {
                    this.comments.clear();
                    this.editing = None;
                    this.collapsed.clear();
                    cx.notify();
                }
            }),
            // The store notifies on every streamed delta; redraw only when
            // what this panel shows changed.
            cx.observe(&store, |this, store, cx| {
                let f = fingerprint(store.read(cx));
                if f != this.seen {
                    this.seen = f;
                    cx.notify();
                }
            }),
        ];
        Self {
            store,
            controller,
            comments: Vec::new(),
            editing: None,
            collapsed: HashSet::new(),
            seen: 0,
            _subs: subs,
        }
    }

    fn start_comment(
        &mut self,
        file: String,
        line: u32,
        snippet: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Comment for the agent… (Enter saves, Esc cancels)")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = ev {
                this.save_comment(window, cx);
            }
        });
        self._subs.push(sub);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.editing = Some(Editing {
            file,
            line,
            snippet,
            input,
        });
        cx.notify();
    }

    fn save_comment(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.editing.take() else { return };
        let text = e.input.read(cx).value().trim().to_string();
        if !text.is_empty() {
            self.comments.push(ReviewComment {
                file: e.file,
                line: e.line,
                snippet: e.snippet,
                text,
            });
        }
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        if self.comments.is_empty() {
            return;
        }
        let text = comments_prompt(&self.comments);
        self.comments.clear();
        self.controller.update(cx, |c, cx| {
            c.send_prompt(
                PromptInput {
                    text,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.notify();
    }

    fn render_comment_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.comments.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let n = self.comments.len();
        Some(
            h_flex()
                .w_full()
                .gap_2()
                .px_3()
                .py_2()
                .items_center()
                .rounded_lg()
                .bg(theme.primary.opacity(0.1))
                .border_1()
                .border_color(theme.primary.opacity(0.4))
                .text_sm()
                .child(div().flex_1().child(format!(
                    "{n} review comment{}",
                    if n == 1 { "" } else { "s" }
                )))
                .child(
                    Button::new("clear-comments")
                        .label("Clear")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.comments.clear();
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("send-comments")
                        .label("Send to agent")
                        .primary()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                )
                .into_any_element(),
        )
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let toggle: crate::ui::changes::ToggleFile = {
            let view = view.clone();
            Rc::new(move |file, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    if !this.collapsed.remove(&file) {
                        this.collapsed.insert(file);
                    }
                    cx.notify();
                });
            })
        };
        let on_comment = {
            let view = view.clone();
            move |file: &str| -> OnLineComment {
                let (view, file) = (view.clone(), file.to_string());
                Rc::new(move |line, snippet, window, cx| {
                    let file = file.clone();
                    let _ = view.update(cx, |this, cx| {
                        this.start_comment(file, line, snippet, window, cx)
                    });
                })
            }
        };
        let theme = cx.theme().clone();
        let editing = self
            .editing
            .as_ref()
            .map(|e| (e.file.clone(), e.line, e.input.clone()));
        let comments = self.comments.clone();
        let cancel_view = view.clone();
        let save_view = view.clone();
        let below = move |file: &str| -> Vec<(u32, AnyElement)> {
            let mut out: Vec<(u32, AnyElement)> = comments
                .iter()
                .enumerate()
                .filter(|(_, c)| c.file == file)
                .map(|(i, c)| {
                    let view = view.clone();
                    (
                        c.line,
                        h_flex()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(theme.primary.opacity(0.1))
                            .text_xs()
                            .font_family(theme.font_family.clone())
                            .child(div().flex_1().child(c.text.clone()))
                            .child(
                                Button::new(SharedString::from(format!("rm-comment-{i}")))
                                    .icon(gpui_kit::component::IconName::Close)
                                    .ghost()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if i < this.comments.len() {
                                                this.comments.remove(i);
                                            }
                                            cx.notify();
                                        });
                                    }),
                            )
                            .into_any_element(),
                    )
                })
                .collect();
            if let Some((f, line, input)) = &editing
                && f == file
            {
                let (cancel_view, save_view) = (cancel_view.clone(), save_view.clone());
                out.push((
                    *line,
                    v_flex()
                        .gap_1()
                        .font_family(theme.font_family.clone())
                        .capture_action(move |_: &gpui_kit::component::input::Escape, _, cx| {
                            let _ = cancel_view.update(cx, |this, cx| {
                                this.editing = None;
                                cx.notify();
                            });
                        })
                        .child(Input::new(input).small())
                        .child(
                            h_flex().justify_end().child(
                                Button::new("save-comment")
                                    .label("Comment")
                                    .primary()
                                    .xsmall()
                                    .on_click(move |_, window, cx| {
                                        let _ = save_view
                                            .update(cx, |this, cx| this.save_comment(window, cx));
                                    }),
                            ),
                        )
                        .into_any_element(),
                ));
            }
            out
        };
        let hooks = FileHooks {
            collapsed: &self.collapsed,
            toggle,
            on_comment: &on_comment,
            below: &below,
        };
        let bar = self.render_comment_bar(cx);
        let body = render_changes(self.store.read(cx), self.controller.clone(), &hooks, cx);
        v_flex()
            .id("changes-panel")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .children(bar)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn fingerprint_tracks_content_and_viewed_file_identity_without_overflow() {
        let mut store = Store {
            selected: Some("session".into()),
            ..Default::default()
        };
        let file = crate::api::model::FileDiff {
            file: "a.rs".into(),
            status: "modified".into(),
            patch: "-a\n+b".into(),
            additions: 1,
            deletions: 1,
            more: false,
        };
        store.diffs.insert("session".into(), vec![file]);
        let before = fingerprint(&store);
        store.diffs.get_mut("session").unwrap()[0].patch = "-c\n+d".into();
        assert_ne!(before, fingerprint(&store));
        for i in 0..5 {
            store
                .preferences
                .viewed
                .insert(format!("file-{i}"), u64::MAX);
        }
        let before = fingerprint(&store);
        store.preferences.viewed.remove("file-0");
        store.preferences.viewed.insert("other".into(), u64::MAX);
        assert_ne!(before, fingerprint(&store));
    }

    #[::core::prelude::v1::test]
    fn comments_become_one_prompt_with_locations() {
        let p = comments_prompt(&[ReviewComment {
            file: "src/a.rs".into(),
            line: 12,
            snippet: "let x = 1;".into(),
            text: "use a const".into(),
        }]);
        assert!(p.contains("- `src/a.rs:12`: use a const"));
        assert!(p.contains("> `let x = 1;`"));
    }
}
