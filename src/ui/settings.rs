//! Editable settings, with staged changes and explicit save/cancel.
use crate::{controller::Controller, settings::Settings};
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

mod config;
mod mcp;
use config::ConfigEditor;
use mcp::McpPanel;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    OpenCode,
    Mcp,
    Permissions,
    Keybindings,
}
impl Section {
    const ALL: [Self; 5] = [
        Self::General,
        Self::OpenCode,
        Self::Mcp,
        Self::Permissions,
        Self::Keybindings,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::OpenCode => "OpenCode",
            Self::Mcp => "MCP servers",
            Self::Permissions => "Saved approvals",
            Self::Keybindings => "Keybindings",
        }
    }
    fn blurb(self) -> &'static str {
        match self {
            Self::General => "Appearance, notifications and your editor.",
            Self::OpenCode => "Edit OpenCode configuration for all projects or a specific project.",
            Self::Mcp => "Connections and setup for Model Context Protocol servers.",
            Self::Permissions => "Grants you saved with \"Always allow\".",
            Self::Keybindings => "secondary = Cmd on macOS, Ctrl on Linux. Leave empty to unbind.",
        }
    }
}
pub struct SettingsView {
    section: Section,
    store: Entity<crate::store::Store>,
    _subs: Vec<Subscription>,
    controller: Entity<Controller>,
    prefs: Settings,
    editor: Entity<InputState>,
    config: Entity<ConfigEditor>,
    mcp: Entity<McpPanel>,
    keys: Vec<(String, &'static str, Entity<InputState>)>,
    error: Option<String>,
}
pub fn open(controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
    controller.update(cx, |c, cx| c.refresh_saved_grants(cx));
    let prefs = controller.read(cx).preferences();
    let view = cx.new(|cx| SettingsView::new(controller, prefs, window, cx));
    let guard = view.clone();
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .entrance(gpui_kit::component::dialog::DialogEntrance::Fade)
            .title("Settings")
            .on_ok(|_, _, _| false)
            .on_cancel({
                let guard = guard.clone();
                move |_, _, cx| guard.update(cx, |view, cx| view.can_close(cx))
            })
            .w((window.viewport_size().width - px(48.)).min(px(960.)))
            .child(view.clone())
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
        let config = cx.new(|cx| ConfigEditor::new(controller.clone(), &prefs, window, cx));
        config.update(cx, |c, cx| c.load(window, cx));
        let parent = cx.weak_entity();
        let configure = std::rc::Rc::new(move |project, window: &mut Window, cx: &mut App| {
            let _ = parent.update(cx, |this: &mut Self, cx| {
                this.section = Section::OpenCode;
                this.config
                    .update(cx, |c, cx| c.show_project(project, window, cx));
                cx.notify();
            });
        });
        let mcp = cx.new(|cx| McpPanel::new(controller.clone(), &prefs, configure, cx));
        let keys = super::keys::editable()
            .into_iter()
            .map(|(name, default, label)| {
                let value = prefs.keybindings.get(&name).cloned().unwrap_or(default);
                (name, label, input(value, "Unbound", window, cx))
            })
            .collect();
        let store = controller.read(cx).view_store();
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        let config_sub = cx.observe(&config, |this, config, cx| {
            if !config.read(cx).has_unsaved_changes() {
                this.error = None;
            }
            cx.notify();
        });
        Self {
            section: Section::General,
            store,
            _subs: vec![sub, config_sub],
            controller,
            prefs,
            editor,
            config,
            mcp,
            keys,
            error: None,
        }
    }
    fn can_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.config.read(cx).has_unsaved_changes() {
            self.section = Section::OpenCode;
            self.error = Some(
                "Save or discard your OpenCode configuration edits before closing Settings.".into(),
            );
            cx.notify();
            return false;
        }
        true
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_close(cx) {
            return;
        }
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
            .child(row(
                "Updates",
                Some(&self.update_status(cx)),
                Button::new("settings-check-updates")
                    .label("Check now")
                    .small()
                    .outline()
                    .disabled(self.store.read(cx).updates.check().is_loading())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |c, cx| c.check_for_updates(true, cx));
                    })),
                &theme,
                true,
            ))
            .child(row(
                "Check for updates automatically",
                Some(
                    "Asks GitHub for the latest ocdesk release at start and every few hours. \
                     Nothing is downloaded or installed until you choose Update.",
                ),
                Switch::new("settings-check-for-updates")
                    .checked(self.prefs.check_for_updates)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.prefs.check_for_updates = *checked;
                        cx.notify();
                    })),
                &theme,
                true,
            ))
            .into_any_element()
    }

    /// One line on the running version and what the last check found.
    fn update_status(&self, cx: &App) -> String {
        use crate::domain::update::Install;
        let store = self.store.read(cx);
        let updates = &store.updates;
        let current = crate::controller::current_version();
        let status = match updates.install() {
            Install::Downloading { version, .. } => format!("downloading {version}…"),
            Install::Installing { version } => format!("installing {version}…"),
            Install::Ready { version } => format!("{version} is installed; restart to use it"),
            Install::Failed { version, error } => format!("updating to {version} failed: {error}"),
            Install::Idle => match (updates.check(), updates.latest()) {
                (check, _) if check.is_loading() => "checking…".into(),
                (_, Some(release)) => format!("{} is available", release.version),
                (check, None) => match check.error() {
                    Some(error) => format!("could not check: {error}"),
                    None if updates.checked() => "up to date".into(),
                    None => "not checked yet".into(),
                },
            },
        };
        format!("ocdesk {current} · {status}")
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
                        .label(if store.saved_grants_status.is_loading() {
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
        if let Some(error) = store.saved_grants_status.error() {
            col = col.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.to_owned()),
            );
        }
        if store.saved_grants.is_empty() {
            if !store.saved_grants_status.is_loading() {
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut nav = v_flex().w(px(168.)).flex_none().gap_1().pr_3();
        for section in Section::ALL {
            let selected = self.section == section;
            nav = nav.child(
                Button::new(SharedString::from(format!(
                    "settings-nav-{}",
                    section.label()
                )))
                .ghost()
                .small()
                .w_full()
                .justify_start()
                .h(px(36.))
                .px_3()
                .py_2()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .when(selected, |el| {
                    el.bg(theme.secondary).font_weight(FontWeight::MEDIUM)
                })
                .when(!selected, |el| el.text_color(theme.muted_foreground))
                .accessibility_label(section.label())
                .child(div().w_full().text_left().child(section.label()))
                .on_action(cx.listener(
                    move |this, _: &gpui_kit::component::dialog::Confirm, _, cx| {
                        this.section = section;
                        if section == Section::Mcp {
                            this.mcp.update(cx, |m, cx| m.refresh(cx));
                        }
                        this.error = None;
                        cx.stop_propagation();
                        cx.notify();
                    },
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.section = section;
                    if section == Section::Mcp {
                        this.mcp.update(cx, |m, cx| m.refresh(cx));
                    }
                    this.error = None;
                    cx.notify();
                })),
            );
        }
        let body = match self.section {
            Section::General => self.general(cx),
            Section::OpenCode => self.config.clone().into_any_element(),
            Section::Mcp => self.mcp.clone().into_any_element(),
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
                    .h((window.viewport_size().height - px(220.)).clamp(px(200.), px(530.)))
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.can_close(cx) {
                                    window.close_dialog(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("save-settings")
                            .label("Save app settings")
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}
