//! The Pull request tab of the right panel: the current branch's PR (via
//! `gh`), its checks with failure logs, and unresolved review comments,
//! each with a button that hands them to the agent. Only shown when `gh`
//! is installed.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::github::{Check, CheckState, Comment, PullRequest, Thread};
use crate::store::{CheckLog, Store, StoreEvent};
use crate::ui::icons;
use crate::ui::kit::section_label;

pub struct PullRequestPanel {
    store: Entity<Store>,
    controller: Entity<Controller>,
    /// Failed checks whose log is shown, by link.
    open_logs: HashSet<String>,
    /// Comments left out of "Send to agent", by URL (all are in by default).
    excluded: HashSet<String>,
    _sub: Subscription,
}

/// A thread or conversation comment's identity for selection.
fn thread_key(t: &Thread) -> String {
    t.comments
        .first()
        .map(|c| c.url.clone())
        .unwrap_or_default()
}

impl PullRequestPanel {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Everything it shows: the PR state, the folder (selection) and
        // its branch. Not every store change, which includes streaming.
        let sub = cx.subscribe(&store, |_, _, ev: &StoreEvent, cx| {
            if matches!(
                ev,
                StoreEvent::PullRequestChanged
                    | StoreEvent::SelectionChanged
                    | StoreEvent::FoldersChanged
                    | StoreEvent::SessionsChanged
                    | StoreEvent::FolderInfoChanged
            ) {
                cx.notify();
            }
        });
        Self {
            store,
            controller,
            open_logs: HashSet::new(),
            excluded: HashSet::new(),
            _sub: sub,
        }
    }

    fn dir(&self, cx: &App) -> Option<PathBuf> {
        self.store.read(cx).target_folder()
    }

    /// Fetch now (the tab was opened, or Refresh).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = self.dir(cx) {
            self.controller.update(cx, |c, cx| c.refresh_pr(dir, cx));
        }
    }

    fn toggle_log(&mut self, check: &Check, cx: &mut Context<Self>) {
        if !self.open_logs.remove(&check.link) {
            self.open_logs.insert(check.link.clone());
            if let Some(dir) = self.dir(cx) {
                let check = check.clone();
                self.controller
                    .update(cx, |c, cx| c.load_check_log(dir, check, cx));
            }
        }
        cx.notify();
    }

    fn send_failures(&mut self, pr: &PullRequest, checks: Vec<Check>, cx: &mut Context<Self>) {
        let Some(dir) = self.dir(cx) else { return };
        let pr = pr.clone();
        self.controller
            .update(cx, |c, cx| c.send_ci_failures(dir, pr, checks, cx));
    }

    fn send_comments(&mut self, pr: &PullRequest, cx: &mut Context<Self>) {
        let threads: Vec<&Thread> = pr
            .threads
            .iter()
            .filter(|t| !self.excluded.contains(&thread_key(t)))
            .collect();
        let comments: Vec<&Comment> = pr
            .comments
            .iter()
            .filter(|c| !self.excluded.contains(&c.url))
            .collect();
        if threads.is_empty() && comments.is_empty() {
            return;
        }
        let text = crate::github::comments_prompt(pr, &threads, &comments);
        self.controller.update(cx, |c, cx| {
            c.send_prompt(
                crate::api::model::PromptInput {
                    text,
                    ..Default::default()
                },
                cx,
            )
        });
    }

    fn header(&self, pr: Option<&PullRequest>, loading: bool, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let title: AnyElement = match pr {
            Some(pr) => h_flex()
                .min_w_0()
                .gap_1p5()
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(format!("#{}", pr.number)),
                )
                .child(div().min_w_0().truncate().child(pr.title.clone()))
                .into_any_element(),
            None => div().child("Pull request").into_any_element(),
        };
        let url = pr.map(|p| p.url.clone());
        h_flex()
            .h_10()
            .flex_none()
            .px_3()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .child(div().flex_1().min_w_0().child(title))
            .when(loading, |el| {
                el.child(super::kit::spinner().xsmall().color(theme.muted_foreground))
            })
            .when_some(url, |el, url| {
                el.child(
                    Button::new("pr-open")
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .tooltip("Open on GitHub")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .child(
                Button::new("pr-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .xsmall()
                    .tooltip("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn status_line(&self, pr: &PullRequest, theme: &Theme) -> Div {
        let (state, color) = match (pr.state.as_str(), pr.draft) {
            ("MERGED", _) => ("Merged", theme.primary),
            ("CLOSED", _) => ("Closed", theme.danger),
            (_, true) => ("Draft", theme.muted_foreground),
            _ => ("Open", theme.success),
        };
        let review = match pr.review_decision.as_str() {
            "APPROVED" => Some(("Approved", theme.success)),
            "CHANGES_REQUESTED" => Some(("Changes requested", theme.danger)),
            "REVIEW_REQUIRED" => Some(("Review required", theme.warning)),
            _ => None,
        };
        let pill = |text: &str, color: Hsla| {
            div()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .bg(color.opacity(0.12))
                .child(text.to_string())
        };
        h_flex()
            .gap_1p5()
            .child(pill(state, color))
            .when_some(review, |el, (t, c)| el.child(pill(t, c)))
    }

    fn checks(&self, pr: &PullRequest, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut checks = pr.checks.clone();
        let rank = |s: &CheckState| match s {
            CheckState::Fail => 0,
            CheckState::Pending => 1,
            CheckState::Pass => 2,
            CheckState::Skipped => 3,
        };
        checks.sort_by_key(|c| rank(&c.state));
        let (fail, pending, pass) = (
            pr.count(CheckState::Fail),
            pr.count(CheckState::Pending),
            pr.count(CheckState::Pass),
        );
        let mut summary = Vec::new();
        if fail > 0 {
            summary.push(format!("{fail} failing"));
        }
        if pending > 0 {
            summary.push(format!("{pending} running"));
        }
        if pass > 0 {
            summary.push(format!("{pass} passed"));
        }
        let mut list = v_flex().rounded_lg().border_1().border_color(theme.border);
        let logs = &self.store.read(cx).check_logs;
        for (i, check) in checks.iter().enumerate() {
            let failed = check.state == CheckState::Fail;
            let open = failed && self.open_logs.contains(&check.link);
            let icon: Icon = match check.state {
                CheckState::Pass => icons::circle_check().text_color(theme.success),
                CheckState::Fail => icons::circle_x().text_color(theme.danger),
                CheckState::Pending => icons::circle_dashed().text_color(theme.warning),
                CheckState::Skipped => icons::circle_dashed().text_color(theme.muted_foreground),
            };
            let row_check = check.clone();
            let send_check = check.clone();
            let send_pr = pr.clone();
            let link = check.link.clone();
            let mut row = v_flex()
                .when(i > 0, |el| el.border_t_1().border_color(theme.border))
                .child(
                    h_flex()
                        .id(SharedString::from(format!("check-{i}")))
                        .px_3()
                        .py_2()
                        .gap_2()
                        .text_sm()
                        .when(failed, |el| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(theme.secondary.opacity(0.5)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_log(&row_check, cx)
                                }))
                        })
                        .child(icon.small())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(check.name.clone()))
                                .when(!check.workflow.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(check.workflow.clone()),
                                    )
                                }),
                        )
                        .when_some(check.duration(), |el, d| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(d),
                            )
                        })
                        .when(failed, |el| {
                            el.child(
                                Button::new(SharedString::from(format!("check-send-{i}")))
                                    .label("Send to agent")
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.send_failures(&send_pr, vec![send_check.clone()], cx)
                                    })),
                            )
                        })
                        .when(!link.is_empty(), |el| {
                            el.child(
                                Button::new(SharedString::from(format!("check-open-{i}")))
                                    .icon(IconName::ExternalLink)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Open on GitHub")
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        cx.open_url(&link)
                                    }),
                            )
                        }),
                );
            if open {
                let body: AnyElement = match logs.get(&check.link) {
                    None | Some(CheckLog::Loading) => h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(super::kit::spinner().xsmall())
                        .child("Loading log…")
                        .into_any_element(),
                    Some(CheckLog::Failed(e)) => div()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(e.clone())
                        .into_any_element(),
                    Some(CheckLog::Ready(log)) => div()
                        .id(SharedString::from(format!("check-log-{i}")))
                        .max_h(px(260.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_md()
                        .bg(theme.secondary.opacity(0.6))
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .children(
                            log.lines()
                                .map(|l| div().whitespace_normal().child(l.to_string())),
                        )
                        .into_any_element(),
                };
                row = row.child(div().px_3().pb_3().child(body));
            }
            list = list.child(row);
        }
        let failing: Vec<Check> = checks
            .iter()
            .filter(|c| c.state == CheckState::Fail)
            .cloned()
            .collect();
        let pr2 = pr.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(section_label("Checks", cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(summary.join(" · ")),
                    ),
            )
            .child(if checks.is_empty() {
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No checks reported.")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .when(failing.len() > 1, |el| {
                el.child(
                    h_flex().justify_end().child(
                        Button::new("checks-send-all")
                            .label(format!("Send {} failures to agent", failing.len()))
                            .primary()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_failures(&pr2, failing.clone(), cx)
                            })),
                    ),
                )
            })
            .into_any_element()
    }

    fn comments(&self, pr: &PullRequest, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let total = pr.threads.len() + pr.comments.len();
        let chosen = pr
            .threads
            .iter()
            .filter(|t| !self.excluded.contains(&thread_key(t)))
            .count()
            + pr.comments
                .iter()
                .filter(|c| !self.excluded.contains(&c.url))
                .count();
        let mut col = v_flex().gap_2().child(
            h_flex()
                .justify_between()
                .child(section_label("Review comments", cx))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} open", pr.threads.len())),
                ),
        );
        if total == 0 {
            return col
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("No open review comments."),
                )
                .into_any_element();
        }
        let mut list = v_flex().rounded_lg().border_1().border_color(theme.border);
        let mut i = 0;
        for t in &pr.threads {
            let key = thread_key(t);
            let at = match t.line {
                Some(l) => format!("{}:{l}", t.path),
                None => t.path.clone(),
            };
            let hunk: Vec<&str> = t.diff_hunk.lines().collect();
            let hunk = hunk[hunk.len().saturating_sub(4)..].join("\n");
            let body = v_flex()
                .gap_1p5()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .truncate()
                                .child(at),
                        )
                        .when(t.outdated, |el| {
                            el.child(div().text_xs().text_color(theme.warning).child("outdated"))
                        }),
                )
                .when(!hunk.is_empty(), |el| {
                    el.child(
                        div()
                            .p_1p5()
                            .rounded_md()
                            .bg(theme.secondary.opacity(0.6))
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(hunk.lines().map(|l| div().truncate().child(l.to_string()))),
                    )
                })
                .children(t.comments.iter().map(|c| comment_line(c, &theme)));
            list = list.child(selectable_row(i, key, body, &self.excluded, cx));
            i += 1;
        }
        for c in &pr.comments {
            let body = v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Conversation"),
                )
                .child(comment_line(c, &theme));
            list = list.child(selectable_row(i, c.url.clone(), body, &self.excluded, cx));
            i += 1;
        }
        let pr2 = pr.clone();
        col = col.child(list).child(
            h_flex().justify_end().child(
                Button::new("comments-send")
                    .label(match chosen {
                        1 => "Send 1 comment to agent".to_string(),
                        n => format!("Send {n} comments to agent"),
                    })
                    .primary()
                    .small()
                    .disabled(chosen == 0)
                    .on_click(cx.listener(move |this, _, _, cx| this.send_comments(&pr2, cx))),
            ),
        );
        col.into_any_element()
    }
}

fn comment_line(c: &Comment, theme: &Theme) -> Div {
    v_flex()
        .gap_0p5()
        .text_sm()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .child(format!("@{}", c.author)),
        )
        .child(div().whitespace_normal().child(c.body.trim().to_string()))
        .text_color(theme.foreground)
}

/// A row with a checkbox that includes it in "Send to agent".
fn selectable_row(
    i: usize,
    key: String,
    body: impl IntoElement,
    excluded: &HashSet<String>,
    cx: &mut Context<PullRequestPanel>,
) -> Stateful<Div> {
    let theme = cx.theme();
    let checked = !excluded.contains(&key);
    h_flex()
        .id(SharedString::from(format!("pr-comment-{i}")))
        .items_start()
        .px_3()
        .py_2()
        .gap_2()
        .when(i > 0, |el| el.border_t_1().border_color(theme.border))
        .child(
            div().pt_0p5().child(
                Checkbox::new(SharedString::from(format!("pr-comment-check-{i}")))
                    .checked(checked)
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        if *on {
                            this.excluded.remove(&key);
                        } else {
                            this.excluded.insert(key.clone());
                        }
                        cx.notify();
                    })),
            ),
        )
        .child(div().flex_1().min_w_0().child(body))
}

impl Render for PullRequestPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let dir = store.target_folder();
        let entry = dir.as_ref().and_then(|d| store.pull_requests.get(d));
        let loading = entry.is_some_and(|e| e.loading);
        let error = entry.and_then(|e| e.error.clone());
        let pr = entry.and_then(|e| e.pr.clone());
        let branch = dir
            .as_ref()
            .and_then(|d| store.vcs.get(d))
            .and_then(|v| v.branch.clone());

        let body: AnyElement = match &pr {
            Some(Some(pr)) => v_flex()
                .p_3()
                .gap_5()
                .child(self.status_line(pr, &theme))
                .child(self.checks(pr, cx))
                .child(self.comments(pr, cx))
                .into_any_element(),
            Some(None) => empty(
                "No pull request",
                &match branch {
                    Some(b) => format!("Nothing open for the branch {b}."),
                    None => "Nothing open for this branch.".into(),
                },
                &theme,
            ),
            None if error.is_none() => empty("Loading pull request…", "", &theme),
            None => div().into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(self.header(pr.as_ref().and_then(|p| p.as_ref()), loading, cx))
            .when_some(error, |el, e| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(theme.danger)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(e),
                )
            })
            .child(
                div()
                    .id("pr-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

fn empty(title: &str, sub: &str, theme: &Theme) -> AnyElement {
    v_flex()
        .p_6()
        .gap_1()
        .items_center()
        .text_center()
        .child(div().text_sm().child(title.to_string()))
        .when(!sub.is_empty(), |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(sub.to_string()),
            )
        })
        .into_any_element()
}

/// Header chip next to the branch: "#15" with a dot for the checks' state.
pub fn chip(store: &Store, cx: &App) -> Option<(String, Hsla, String)> {
    if !store.gh_available {
        return None;
    }
    let dir = store.target_folder()?;
    let pr = store.pull_requests.get(&dir)?.pr.as_ref()?.as_ref()?;
    let theme = cx.theme();
    let (fail, pending) = (pr.count(CheckState::Fail), pr.count(CheckState::Pending));
    let color = if fail > 0 {
        theme.danger
    } else if pending > 0 {
        theme.warning
    } else if pr.checks.is_empty() {
        theme.muted_foreground
    } else {
        theme.success
    };
    let mut tip = vec![format!("PR #{}: {}", pr.number, pr.title)];
    if fail > 0 {
        tip.push(format!("{fail} failing"));
    }
    if pending > 0 {
        tip.push(format!("{pending} running"));
    }
    if !pr.threads.is_empty() {
        tip.push(format!("{} open review threads", pr.threads.len()));
    }
    Some((format!("#{}", pr.number), color, tip.join(" · ")))
}
