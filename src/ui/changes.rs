//! Read-only latest-turn diffs from opencode, never inferred from tool arguments.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::Store;

pub fn render_changes(store: &Store, controller: Entity<Controller>, cx: &App) -> AnyElement {
    let Some(sid) = store.selected.clone() else {
        return div().into_any_element();
    };
    let theme = cx.theme();
    let loading = store.diff_loading.contains(&sid);
    let mut col = v_flex().w_full().gap_3().child(
        h_flex()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Changes in latest turn"),
            )
            .child(
                Button::new("refresh-diff")
                    .label(if loading { "Loading…" } else { "Refresh" })
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |c, cx| c.refresh_diff(sid.clone(), cx))
                    }),
            ),
    );
    let sid = store.selected.as_ref().unwrap();
    if let Some(error) = store.diff_errors.get(sid) {
        col = col.child(div().text_color(theme.danger).child(error.clone()));
    }
    if let Some(files) = store.diffs.get(sid) {
        if files.is_empty() && !loading {
            col = col.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("No file changes in the latest turn."),
            );
        }
        for (i, file) in files.iter().enumerate() {
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
