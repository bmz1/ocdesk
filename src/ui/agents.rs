//! Agents panel (right panel tab): the opencode agent files visible from
//! the selected session's folder, with an editor for the parts that make an
//! agent (description, mode, model, prompt) and a starter team template.
//!
//! The files are opencode's own (`.opencode/agent/*.md` and friends, see
//! `crate::agents`); ocdesk only reads and writes them. opencode watches
//! those directories and reloads, and the panel also asks the controller to
//! re-read the folder's agents so the composer's pickers update at once.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::agents::{self, AgentEdit, AgentFile, Scope};
use crate::api::model::{AgentMode, ModelInfo};
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};

const MODES: [(&str, &str); 3] = [
    ("primary", "Primary: pick it for a session"),
    ("subagent", "Subagent: other agents delegate to it"),
    ("all", "Both"),
];

struct Editing {
    file: AgentFile,
    /// The file's content when it was opened, to notice outside edits.
    loaded: String,
    description: Entity<InputState>,
    prompt: Entity<TextareaState>,
    mode: Option<String>,
    model: Option<String>,
}

pub struct AgentsPanel {
    store: Entity<Store>,
    controller: Entity<Controller>,
    folder: Option<PathBuf>,
    files: Vec<AgentFile>,
    editing: Option<Editing>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl AgentsPanel {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![
            cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| match ev {
                StoreEvent::SelectionChanged | StoreEvent::FoldersChanged => {
                    let folder = this.store.read(cx).target_folder();
                    if folder != this.folder {
                        this.editing = None;
                        this.reload(cx);
                    }
                }
                _ => {}
            }),
            // The catalog (built-in agents, models) changes.
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            store,
            controller,
            folder: None,
            files: Vec::new(),
            editing: None,
            error: None,
            _subs: subs,
        };
        this.reload(cx);
        this
    }

    /// Re-read the agent files (called whenever the tab opens).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.folder = self.store.read(cx).target_folder();
        self.files = self
            .folder
            .as_deref()
            .map(agents::discover)
            .unwrap_or_default();
        self.error = None;
        cx.notify();
    }

    fn edit(&mut self, file: AgentFile, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = std::fs::read_to_string(&file.path).unwrap_or_else(|_| file.doc.render());
        let file = AgentFile {
            doc: agents::AgentDoc::parse(&loaded),
            ..file
        };
        let e = AgentEdit::of(&file);
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("What this agent does (shown when picking it)")
                .default_value(e.description.clone())
        });
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("System prompt: who the agent is and how it works")
                .auto_grow(8, 28)
                .default_value(e.prompt.clone())
        });
        self.editing = Some(Editing {
            file,
            loaded,
            description,
            prompt,
            mode: e.mode,
            model: e.model,
        });
        self.error = None;
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = &self.editing else { return };
        let on_disk = std::fs::read_to_string(&ed.file.path).ok();
        if on_disk.as_deref().is_some_and(|d| d != ed.loaded) {
            self.error = Some(
                "The file changed on disk since you opened it. Go back and open it again to edit the latest version."
                    .into(),
            );
            cx.notify();
            return;
        }
        let edit = AgentEdit {
            description: ed.description.read(cx).value().to_string(),
            mode: ed.mode.clone(),
            model: ed.model.clone(),
            prompt: ed.prompt.read(cx).value().to_string(),
        };
        let mut doc = ed.file.doc.clone();
        edit.apply(&mut doc);
        let path = ed.file.path.clone();
        let name = ed.file.name.clone();
        match agents::write(&path, &doc) {
            Ok(()) => {
                self.editing = None;
                self.reload(cx);
                self.controller.update(cx, |c, cx| c.agents_changed(cx));
                super::kit::toast(window, cx, format!("Saved agent {name}"), None);
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = &self.editing else { return };
        let path = ed.file.path.clone();
        let name = ed.file.name.clone();
        let weak = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (path, weak, name) = (path.clone(), weak.clone(), name.clone());
            alert
                .title(format!("Delete agent {name}?"))
                .description(format!("This deletes {}.", path.display()))
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let res = std::fs::remove_file(&path);
                    let _ = weak.update(cx, |this, cx| match res {
                        Ok(()) => {
                            this.editing = None;
                            this.reload(cx);
                            this.controller.update(cx, |c, cx| c.agents_changed(cx));
                            super::kit::toast(window, cx, format!("Deleted agent {name}"), None);
                        }
                        Err(e) => {
                            this.error = Some(format!("Could not delete: {e}"));
                            cx.notify();
                        }
                    });
                    true
                })
        });
    }

    fn new_agent(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("security-review"));
        let weak = cx.entity().downgrade();
        let first = std::cell::Cell::new(true);
        let where_ = match scope {
            Scope::Project => format!("{}/.opencode/agent/", folder.display()),
            Scope::Global => agents::global_config_dir()
                .map(|d| format!("{}/agent/", d.display()))
                .unwrap_or_default(),
        };
        window.open_alert_dialog(cx, move |alert, window, cx| {
            if first.replace(false) {
                input.update(cx, |i, cx| i.focus(window, cx));
            }
            let (input, weak, folder) = (input.clone(), weak.clone(), folder.clone());
            alert
                .title(format!("New {} agent", scope.label().to_lowercase()))
                .description(
                    v_flex().gap_2().child(Input::new(&input)).child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("Saved in {where_}")),
                    ),
                )
                .confirm()
                .ok_text("Create")
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    let made = agents::new_agent_path(&folder, scope, &name).and_then(|path| {
                        agents::create(
                            &path,
                            &AgentEdit {
                                description: String::new(),
                                mode: Some("subagent".into()),
                                model: None,
                                prompt: format!("You are the {name} agent."),
                            },
                        )
                        .map(|()| path)
                    });
                    match made {
                        Ok(path) => {
                            let _ = weak.update(cx, |this, cx| {
                                this.reload(cx);
                                this.controller.update(cx, |c, cx| c.agents_changed(cx));
                                if let Some(f) = this.files.iter().find(|f| f.path == path).cloned()
                                {
                                    this.edit(f, window, cx);
                                }
                            });
                            true
                        }
                        Err(e) => {
                            super::kit::toast(window, cx, format!("{e:#}"), None);
                            false
                        }
                    }
                })
        });
    }

    /// Write the starter team into the project, skipping names that exist.
    fn add_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let (mut added, mut skipped, mut failed) = (Vec::new(), Vec::new(), None);
        for (name, edit) in agents::pipeline_template() {
            match agents::new_agent_path(&folder, Scope::Project, name) {
                Ok(path) if path.exists() => skipped.push(name),
                Ok(path) => match agents::create(&path, &edit) {
                    Ok(()) => added.push(name),
                    Err(e) => failed = Some(format!("{e:#}")),
                },
                Err(e) => failed = Some(format!("{e:#}")),
            }
        }
        self.reload(cx);
        self.error = failed;
        self.controller.update(cx, |c, cx| c.agents_changed(cx));
        let mut msg = if added.is_empty() {
            "No agents added".to_string()
        } else {
            format!(
                "Added {}. Pick the lead agent in the composer to run the team.",
                added.join(", ")
            )
        };
        if !skipped.is_empty() {
            msg.push_str(&format!(" Kept existing: {}.", skipped.join(", ")));
        }
        super::kit::toast(window, cx, msg, None);
    }

    fn models(&self, cx: &App) -> Vec<ModelInfo> {
        let s = self.store.read(cx);
        let mut models = s
            .catalog_folder()
            .and_then(|f| s.catalogs.get(&f))
            .map(|c| c.models.clone())
            .unwrap_or_default();
        let mut providers: Vec<String> = Vec::new();
        for m in &models {
            if !providers.contains(&m.provider_id) {
                providers.push(m.provider_id.clone());
            }
        }
        models.sort_by_key(|m| providers.iter().position(|p| *p == m.provider_id));
        models
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editing = self
            .editing
            .as_ref()
            .map(|e| (e.file.name.clone(), e.file.scope));
        let weak = cx.entity().downgrade();
        h_flex()
            .h_11()
            .flex_none()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .when(editing.is_some(), |el| {
                el.child(
                    Button::new("agents-back")
                        .icon(IconName::ChevronLeft)
                        .ghost()
                        .small()
                        .tooltip("All agents")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.editing = None;
                            this.reload(cx);
                        })),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .when(editing.is_none(), |d| d.pl_2())
                    .child(match &editing {
                        Some((name, scope)) => format!("{name} · {}", scope.label()),
                        None => "Agents".to_string(),
                    }),
            )
            .when(editing.is_none() && self.folder.is_some(), |el| {
                el.child(
                    Button::new("agents-new")
                        .icon(IconName::Plus)
                        .label("New")
                        .ghost()
                        .small()
                        .dropdown_menu(move |menu, _, _| {
                            let (w1, w2, w3) = (weak.clone(), weak.clone(), weak.clone());
                            menu.item(PopupMenuItem::new("Project agent…").on_click(
                                move |_, window, cx| {
                                    let _ = w1.update(cx, |t, cx| {
                                        t.new_agent(Scope::Project, window, cx)
                                    });
                                },
                            ))
                            .item(PopupMenuItem::new("Global agent…").on_click(
                                move |_, window, cx| {
                                    let _ = w2
                                        .update(cx, |t, cx| t.new_agent(Scope::Global, window, cx));
                                },
                            ))
                            .separator()
                            .item(
                                PopupMenuItem::new(
                                    "Team template: lead + architect, swe, security, review, qa",
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = w3.update(cx, |t, cx| t.add_template(window, cx));
                                }),
                            )
                        }),
                )
            })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if self.folder.is_none() {
            return div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Add a folder to manage its agents.")
                .into_any_element();
        }
        let store = self.store.read(cx);
        let builtin: Vec<(String, AgentMode)> = store
            .catalog_folder()
            .and_then(|f| store.catalogs.get(&f))
            .map(|c| {
                c.agents
                    .iter()
                    .filter(|a| !a.hidden && !self.files.iter().any(|f| f.name == a.id))
                    .map(|a| (a.id.clone(), a.mode))
                    .collect()
            })
            .unwrap_or_default();
        let section = |label: String| {
            div()
                .px_3()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let mut col = v_flex().pb_3();
        for scope in [Scope::Project, Scope::Global] {
            let files: Vec<&AgentFile> = self.files.iter().filter(|f| f.scope == scope).collect();
            col = col.child(section(scope.label().to_string()));
            if files.is_empty() {
                col = col.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(match scope {
                            Scope::Project => "No agent files in this folder. New → Team template adds a lead agent and five subagents to .opencode/agent/.",
                            Scope::Global => "No global agent files.",
                        }),
                );
            }
            for (i, f) in files.into_iter().enumerate() {
                let file = f.clone();
                let detail = [
                    Some(f.mode().unwrap_or_else(|| "all".into())),
                    f.model(),
                    Some(dir_label(f)),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                col = col.child(
                    v_flex()
                        .id(SharedString::from(format!("agent-{}-{i}", scope.label())))
                        .mx_1()
                        .px_2()
                        .py_1p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.secondary))
                        .child(div().text_sm().truncate().child(f.name.clone()))
                        .when_some(f.description(), |el, d| {
                            el.child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.foreground.opacity(0.75))
                                    .child(d),
                            )
                        })
                        .child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.edit(file.clone(), window, cx)
                        })),
                );
            }
        }
        if !builtin.is_empty() {
            col = col.child(section("Built into opencode".into())).child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        builtin
                            .iter()
                            .map(|(id, mode)| format!("{id} ({})", mode_label(*mode)))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
            );
        }
        col = col.child(
            div()
                .px_3()
                .pt_4()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Primary agents appear in the composer's agent picker. Subagents can be @mentioned, and other agents delegate to them; parallel delegations show as a tree in the conversation."),
        );
        col.into_any_element()
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(ed) = &self.editing else {
            return div().into_any_element();
        };
        let weak = cx.entity().downgrade();
        let label = |text: &'static str| {
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(text)
        };

        let mode_label = ed.mode.clone().unwrap_or_else(|| "all (default)".into());
        let w = weak.clone();
        let current_mode = ed.mode.clone();
        let mode_picker = super::kit::quiet_select("agent-mode", mode_label).dropdown_menu(
            move |mut menu, _, _| {
                for (id, text) in MODES {
                    let w = w.clone();
                    menu = menu.item(
                        PopupMenuItem::new(text)
                            .checked(current_mode.as_deref() == Some(id))
                            .on_click(move |_, _, cx| {
                                let _ = w.update(cx, |t, cx| {
                                    if let Some(e) = &mut t.editing {
                                        e.mode = Some(id.to_string());
                                    }
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            },
        );

        let models = self.models(cx);
        let current = ed.model.as_deref().and_then(agents::parse_model);
        let model_label = match &current {
            Some(r) => models
                .iter()
                .find(|m| m.provider_id == r.provider_id && m.id == r.id)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("{}/{}", r.provider_id, r.id)),
            None => "Default (session's model)".into(),
        };
        let set_model = |w: WeakEntity<Self>, m: Option<String>| {
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = w.update(cx, |t, cx| {
                    if let Some(e) = &mut t.editing {
                        e.model = m.clone();
                    }
                    cx.notify();
                });
            }
        };
        let (w, list, cur) = (weak.clone(), models.clone(), current.clone());
        let model_picker = super::kit::quiet_select("agent-model", model_label).dropdown_menu(
            move |mut menu, _, _| {
                menu = menu.scrollable(true).max_h(px(360.)).min_w(px(220.));
                menu = menu.item(
                    PopupMenuItem::new("Default (session's model)")
                        .checked(cur.is_none())
                        .on_click(set_model(w.clone(), None)),
                );
                let mut provider: Option<&str> = None;
                for m in &list {
                    if provider != Some(m.provider_id.as_str()) {
                        menu = menu.separator().label(m.provider_id.clone());
                        provider = Some(&m.provider_id);
                    }
                    let checked = cur
                        .as_ref()
                        .is_some_and(|r| r.provider_id == m.provider_id && r.id == m.id);
                    menu = menu.item(
                        PopupMenuItem::new(m.name.clone())
                            .checked(checked)
                            .on_click(set_model(
                                w.clone(),
                                Some(agents::model_string(&m.as_ref())),
                            )),
                    );
                }
                menu
            },
        );
        let variants = current.as_ref().and_then(|r| {
            models
                .iter()
                .find(|m| m.provider_id == r.provider_id && m.id == r.id)
                .filter(|m| !m.variants.is_empty())
                .map(|m| (r.clone(), m.variants.clone()))
        });
        let variant_picker = variants.map(|(r, vs)| {
            let w = weak.clone();
            super::kit::quiet_select(
                "agent-variant",
                r.variant.clone().unwrap_or_else(|| "default effort".into()),
            )
            .dropdown_menu(move |mut menu, _, _| {
                for v in std::iter::once(None).chain(vs.iter().cloned().map(Some)) {
                    let pick = crate::api::model::ModelRef {
                        variant: v.clone(),
                        ..r.clone()
                    };
                    menu = menu.item(
                        PopupMenuItem::new(v.clone().unwrap_or_else(|| "default".into()))
                            .checked(r.variant == v)
                            .on_click(set_model(w.clone(), Some(agents::model_string(&pick)))),
                    );
                }
                menu
            })
        });

        let path = ed.file.path.clone();
        v_flex()
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(path.display().to_string()),
                    )
                    .child(
                        Button::new("agent-open-editor")
                            .label("Open in editor")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let p = path.clone();
                                this.controller.update(cx, |c, cx| c.open_agent_file(p, cx));
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Description"))
                    .child(Input::new(&ed.description).small()),
            )
            .child(
                h_flex()
                    .gap_4()
                    .flex_wrap()
                    .child(v_flex().gap_1().child(label("Mode")).child(mode_picker))
                    .child(
                        v_flex().gap_1().child(label("Model")).child(
                            h_flex()
                                .gap_1()
                                .child(model_picker)
                                .children(variant_picker),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Prompt"))
                    .child(Textarea::new(&ed.prompt)),
            )
            .when_some(self.error.clone(), |el, e| {
                el.child(div().text_xs().text_color(theme.danger).child(e))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("agent-save")
                            .label("Save")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("agent-delete")
                            .label("Delete")
                            .danger()
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                    ),
            )
            .child(
                div().text_xs().text_color(theme.muted_foreground).child(
                    "Other frontmatter (permissions, request, color, steps, …) is kept as is.",
                ),
            )
            .into_any_element()
    }
}

fn mode_label(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Primary => "primary",
        AgentMode::Subagent => "subagent",
        _ => "all",
    }
}

/// `.opencode`, `~/.claude`, … for a row's detail line.
fn dir_label(f: &AgentFile) -> String {
    let root = f.root.display().to_string();
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if root.starts_with(&h) => format!("~{}", &root[h.len()..]),
        _ => f
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(root),
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.editing.is_some() {
            self.render_editor(cx)
        } else {
            self.render_list(cx)
        };
        v_flex().size_full().child(self.header(cx)).child(
            div()
                .id("agents-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(body),
        )
    }
}
