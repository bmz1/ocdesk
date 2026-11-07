//! Editable settings, with staged changes and explicit save/cancel.
use crate::{
    api::model::ModelRef,
    controller::Controller,
    settings::{PermissionMode, ProjectDefaults, Settings},
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use std::path::PathBuf;

struct DefaultsEditor {
    folder: Option<PathBuf>,
    model: Entity<InputState>,
    agent: Entity<InputState>,
    permission: Option<PermissionMode>,
}
pub struct SettingsView {
    store: Entity<crate::store::Store>,
    _sub: Subscription,
    controller: Entity<Controller>,
    prefs: Settings,
    editor: Entity<InputState>,
    defaults: Vec<DefaultsEditor>,
    keys: Vec<(String, &'static str, Entity<InputState>)>,
    error: Option<String>,
}
pub fn open(controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
    controller.update(cx, |c, cx| c.refresh_saved_grants(cx));
    let prefs = controller.read(cx).preferences();
    let view = cx.new(|cx| SettingsView::new(controller, prefs, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Settings").w(px(680.)).child(view.clone())
    });
}
fn input(
    value: String,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(value)
            .placeholder(placeholder.to_string())
    })
}
fn model_text(model: &Option<ModelRef>) -> String {
    model
        .as_ref()
        .map(|m| format!("{}/{}", m.provider_id, m.id))
        .unwrap_or_default()
}
fn parse_model(value: &str) -> anyhow::Result<Option<ModelRef>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let (provider, id) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Model must be provider/model"))?;
    if provider.is_empty() || id.is_empty() {
        anyhow::bail!("Model must be provider/model");
    }
    Ok(Some(ModelRef {
        provider_id: provider.into(),
        id: id.into(),
        variant: None,
    }))
}
impl SettingsView {
    fn new(
        controller: Entity<Controller>,
        prefs: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = input(
            prefs.editor.clone().unwrap_or_default(),
            "code (executable, no arguments)",
            window,
            cx,
        );
        let mut defaults = vec![DefaultsEditor {
            folder: None,
            model: input(model_text(&prefs.draft_model), "Server default", window, cx),
            agent: input(
                prefs.draft_agent.clone().unwrap_or_default(),
                "Server default",
                window,
                cx,
            ),
            permission: Some(prefs.permission_mode),
        }];
        for folder in &prefs.folders {
            let p = prefs
                .project_defaults
                .get(folder)
                .cloned()
                .unwrap_or_default();
            defaults.push(DefaultsEditor {
                folder: Some(folder.clone()),
                model: input(model_text(&p.model), "Inherit default model", window, cx),
                agent: input(
                    p.agent.unwrap_or_default(),
                    "Inherit default agent",
                    window,
                    cx,
                ),
                permission: p.permission,
            });
        }
        let keys = super::keys::editable()
            .into_iter()
            .map(|(name, default, label)| {
                let value = prefs.keybindings.get(&name).cloned().unwrap_or(default);
                (name, label, input(value, "Unbound", window, cx))
            })
            .collect();
        let store = controller.read(cx).view_store();
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            _sub: sub,
            controller,
            prefs,
            editor,
            defaults,
            keys,
            error: None,
        }
    }
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut prefs = self.prefs.clone();
        prefs.editor =
            Some(self.editor.read(cx).value().trim().to_string()).filter(|s| !s.is_empty());
        prefs.keybindings = self
            .keys
            .iter()
            .map(|(name, _, input)| (name.clone(), input.read(cx).value().trim().to_string()))
            .collect();
        if let Err(e) = super::keys::validate(&prefs.keybindings, cx) {
            self.error = Some(format!("{e:#}"));
            cx.notify();
            return;
        }
        for d in &self.defaults {
            let mut model = match parse_model(&d.model.read(cx).value()) {
                Ok(m) => m,
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    cx.notify();
                    return;
                }
            };
            let previous = d
                .folder
                .as_ref()
                .and_then(|f| {
                    self.prefs
                        .project_defaults
                        .get(f)
                        .and_then(|p| p.model.as_ref())
                })
                .or_else(|| {
                    if d.folder.is_none() {
                        self.prefs.draft_model.as_ref()
                    } else {
                        None
                    }
                });
            if let (Some(model), Some(previous)) = (&mut model, previous)
                && model.provider_id == previous.provider_id
                && model.id == previous.id
            {
                model.variant = previous.variant.clone();
            }
            let agent = Some(d.agent.read(cx).value().trim().to_string()).filter(|s| !s.is_empty());
            if let Some(folder) = &d.folder {
                prefs.project_defaults.insert(
                    folder.clone(),
                    ProjectDefaults {
                        model,
                        agent,
                        permission: d.permission,
                    },
                );
            } else {
                prefs.draft_model = model;
                prefs.draft_agent = agent;
                prefs.permission_mode = d.permission.unwrap_or_default();
            }
        }
        if let Err(e) = super::keys::apply(&prefs.keybindings, cx) {
            self.error = Some(format!("{e:#}"));
            cx.notify();
            return;
        }
        self.controller
            .update(cx, |c, cx| c.save_preferences(prefs, cx));
        window.close_dialog(cx);
    }
}
impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let weak = cx.weak_entity();
        let mut col = v_flex().gap_3().child(
            h_flex().gap_2().child(div().flex_1().child("Theme")).child(
                Button::new("settings-theme")
                    .label(self.prefs.theme.clone().unwrap_or_else(|| "dark".into()))
                    .outline()
                    .small()
                    .dropdown_menu(move |mut menu, _, _| {
                        for mode in ["dark", "light", "system"] {
                            let weak = weak.clone();
                            menu = menu.item(PopupMenuItem::new(mode).on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.prefs.theme = Some(mode.into());
                                    cx.notify();
                                });
                            }));
                        }
                        menu
                    }),
            ),
        );
        col = col.child(
            Button::new("settings-notifications")
                .label(if self.prefs.notifications {
                    "Desktop notifications: on"
                } else {
                    "Desktop notifications: off"
                })
                .outline()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.prefs.notifications = !this.prefs.notifications;
                    cx.notify();
                })),
        );
        col=col.child(div().text_xs().text_color(theme.muted_foreground).child("Unread indicators work independently. Notifications use your operating system’s notification settings."));
        col = col
            .child(div().child("Open files with"))
            .child(Input::new(&self.editor).small());
        for (i, d) in self.defaults.iter().enumerate() {
            let weak = cx.weak_entity();
            let inherit = d.folder.is_some();
            col = col.child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_lg()
                    .child(
                        div().text_sm().font_weight(FontWeight::SEMIBOLD).child(
                            d.folder
                                .as_ref()
                                .map(|f| format!("Project: {}", f.display()))
                                .unwrap_or_else(|| "Defaults for new threads".into()),
                        ),
                    )
                    .child(div().text_xs().child("Model (provider/model) and agent"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&d.model).small()))
                            .child(div().flex_1().child(Input::new(&d.agent).small())),
                    )
                    .child(
                        Button::new(SharedString::from(format!("settings-permission-{i}")))
                            .label(
                                d.permission
                                    .map(|m| m.label())
                                    .unwrap_or("Inherit permission mode"),
                            )
                            .outline()
                            .small()
                            .dropdown_menu(move |mut menu, _, _| {
                                let modes = if inherit {
                                    vec![
                                        None,
                                        Some(PermissionMode::Supervised),
                                        Some(PermissionMode::AutoEdits),
                                        Some(PermissionMode::FullAccess),
                                    ]
                                } else {
                                    PermissionMode::ALL.into_iter().map(Some).collect()
                                };
                                for mode in modes {
                                    let weak = weak.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(
                                            mode.map(|m| m.label())
                                                .unwrap_or("Inherit permission mode"),
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                let _ = weak.update(cx, |this, cx| {
                                                    this.defaults[i].permission = mode;
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    );
                                }
                                menu
                            }),
                    ),
            );
        }
        col=col.child(div().text_xs().text_color(theme.muted_foreground).child("Defaults apply to new threads. Saved project grants apply in every mode, including Supervised. Supervised asks before writes and shell commands without a saved grant. Auto-accept edits allows file edits; shell commands still ask. Full access allows all actions supported by the server. Existing pending approvals remain explicit."));
        col = col.child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Saved project permissions"),
                )
                .child(
                    Button::new("refresh-saved-grants")
                        .label("Refresh")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .update(cx, |c, cx| c.refresh_saved_grants(cx))
                        })),
                ),
        );
        col=col.child(div().text_xs().text_color(theme.muted_foreground).child("Revoke removes the grant immediately for all threads in that project. It does not cancel a running tool."));
        let store = self.store.read(cx);
        if store.saved_grants_loading {
            col = col.child("Loading saved permissions…");
        }
        if let Some(error) = &store.saved_grants_error {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if !store.saved_grants_loading && store.saved_grants.is_empty() {
            col = col.child(div().text_sm().child("No saved grants."));
        }
        for grant in &store.saved_grants {
            let id = grant.id.clone();
            let c = self.controller.clone();
            let project = store
                .projects
                .iter()
                .find(|(_, pid)| **pid == grant.project_id)
                .map(|(p, _)| p.display().to_string())
                .unwrap_or_else(|| grant.project_id.clone());
            col = col.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .child(format!("{project}: {} → {}", grant.action, grant.resource)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("revoke-{}", grant.id)))
                            .label("Revoke now")
                            .outline()
                            .small()
                            .disabled(store.grant_sending.contains(&grant.id))
                            .on_click(move |_, _, cx| {
                                c.update(cx, |c, cx| c.revoke_saved_grant(id.clone(), cx))
                            }),
                    ),
            );
        }
        col = col.child(div().font_weight(FontWeight::SEMIBOLD).child("Keybindings"));
        col=col.child(div().text_xs().child("secondary = Cmd on macOS, Ctrl on Linux. Leave empty to unbind. Changes apply when saved."));
        for (_, label, input) in &self.keys {
            col = col.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_sm().child(*label))
                    .child(div().w(px(220.)).child(Input::new(input).small())),
            );
        }
        let mut root = v_flex().gap_3().child(
            div()
                .id("settings-scroll")
                .max_h(px(530.))
                .overflow_y_scroll()
                .child(col),
        );
        if let Some(error) = &self.error {
            root = root.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        root.child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("cancel-settings")
                        .label("Cancel")
                        .ghost()
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("save-settings")
                        .label("Save settings")
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                ),
        )
    }
}
