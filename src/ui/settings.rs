//! Editable settings, with staged changes and explicit save/cancel.
use crate::{
    api::model::ModelRef,
    controller::Controller,
    settings::{PermissionMode, ProjectDefaults, Settings},
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Theme, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;

struct DefaultsEditor {
    folder: Option<PathBuf>,
    model: Entity<InputState>,
    agent: Entity<InputState>,
    permission: Option<PermissionMode>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Defaults,
    Permissions,
    Keybindings,
}
impl Section {
    const ALL: [Self; 4] = [
        Self::General,
        Self::Defaults,
        Self::Permissions,
        Self::Keybindings,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Defaults => "Defaults",
            Self::Permissions => "Permissions",
            Self::Keybindings => "Keybindings",
        }
    }
    fn blurb(self) -> &'static str {
        match self {
            Self::General => "Appearance, notifications and your editor.",
            Self::Defaults => "Model, agent and permission mode for new threads.",
            Self::Permissions => "Grants you saved with \"Always allow\".",
            Self::Keybindings => "secondary = Cmd on macOS, Ctrl on Linux. Leave empty to unbind.",
        }
    }
}
pub struct SettingsView {
    section: Section,
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
        dialog.title("Settings").w(px(780.)).child(view.clone())
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
            section: Section::General,
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
impl SettingsView {
    fn general(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let current = self.prefs.theme.clone().unwrap_or_else(|| "dark".into());
        let mut modes = h_flex().gap_1();
        for mode in ["dark", "light", "system"] {
            let selected = current == mode;
            let label = format!("{}{}", mode[..1].to_uppercase(), &mode[1..]);
            modes = modes.child(
                Button::new(SharedString::from(format!("theme-{mode}")))
                    .label(label)
                    .small()
                    .when(selected, |b| b.outline())
                    .when(!selected, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.prefs.theme = Some(mode.into());
                        cx.notify();
                    })),
            );
        }
        let notifications = self.prefs.notifications;
        card(&theme)
            .child(row("Theme", None, modes, &theme, false))
            .child(row(
                "Desktop notifications",
                Some("When a thread finishes or needs you. Unread dots work either way."),
                Switch::new("settings-notifications")
                    .checked(notifications)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.prefs.notifications = *checked;
                        cx.notify();
                    })),
                &theme,
                true,
            ))
            .child(row(
                "Open files with",
                Some("Editor executable, no arguments."),
                div().w(px(220.)).child(Input::new(&self.editor).small()),
                &theme,
                true,
            ))
            .into_any_element()
    }

    fn defaults_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut col = v_flex().gap_4();
        for (i, d) in self.defaults.iter().enumerate() {
            let weak = cx.weak_entity();
            let inherit = d.folder.is_some();
            let (title, sub) = match &d.folder {
                Some(f) => (
                    f.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| f.display().to_string()),
                    Some(f.display().to_string()),
                ),
                None => ("All new threads".to_string(), None),
            };
            let permission = Button::new(SharedString::from(format!("settings-permission-{i}")))
                .label(d.permission.map(|m| m.label()).unwrap_or("Inherit"))
                .dropdown_caret(true)
                .outline()
                .small()
                .dropdown_menu(move |mut menu, _, _| {
                    let modes = if inherit {
                        std::iter::once(None)
                            .chain(PermissionMode::ALL.into_iter().map(Some))
                            .collect::<Vec<_>>()
                    } else {
                        PermissionMode::ALL.into_iter().map(Some).collect()
                    };
                    for mode in modes {
                        let weak = weak.clone();
                        menu = menu.item(
                            PopupMenuItem::new(mode.map(|m| m.label()).unwrap_or("Inherit"))
                                .on_click(move |_, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.defaults[i].permission = mode;
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                });
            col = col.child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .when_some(sub, |el, sub| {
                                el.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(sub),
                                )
                            }),
                    )
                    .child(
                        card(&theme)
                            .child(row(
                                "Model",
                                Some("provider/model"),
                                div().w(px(260.)).child(Input::new(&d.model).small()),
                                &theme,
                                false,
                            ))
                            .child(row(
                                "Agent",
                                None,
                                div().w(px(260.)).child(Input::new(&d.agent).small()),
                                &theme,
                                true,
                            ))
                            .child(row("Permission mode", None, permission, &theme, true)),
                    ),
            );
        }
        let modes = [
            (
                "Supervised",
                "Asks before writes and shell commands without a saved grant.",
            ),
            (
                "Auto-accept edits",
                "Allows file edits; shell commands still ask.",
            ),
            ("Full access", "Allows everything the server supports."),
        ];
        let mut legend = v_flex()
            .gap_1()
            .p_3()
            .rounded_lg()
            .bg(theme.secondary.opacity(0.5))
            .text_xs();
        for (name, desc) in modes {
            legend = legend.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(110.))
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name),
                    )
                    .child(div().text_color(theme.muted_foreground).child(desc)),
            );
        }
        legend =
            legend.child(div().pt_1().text_color(theme.muted_foreground).child(
                "Saved grants apply in every mode. Pending approvals always stay explicit.",
            ));
        col.child(legend).into_any_element()
    }

    fn permissions_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let mut col = v_flex().gap_3().child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Revoking applies to every thread in that project at once."),
                )
                .child(
                    Button::new("refresh-saved-grants")
                        .label(if store.saved_grants_loading {
                            "Loading…"
                        } else {
                            "Refresh"
                        })
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .update(cx, |c, cx| c.refresh_saved_grants(cx))
                        })),
                ),
        );
        if let Some(error) = &store.saved_grants_error {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if store.saved_grants.is_empty() {
            if !store.saved_grants_loading {
                col = col.child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .py_8()
                        .rounded_lg()
                        .border_1()
                        .border_dashed()
                        .border_color(theme.border)
                        .child(div().text_sm().child("No saved grants"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Choose \"Always allow\" on an approval to save one."),
                        ),
                );
            }
            return col.into_any_element();
        }
        // Group by project.
        let mut groups: Vec<(String, Vec<&crate::api::features::SavedGrant>)> = Vec::new();
        for grant in &store.saved_grants {
            let project = store
                .projects
                .iter()
                .find(|(_, pid)| **pid == grant.project_id)
                .map(|(p, _)| p.display().to_string())
                .unwrap_or_else(|| grant.project_id.clone());
            match groups.iter_mut().find(|(p, _)| *p == project) {
                Some((_, v)) => v.push(grant),
                None => groups.push((project, vec![grant])),
            }
        }
        for (project, grants) in groups {
            let mut list = card(&theme);
            for (i, grant) in grants.into_iter().enumerate() {
                let id = grant.id.clone();
                let c = self.controller.clone();
                list = list.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_3()
                        .when(i > 0, |el| el.border_t_1().border_color(theme.border))
                        .child(
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .bg(theme.secondary)
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child(grant.action.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .child(grant.resource.clone()),
                        )
                        .child(
                            Button::new(SharedString::from(format!("revoke-{}", grant.id)))
                                .label("Revoke")
                                .ghost()
                                .xsmall()
                                .disabled(store.grant_sending.contains(&grant.id))
                                .on_click(move |_, _, cx| {
                                    c.update(cx, |c, cx| c.revoke_saved_grant(id.clone(), cx))
                                }),
                        ),
                );
            }
            col = col.child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(project),
                    )
                    .child(list),
            );
        }
        col.into_any_element()
    }

    fn keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = card(&theme);
        for (i, (_, label, input)) in self.keys.iter().enumerate() {
            list = list.child(row(
                label,
                None,
                div().w(px(220.)).child(Input::new(input).small()),
                &theme,
                i > 0,
            ));
        }
        list.into_any_element()
    }
}

/// A bordered group of settings rows.
fn card(theme: &Theme) -> Div {
    v_flex().rounded_lg().border_1().border_color(theme.border)
}

/// One setting: title and optional description on the left, control on the right.
fn row(
    title: &str,
    desc: Option<&str>,
    control: impl IntoElement,
    theme: &Theme,
    divider: bool,
) -> Div {
    h_flex()
        .px_3()
        .py_2p5()
        .gap_4()
        .justify_between()
        .when(divider, |el| el.border_t_1().border_color(theme.border))
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().child(title.to_string()))
                .when_some(desc, |el, d| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(d.to_string()),
                    )
                }),
        )
        .child(div().flex_none().child(control))
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut nav = v_flex().w(px(160.)).flex_none().gap_0p5().pr_3();
        for section in Section::ALL {
            let selected = self.section == section;
            nav = nav.child(
                div()
                    .id(SharedString::from(format!(
                        "settings-nav-{}",
                        section.label()
                    )))
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |el| {
                        el.bg(theme.secondary).font_weight(FontWeight::MEDIUM)
                    })
                    .when(!selected, |el| {
                        el.text_color(theme.muted_foreground).hover(|s| {
                            s.bg(theme.secondary.opacity(0.5))
                                .text_color(theme.foreground)
                        })
                    })
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    })),
            );
        }
        let body = match self.section {
            Section::General => self.general(cx),
            Section::Defaults => self.defaults_section(cx),
            Section::Permissions => self.permissions_section(cx),
            Section::Keybindings => self.keybindings(cx),
        };
        let pane = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .gap_0p5()
                    .pb_3()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.section.label()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.section.blurb()),
                    ),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pr_1()
                    .child(body),
            );

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .h(px(500.))
                    .items_start()
                    .child(nav)
                    .child(div().w(px(1.)).h_full().bg(theme.border).mr_4())
                    .child(pane),
            )
            .child(
                h_flex()
                    .pt_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.danger)
                            .when_some(self.error.clone(), |el, e| el.child(e)),
                    )
                    .child(
                        Button::new("cancel-settings")
                            .label("Cancel")
                            .ghost()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save-settings")
                            .label("Save")
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}
