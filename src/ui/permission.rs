//! Approval requests, shown in the composer's place above the prompt box.
//! Nothing is answered automatically. With the prompt box empty, Enter
//! allows once and Esc denies (see `Composer`); typing a reply and pressing
//! Enter denies with that text as feedback to the agent.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{Block, FileDiff, Message, PermissionDecision, PermissionRequest};
use crate::controller::Controller;
use crate::store::{Connection, Store};

/// Preview area height before it scrolls; the buttons stay visible.
const PREVIEW_MAX_H: f32 = 320.;

/// Requests the selected session is waiting on: its own, then its
/// subagents' (labelled with the subagent's agent), oldest first.
pub fn pending<'a>(store: &'a Store, sid: &str) -> Vec<(&'a PermissionRequest, Option<String>)> {
    let mut out: Vec<(&PermissionRequest, Option<String>)> = store
        .permissions
        .get(sid)
        .into_iter()
        .flatten()
        .map(|r| (r, None))
        .collect();
    for child in store.children_of(sid) {
        let label = child
            .agent
            .clone()
            .map(|a| format!("@{a}"))
            .unwrap_or_else(|| child.display_title());
        for r in store.permissions.get(&child.id).into_iter().flatten() {
            out.push((r, Some(label.clone())));
        }
    }
    out
}

/// The shell command a request is about: the asking tool call's input,
/// else the request metadata.
fn command_of(store: &Store, request: &PermissionRequest) -> Option<String> {
    let from_call = request.call_id.as_ref().and_then(|call| {
        let conv = store.conversations.get(&request.session_id)?;
        conv.messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => a.blocks.iter().find_map(|b| match b {
                Block::Tool(t) if &t.call_id == call => t
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                _ => None,
            }),
            _ => None,
        })
    });
    from_call.or_else(|| {
        request
            .metadata
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })
}

fn title(request: &PermissionRequest, has_command: bool) -> String {
    match request.action.as_str() {
        _ if has_command => "Run this command?".into(),
        "edit" | "write" | "patch" => "Make these edits?".into(),
        "read" => "Read these files?".into(),
        "webfetch" | "websearch" => "Access the web?".into(),
        "external_directory" => "Work outside the project folder?".into(),
        other => format!("Allow {other}?"),
    }
}

pub fn render_permissions(store: &Store, controller: Entity<Controller>, cx: &App) -> AnyElement {
    let Some(sid) = store.selected.as_ref() else {
        return div().into_any_element();
    };
    let theme = cx.theme();
    let requests = pending(store, sid);
    let mut col = v_flex().w_full().gap_2();
    for (n, (request, from)) in requests.iter().enumerate() {
        let request = *request;
        let key = (request.session_id.clone(), request.id.clone());
        let sending = store.permission_sending.contains(&key);
        let disabled = sending || store.connection() != Connection::Connected;
        let command = command_of(store, request);
        let mut card = v_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .p_3()
            .rounded_xl()
            .border_1()
            .border_color(theme.warning.opacity(0.7))
            .bg(theme.warning.opacity(0.06))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(theme.warning),
                    )
                    .when_some(from.clone(), |el, from| {
                        el.child(div().text_color(theme.muted_foreground).child(from))
                    })
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title(request, command.is_some())),
                    ),
            );
        if let Some(message) = &request.message {
            card = card.child(div().text_sm().child(message.clone()));
        }
        let mut preview = v_flex().w_full().gap_2();
        let files: Vec<_> = request
            .metadata
            .get("files")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(FileDiff::from_json)
            .collect();
        if let Some(cmd) = &command {
            preview = preview.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(div().text_color(theme.muted_foreground).child("$"))
                    .child(div().flex_1().min_w_0().child(cmd.clone())),
            );
        } else if !files.is_empty() {
            for (i, file) in files.iter().enumerate() {
                preview = preview
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(file.file.clone()),
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
                    .child(super::changes::patch_view_with(
                        &format!("permission-diff-{}-{i}", request.id),
                        &file.patch,
                        super::changes::PatchOpts {
                            language: Some(file.file.clone()),
                            ..Default::default()
                        },
                        cx,
                    ));
            }
        } else if !request.resources.is_empty() {
            preview = preview.child(super::tool_card::text_panel(
                &format!("permission-{}", request.id),
                "Resources",
                &request.resources.join("\n"),
                cx,
            ));
        }
        card = card.child(
            div()
                .id(SharedString::from(format!(
                    "permission-preview-{}",
                    request.id
                )))
                .max_h(px(PREVIEW_MAX_H))
                .overflow_y_scroll()
                .child(preview),
        );
        if !request.save.is_empty() {
            let wildcard = request
                .save
                .iter()
                .any(|r| r.trim() == "*" || r.trim_end().ends_with(" *") || r.contains("**"));
            card = card.child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .text_xs()
                    .text_color(if wildcard {
                        theme.warning
                    } else {
                        theme.muted_foreground
                    })
                    .child("Always allow saves:")
                    .children(request.save.iter().map(|r| {
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(theme.secondary)
                            .font_family(theme.mono_font_family.clone())
                            .child(r.clone())
                    }))
                    .when(wildcard, |el| {
                        el.child("· a wildcard: it matches more than this request")
                    }),
            );
        }
        let first = n == 0;
        let reply = |decision: PermissionDecision| {
            let (controller, sid, rid) = (
                controller.clone(),
                request.session_id.clone(),
                request.id.clone(),
            );
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                controller.update(cx, |c, cx| {
                    c.reply_permission(sid.clone(), rid.clone(), decision, None, cx)
                })
            }
        };
        let mut actions = h_flex().gap_2().items_center().child(
            Button::new(SharedString::from(format!("once-{}", request.id)))
                .label("Allow once")
                .small()
                .primary()
                .disabled(disabled)
                .when(first, |b| b.child(div().text_xs().opacity(0.6).child("↵")))
                .on_click(reply(PermissionDecision::Once)),
        );
        if !request.save.is_empty() {
            actions = actions.child(
                Button::new(SharedString::from(format!("always-{}", request.id)))
                    .label("Always allow")
                    .small()
                    .outline()
                    .disabled(disabled)
                    .on_click(reply(PermissionDecision::Always)),
            );
        }
        actions = actions
            .child(
                Button::new(SharedString::from(format!("reject-{}", request.id)))
                    .label("Deny")
                    .small()
                    .ghost()
                    .disabled(disabled)
                    .when(first, |b| {
                        b.child(div().text_xs().opacity(0.6).child("Esc"))
                    })
                    .on_click(reply(PermissionDecision::Reject)),
            )
            .child(div().flex_1())
            .when(first, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Or type a reply below to deny with feedback"),
                )
            });
        card = card.child(actions);
        if sending {
            card = card.child(div().text_xs().child("Sending reply…"));
        } else if disabled {
            card = card.child(div().text_xs().child("Waiting for opencode to reconnect…"));
        }
        if let Some(error) = store.permission_errors.get(&key) {
            card = card.child(div().text_color(theme.danger).child(error.clone()));
        }
        col = col.child(card);
    }
    div()
        .id("permission-queue")
        .max_h(px(PREVIEW_MAX_H + 180.))
        .overflow_y_scroll()
        .child(col)
        .into_any_element()
}
