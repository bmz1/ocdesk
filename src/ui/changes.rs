//! Read-only latest-turn diffs from opencode, never inferred from tool arguments.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::features::{ReviewMode, patch_fingerprint};
use crate::controller::Controller;
use crate::store::Store;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};

pub fn render_changes(store: &Store, controller: Entity<Controller>, cx: &App) -> AnyElement {
    let Some(sid) = store.selected.clone() else {
        return div().into_any_element();
    };
    let theme = cx.theme();
    let mode = store.review.get(&sid).cloned().unwrap_or_default();
    let loading = store.diff_loading.contains(&sid);
    let refresh_controller = controller.clone();
    let refresh_sid = sid.clone();
    let mut col = v_flex().w_full().gap_3().child(
        h_flex()
            .gap_2()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(mode.label().to_string()),
            )
            .child(
                Button::new("refresh-diff")
                    .label(if loading { "Loading…" } else { "Refresh" })
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        refresh_controller
                            .update(cx, |c, cx| c.refresh_diff(refresh_sid.clone(), cx))
                    }),
            ),
    );
    let controller_scope = controller.clone();
    col = col.child(
        h_flex()
            .gap_2()
            .child(
                Button::new("review-scope")
                    .label(mode.label().to_string())
                    .outline()
                    .small()
                    .dropdown_menu(move |mut menu, _, _| {
                        for mode in [
                            ReviewMode::Turn,
                            ReviewMode::Working,
                            ReviewMode::Branch,
                            ReviewMode::Committed,
                        ] {
                            let c = controller_scope.clone();
                            menu =
                                menu.item(PopupMenuItem::new(mode.label().to_string()).on_click(
                                    move |_, _, cx| {
                                        c.update(cx, |c, cx| c.set_review(mode.clone(), cx))
                                    },
                                ));
                        }
                        menu
                    }),
            )
            .child(
                Button::new("load-commits")
                    .label("Load recent commits")
                    .ghost()
                    .small()
                    .on_click({
                        let c = controller.clone();
                        move |_, _, cx| c.update(cx, |c, cx| c.load_commits(cx))
                    }),
            )
            .child(
                Button::new("diff-settings")
                    .label("Settings")
                    .ghost()
                    .small()
                    .on_click({
                        let c = controller.clone();
                        move |_, window, cx| crate::ui::settings::open(c.clone(), window, cx)
                    }),
            ),
    );
    if matches!(mode, ReviewMode::Branch | ReviewMode::Committed) {
        let current = store.review_base.get(&sid).cloned().unwrap_or_default();
        let label = if current.is_empty() {
            "Compare against: repository default".into()
        } else {
            format!("Compare against: {current}")
        };
        let c = controller.clone();
        let id = sid.clone();
        col = col.child(
            Button::new("review-base")
                .label(label)
                .outline()
                .small()
                .on_click(move |_, window, cx| {
                    open_base(c.clone(), id.clone(), current.clone(), window, cx)
                }),
        );
    }
    if let Some(commits) = store.commits.get(&sid) {
        let commits = commits.clone();
        let c = controller.clone();
        let label = if let ReviewMode::Commit(sha) = &mode {
            commits
                .iter()
                .find(|c| &c.sha == sha)
                .map(|c| format!("{} {}", &c.sha[..8], c.subject))
                .unwrap_or_else(|| sha.clone())
        } else {
            "Choose a commit…".into()
        };
        col = col.child(
            Button::new("review-commit")
                .label(label)
                .outline()
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    for commit in &commits {
                        let c = c.clone();
                        let sha = commit.sha.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format!("{} {}", &sha[..8], commit.subject))
                                .on_click(move |_, _, cx| {
                                    c.update(cx, |c, cx| {
                                        c.set_review(ReviewMode::Commit(sha.clone()), cx)
                                    })
                                }),
                        );
                    }
                    menu
                }),
        );
    }
    let sid = store.selected.as_ref().unwrap();
    if let Some(error) = store.diff_errors.get(sid) {
        col = col.child(div().text_color(theme.danger).child(error.clone()));
    }
    if let Some(files) = store.diffs.get(sid) {
        if files.is_empty() && !loading {
            col = col.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("No file changes in this scope."),
            );
        }
        for (i, file) in files.iter().enumerate() {
            let key = controller.read(cx).viewed_key(sid, &file.file, cx);
            let viewed =
                store.preferences.viewed.get(&key) == Some(&patch_fingerprint(&file.patch));
            let mark_c = controller.clone();
            let mark_sid = sid.clone();
            let mark_file = file.file.clone();
            let patch = file.patch.clone();
            let open_c = controller.clone();
            let open_file = file.file.clone();
            col = col.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("viewed-{i}")))
                                    .label(if viewed { "✓ Viewed" } else { "Mark viewed" })
                                    .ghost()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        mark_c.update(cx, |c, cx| {
                                            c.mark_viewed(
                                                mark_sid.clone(),
                                                mark_file.clone(),
                                                patch.clone(),
                                                cx,
                                            )
                                        })
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!("open-file-{i}")))
                                    .label("Open in editor")
                                    .ghost()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        open_c
                                            .update(cx, |c, cx| c.open_file(open_file.clone(), cx))
                                    }),
                            )
                            .child(div().flex_1().min_w_0().child(file.file.clone()))
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(file.status.clone()),
                            )
                            .child(
                                div()
                                    .text_color(theme.success)
                                    .child(format!("+{}", file.additions)),
                            )
                            .child(
                                div()
                                    .text_color(theme.danger)
                                    .child(format!("−{}", file.deletions)),
                            ),
                    )
                    .child(patch_view(&format!("diff-{sid}-{i}"), &file.patch, cx))
                    .when(file.more, |el| {
                        el.child(
                            div()
                                .text_color(theme.warning)
                                .child("Patch truncated by opencode."),
                        )
                    })
                    .when(file.patch.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child("No textual patch available."),
                        )
                    }),
            );
        }
    }
    col.into_any_element()
}

pub fn patch_view(id: &str, patch: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let copy = patch.to_string();
    let mut lines = v_flex()
        .w_full()
        .font_family(theme.mono_font_family.clone())
        .text_xs();
    for line in patch.lines().take(500) {
        let color = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@")
        {
            theme.muted_foreground
        } else if line.starts_with('+') {
            theme.success
        } else if line.starts_with('-') {
            theme.danger
        } else {
            theme.foreground
        };
        // A line can itself be enormous (minified files).
        let preview: String = line.chars().take(2000).collect();
        lines = lines.child(div().flex_none().text_color(color).child(preview));
    }
    v_flex()
        .w_full()
        .gap_1()
        .child(
            Button::new(SharedString::from(format!("copy-{id}")))
                .label("Copy patch")
                .ghost()
                .xsmall()
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                }),
        )
        .child(
            div()
                .id(SharedString::from(id.to_string()))
                .max_h(px(400.))
                .overflow_y_scroll()
                .child(lines),
        )
        .child(div().text_color(theme.muted_foreground).text_xs().child(
            "Preview: up to 500 lines, 2,000 characters per line. Copy includes the full patch.",
        ))
        .into_any_element()
}

fn open_base(
    controller: Entity<Controller>,
    sid: String,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::{
        WindowExt as _,
        input::{Input, InputState},
    };
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(current)
            .placeholder("origin/main, master, or commit; empty = repository default")
    });
    window.open_alert_dialog(cx, move |alert, _, _| {
        let c = controller.clone();
        let id = sid.clone();
        let i = input.clone();
        alert
            .title("Comparison base")
            .description(Input::new(&input))
            .confirm()
            .ok_text("Compare")
            .on_ok(move |_, _, cx| {
                let base = i.read(cx).value().to_string();
                c.update(cx, |c, cx| c.set_review_base(id.clone(), base, cx));
                true
            })
    });
}
