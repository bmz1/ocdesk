//! Explicit permission replies. No action is selected or submitted automatically.
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::model::{FileDiff, PermissionDecision};
use crate::controller::Controller;
use crate::store::{Connection, Store};

pub fn render_permissions(store: &Store, controller: Entity<Controller>, cx: &App) -> AnyElement {
    let Some(sid) = store.selected.as_ref() else {
        return div().into_any_element();
    };
    let theme = cx.theme();
    let mut col = v_flex().w_full().gap_2();
    for request in store.permissions.get(sid).into_iter().flatten() {
        let key = (sid.clone(), request.id.clone());
        let sending = store.permission_sending.contains(&key);
        let disabled = sending || store.connection() != Connection::Connected;
        let mut card = v_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.warning)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Permission needed: {}", request.action)),
            );
        if let Some(message) = &request.message {
            card = card.child(message.clone());
        }
        let mut preview = v_flex()
            .w_full()
            .gap_2()
            .child(super::tool_card::text_panel(
                &format!("permission-{}", request.id),
                "Resources",
                &request.resources.join("\n"),
                cx,
            ));
        // Render server-provided edit previews as patches, rather than escaped JSON.
        let files: Vec<_> = request
            .metadata
            .get("files")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(FileDiff::from_json)
            .collect();
        if files.is_empty() {
            if !request.metadata.is_null() && request.metadata != serde_json::json!({}) {
                preview = preview.child(super::tool_card::text_panel(
                    &format!("permission-meta-{}", request.id),
                    "Details",
                    &serde_json::to_string_pretty(&request.metadata).unwrap_or_default(),
                    cx,
                ));
            }
        } else {
            for (i, file) in files.iter().enumerate() {
                preview = preview
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(file.file.clone()),
                    )
                    .child(super::changes::patch_view(
                        &format!("permission-diff-{}-{i}", request.id),
                        &file.patch,
                        cx,
                    ));
            }
        }
        // Keep approval buttons visible when an edit preview is large.
        card = card.child(
            div()
                .id(SharedString::from(format!(
                    "permission-preview-{}",
                    request.id
                )))
                .max_h(px(180.))
                .overflow_y_scroll()
                .child(preview),
        );
        let mut actions = h_flex().gap_2();
        for (label, decision) in [
            ("Allow once", PermissionDecision::Once),
            ("Always allow", PermissionDecision::Always),
            ("Reject", PermissionDecision::Reject),
        ] {
            if decision == PermissionDecision::Always && request.save.is_empty() {
                continue;
            }
            let (controller, sid, rid) = (controller.clone(), sid.clone(), request.id.clone());
            actions = actions.child(
                Button::new(SharedString::from(format!("{}-{rid}", decision.as_str())))
                    .label(label)
                    .small()
                    .disabled(disabled)
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |c, cx| {
                            c.reply_permission(sid.clone(), rid.clone(), decision, cx)
                        })
                    }),
            );
        }
        if !request.save.is_empty() {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Always allow saves these rules: {}",
                        request.save.join(", ")
                    )),
            );
        }
        card = card.child(actions);
        if sending {
            card = card.child("Sending reply…");
        } else if disabled {
            card = card.child("Waiting for opencode to reconnect…");
        }
        if let Some(error) = store.permission_errors.get(&key) {
            card = card.child(div().text_color(theme.danger).child(error.clone()));
        }
        col = col.child(card);
    }
    div()
        .id("permission-queue")
        .max_h(px(340.))
        .overflow_y_scroll()
        .child(col)
        .into_any_element()
}
