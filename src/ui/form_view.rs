//! Blocking question forms (opencode `form.*`), shown above the composer.
//! Option rows follow the mock: indicator, label + description, letter badge;
//! `custom: true` fields get a free-text row besides their options.
use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::{FormField, FormFieldKind, FormRequest};
use crate::controller::Controller;
use crate::store::{Connection, Store};

/// Free-text inputs, keyed `(form id, field key)`. Owned by
/// `ConversationView` (they need a window to be created); this view only
/// renders them.
pub type FormInputs = HashMap<(String, String), Entity<InputState>>;

/// Whether a form's field should carry a free-text input: optionless text,
/// number, or integer fields, or the extra `custom` row.
pub fn field_needs_input(f: &FormField) -> bool {
    (f.options.is_empty()
        && matches!(
            f.kind,
            FormFieldKind::String | FormFieldKind::Number | FormFieldKind::Integer
        ))
        || f.custom
}

pub fn render_forms(
    store: &Store,
    inputs: &FormInputs,
    controller: Entity<Controller>,
    cx: &App,
) -> AnyElement {
    let Some(sid) = store.selected.clone() else {
        return div().into_any_element();
    };
    let forms = store.forms.of_session(&sid).to_vec();
    if forms.is_empty() {
        return div().into_any_element();
    }
    let theme = cx.theme();
    let mut col = v_flex().w_full().gap_2();
    let total = forms.len();
    for (i, form) in forms.iter().enumerate() {
        col = col.child(render_card(
            store,
            &sid,
            form,
            i + 1,
            total,
            inputs,
            controller.clone(),
            cx,
        ));
    }
    div()
        .id("form-queue")
        .max_h(px(420.))
        .overflow_y_scroll()
        .border_1()
        .border_color(theme.warning)
        .rounded_lg()
        .p_3()
        .child(col)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_card(
    store: &Store,
    sid: &str,
    form: &FormRequest,
    index: usize,
    total: usize,
    inputs: &FormInputs,
    controller: Entity<Controller>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let draft = store.forms.draft(&form.id);
    let sending = store.forms.is_sending(sid, &form.id);
    let connected = store.connection() == Connection::Connected;
    let disabled = sending || !connected;
    let ready = form
        .fields
        .iter()
        .filter(|f| !f.hidden && f.required)
        .all(|f| answered(draft.get(&f.key), f));

    let mut card = v_flex().w_full().flex_none().gap_3();
    card = card.child(
        h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(form.title.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Question {index} of {total}")),
            ),
    );

    for field in form.fields.iter().filter(|f| !f.hidden) {
        card = card.child(render_field(
            sid,
            form,
            field,
            &draft,
            inputs,
            controller.clone(),
            cx,
        ));
    }

    let error = store.forms.reply_error(sid, &form.id).map(str::to_owned);
    let (controller2, sid2, fid2) = (controller.clone(), sid.to_string(), form.id.clone());
    let cancel: Entity<Controller> = controller;
    let (sid3, fid3) = (sid.to_string(), form.id.clone());
    card = card.child(
        h_flex()
            .justify_between()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("form-cancel-{}", form.id)))
                    .label("Dismiss")
                    .ghost()
                    .small()
                    // Like replies: a dismissal sent while reconnecting could
                    // never report back, leaving the form stuck "sending".
                    .disabled(disabled)
                    .on_click(move |_, _, cx| {
                        cancel.update(cx, |c, cx| c.cancel_form(sid3.clone(), fid3.clone(), cx))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when_some(error, |el, e| {
                        el.child(div().text_color(theme.danger).text_sm().child(e))
                    })
                    .child(if sending {
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Sending…")
                    } else {
                        div()
                    })
                    .child(
                        Button::new(SharedString::from(format!("form-next-{}", form.id)))
                            .label("Next")
                            .small()
                            .primary()
                            .disabled(disabled || !ready)
                            .on_click(move |_, _, cx| {
                                controller2.update(cx, |c, cx| {
                                    let draft = c.form_draft(fid2.clone(), cx);
                                    c.reply_form(sid2.clone(), fid2.clone(), draft, cx)
                                })
                            }),
                    ),
            ),
    );
    if !connected {
        card = card.child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Waiting for opencode to reconnect…"),
        );
    }
    card.into_any_element()
}

/// A required field is answered when its draft holds something usable.
fn answered(v: Option<&serde_json::Value>, f: &FormField) -> bool {
    let Some(v) = v else { return !f.required };
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => {
            if f.required {
                !s.is_empty()
            } else {
                true
            }
        }
        serde_json::Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

fn render_field(
    sid: &str,
    form: &FormRequest,
    field: &FormField,
    draft: &serde_json::Value,
    inputs: &FormInputs,
    controller: Entity<Controller>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let mut col = v_flex().w_full().gap_1();
    if !field.title.is_empty() {
        col = col.child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(field.title.clone()),
        );
    }
    if let Some(d) = &field.description {
        col = col.child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(d.clone()),
        );
    }
    let answer = draft.get(&field.key);
    match field.kind {
        FormFieldKind::Boolean => {
            for (i, (label, value)) in [("Yes", true), ("No", false)].into_iter().enumerate() {
                let selected = answer == Some(&serde_json::Value::Bool(value));
                col = col.child(option_row(
                    sid,
                    form,
                    field,
                    OptionRow::Bool(value),
                    label,
                    None,
                    (b'A' + i as u8) as char,
                    selected,
                    controller.clone(),
                    cx,
                ));
            }
        }
        FormFieldKind::External => {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Follow the link shown in your terminal to continue."),
            );
        }
        kind => {
            for (i, option) in field.options.iter().enumerate() {
                let selected = match kind {
                    FormFieldKind::MultiSelect => answer
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|a| {
                            a.iter().any(|v| v.as_str() == Some(option.value.as_str()))
                        }),
                    _ => answer == Some(&serde_json::Value::String(option.value.clone())),
                };
                col = col.child(option_row(
                    sid,
                    form,
                    field,
                    OptionRow::Option(option.value.clone()),
                    &option.label,
                    option.description.as_deref(),
                    (b'A' + i as u8) as char,
                    selected,
                    controller.clone(),
                    cx,
                ));
            }
        }
    }
    // The extra free-text row of `custom` fields, or the input of a
    // plain text/number field.
    if field_needs_input(field) {
        let k = (form.id.clone(), field.key.clone());
        let placeholder = field.placeholder.clone().unwrap_or_else(|| {
            if field.custom {
                "Or describe in your own words…".into()
            } else {
                "Your answer…".into()
            }
        });
        if let Some(input) = inputs.get(&k) {
            col = col.child(
                div()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .px_2()
                    .py_1()
                    .child(Input::new(input)),
            );
        } else {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(placeholder),
            );
        }
    }
    col.into_any_element()
}

/// What clicking one row sets as the field's answer.
#[derive(Clone)]
enum OptionRow {
    /// A single-choice option value.
    Option(String),
    /// The boolean Yes/No row.
    Bool(bool),
}

#[allow(clippy::too_many_arguments)]
fn option_row(
    sid: &str,
    form: &FormRequest,
    field: &FormField,
    row: OptionRow,
    label: &str,
    description: Option<&str>,
    letter: char,
    selected: bool,
    controller: Entity<Controller>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (fid, key) = (form.id.clone(), field.key.clone());
    let (controller, _sid) = (controller, sid.to_string());
    let multi = field.kind == FormFieldKind::MultiSelect;
    let border = if selected {
        theme.primary
    } else {
        theme.border
    };
    let bg = if selected {
        theme.selection
    } else {
        theme.background
    };
    div()
        .id(SharedString::from(format!(
            "form-{}-{}",
            form.id, field.key
        )))
        .w_full()
        .flex_none()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(bg)
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .hover(|el| el.border_color(theme.primary))
        .on_click(move |_, _, cx| {
            let value = match &row {
                OptionRow::Option(v) => serde_json::Value::String(v.clone()),
                OptionRow::Bool(b) => serde_json::Value::Bool(*b),
            };
            controller.update(cx, |c, cx| {
                c.set_form_answer(
                    fid.clone(),
                    key.clone(),
                    if multi {
                        // Toggle inside the array; an already-selected row
                        // unselects on a second click.
                        let current = c
                            .form_draft(fid.clone(), cx)
                            .get(&key)
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let pos = current
                            .iter()
                            .position(|v| v.as_str() == Some(value.as_str().unwrap_or("")));
                        let mut current = current;
                        match pos {
                            Some(i) => {
                                current.remove(i);
                            }
                            None => current.push(value),
                        }
                        serde_json::Value::Array(current)
                    } else {
                        value
                    },
                    cx,
                );
            });
        })
        .child(
            // indicator: circle for single choice, square for multiselect
            if multi {
                div()
                    .size_4()
                    .flex_none()
                    .rounded_xs()
                    .border_1()
                    .border_color(border)
                    .bg(if selected {
                        theme.primary
                    } else {
                        theme.background
                    })
                    .when(selected, |el| {
                        el.child(
                            div()
                                .size_2()
                                .m_auto()
                                .rounded_xs()
                                .bg(theme.primary_foreground),
                        )
                    })
            } else {
                div()
                    .size_4()
                    .flex_none()
                    .rounded_full()
                    .border_1()
                    .border_color(border)
                    .bg(theme.background)
                    .when(selected, |el| {
                        el.child(div().size_2().m_auto().rounded_full().bg(theme.primary))
                    })
            },
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(if selected {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .child(label.to_string()),
                )
                .when_some(description, |el, d| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(d.to_string()),
                    )
                }),
        )
        .child(
            div()
                .size_6()
                .flex_none()
                .rounded_md()
                .border_1()
                .border_color(if selected {
                    theme.primary
                } else {
                    theme.border
                })
                .text_color(if selected {
                    theme.primary
                } else {
                    theme.muted_foreground
                })
                .font_weight(FontWeight::SEMIBOLD)
                .text_xs()
                .flex()
                .items_center()
                .justify_center()
                .child(letter.to_string()),
        )
        .into_any_element()
}
