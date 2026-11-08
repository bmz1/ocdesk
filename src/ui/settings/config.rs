//! A focused editor for actual OpenCode config files, with explicit disk saves.
use super::*;
use crate::opencode_config::{self as config, ConfigFile, Snapshot};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};

/// Disk I/O and server reload are mutually exclusive; dirty state is independent.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Operation {
    #[default]
    Idle,
    Loading,
    Saving,
    Reloading,
}

pub struct ConfigEditor {
    controller: Entity<Controller>,
    projects: Vec<PathBuf>,
    project: Option<PathBuf>,
    global: Option<PathBuf>,
    files: Vec<ConfigFile>,
    snapshot: Option<Snapshot>,
    editor: Entity<EditorState>,
    baseline: String,
    dirty: bool,
    operation: Operation,
    error: Option<String>,
    message: Option<String>,
    _sub: Subscription,
}

impl ConfigEditor {
    pub fn new(
        controller: Entity<Controller>,
        prefs: &Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .searchable(true)
                .soft_wrap(false)
        });
        let sub = cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dirty = editor.read(cx).value().as_ref() != this.baseline;
                if this.dirty {
                    this.error = None;
                    this.message = None;
                }
                cx.notify();
            }
        });
        Self {
            controller,
            projects: prefs.folders.clone(),
            project: None,
            global: config::global_directory(),
            files: Vec::new(),
            snapshot: None,
            editor,
            baseline: String::new(),
            dirty: false,
            operation: Operation::Idle,
            error: None,
            message: None,
            _sub: sub,
        }
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.dirty || self.operation == Operation::Saving
    }

    pub fn show_project(
        &mut self,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation != Operation::Idle || (self.project == project && self.snapshot.is_some())
        {
            return;
        }
        if self.dirty {
            self.error =
                Some("Save or discard your edits before switching configuration files.".into());
            cx.notify();
            return;
        }
        self.project = project;
        self.load(window, cx);
    }

    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation != Operation::Idle {
            return;
        }
        if self.dirty {
            self.error =
                Some("Save or discard your edits before switching configuration files.".into());
            cx.notify();
            return;
        }
        let Some(global) = self.global.clone() else {
            self.error = Some("Could not locate OpenCode's global configuration directory.".into());
            cx.notify();
            return;
        };
        let project = self.project.clone();
        self.snapshot = None;
        self.files.clear();
        self.operation = Operation::Loading;
        self.error = None;
        self.message = None;
        self.editor.update(cx, |e, cx| e.set_readonly(true, cx));
        let task = cx.background_executor().spawn(async move {
            let files = config::discover(&global, project.as_deref())?;
            let file = files
                .iter()
                .rev()
                .find(|f| !f.inherited)
                .or_else(|| files.last())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("No configuration file"))?;
            let snapshot = config::read(file)?;
            Ok::<_, anyhow::Error>((files, snapshot))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.operation = Operation::Idle;
                match result {
                    Ok((files, snapshot)) => {
                        this.files = files;
                        this.install(snapshot, window, cx);
                    }
                    Err(e) => this.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn install(&mut self, snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        let text = snapshot
            .original
            .as_deref()
            .unwrap_or(config::TEMPLATE)
            .replace("\r\n", "\n");
        self.baseline = text.clone();
        self.dirty = false;
        self.editor.update(cx, |e, cx| {
            e.set_value(text, window, cx);
            e.set_readonly(snapshot.file.inherited, cx);
        });
        self.snapshot = Some(snapshot);
        self.error = None;
        self.message = None;
    }

    fn read_file(&mut self, file: ConfigFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation != Operation::Idle {
            return;
        }
        self.operation = Operation::Loading;
        self.editor.update(cx, |e, cx| e.set_readonly(true, cx));
        let task = cx
            .background_executor()
            .spawn(async move { config::read(file) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.operation = Operation::Idle;
                match result {
                    Ok(snapshot) => this.install(snapshot, window, cx),
                    Err(e) => {
                        this.error = Some(format!("{e:#}"));
                        let readonly = this.snapshot.as_ref().is_none_or(|s| s.file.inherited);
                        this.editor.update(cx, |e, cx| e.set_readonly(readonly, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        if self.operation != Operation::Idle || snapshot.file.inherited {
            return;
        }
        let crlf = snapshot
            .original
            .as_ref()
            .is_some_and(|s| s.contains("\r\n"));
        let text = String::from_utf8(crate::files::encode(&self.editor.read(cx).value(), crlf))
            .expect("UTF-8 editor content");
        self.operation = Operation::Saving;
        self.error = None;
        self.message = None;
        self.editor.update(cx, |e, cx| e.set_readonly(true, cx));
        let task = cx
            .background_executor()
            .spawn(async move { config::save(&snapshot, &text) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.operation = Operation::Idle;
                match result {
                    Ok(snapshot) => {
                        this.install(snapshot, window, cx);
                        this.message = Some(
                            "Saved to disk. Reload OpenCode to apply the configuration.".into(),
                        );
                    }
                    Err(e) => {
                        this.error = Some(format!("{e:#}"));
                        this.editor.update(cx, |e, cx| e.set_readonly(false, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn reload_opencode(&mut self, cx: &mut Context<Self>) {
        if self.operation != Operation::Idle || self.dirty {
            return;
        }
        let rx = match self.controller.read(cx).reload_configuration(cx) {
            Ok(rx) => rx,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
                return;
            }
        };
        self.operation = Operation::Reloading;
        self.error = None;
        self.message = None;
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("OpenCode reload was interrupted")));
            let _ = this.update(cx, |this, cx| {
                this.operation = Operation::Idle;
                match result {
                    Ok(()) => {
                        this.message = Some("OpenCode configuration reloaded.".into());
                        this.controller
                            .update(cx, |c, cx| c.configuration_changed(cx));
                    }
                    Err(e) => this.error = Some(format!("Could not reload OpenCode: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for ConfigEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let locked = self.operation != Operation::Idle;
        let scope_locked = locked || self.dirty;
        let weak = cx.weak_entity();
        let projects = self.projects.clone();
        let current_project = self.project.clone();
        let project = Button::new("config-project")
            .label(
                self.project
                    .as_deref()
                    .map(super::super::folder_name)
                    .unwrap_or_else(|| "Project".into()),
            )
            .small()
            .when(self.project.is_some(), |b| b.outline())
            .when(self.project.is_none(), |b| b.ghost())
            .dropdown_caret(true)
            .disabled(scope_locked || projects.is_empty())
            .dropdown_menu(move |mut menu, _, _| {
                for folder in &projects {
                    let weak = weak.clone();
                    let folder = folder.clone();
                    menu = menu.item(
                        PopupMenuItem::new(folder.display().to_string())
                            .checked(current_project.as_ref() == Some(&folder))
                            .on_click(move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.show_project(Some(folder.clone()), window, cx);
                                });
                            }),
                    );
                }
                menu
            });
        let weak = cx.weak_entity();
        let files = self.files.clone();
        let selected_path = self.snapshot.as_ref().map(|s| s.file.path.clone());
        let selected_label = selected_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Choose configuration".into());
        let chooser = Button::new("config-file")
            .label(selected_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .disabled(scope_locked || files.len() < 2)
            .dropdown_menu(move |mut menu, _, _| {
                for file in &files {
                    let weak = weak.clone();
                    let file = file.clone();
                    let label = format!(
                        "{}{}",
                        file.path.display(),
                        if file.inherited { " (inherited)" } else { "" }
                    );
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(selected_path.as_ref() == Some(&file.path))
                            .on_click(move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.read_file(file.clone(), window, cx)
                                });
                            }),
                    );
                }
                menu
            });
        let missing = self.snapshot.as_ref().is_some_and(|s| s.original.is_none());
        let inherited = self.snapshot.as_ref().is_some_and(|s| s.file.inherited);
        let file = self.snapshot.as_ref().map(|s| s.file.clone());
        v_flex().w_full().gap_2()
            .child(h_flex().gap_1().items_center()
                .child(Button::new("config-global").label("Global").small()
                    .when(self.project.is_none(), |b| b.outline()).when(self.project.is_some(), |b| b.ghost())
                    .disabled(scope_locked).on_click(cx.listener(|this, _, window, cx| { this.show_project(None, window, cx); })))
                .child(project)
                .child(div().flex_1())
                .child(Button::new("config-schema").label("Config reference ↗").ghost().xsmall()
                    .on_click(|_, _, cx| cx.open_url("https://opencode.ai/v2/docs/config"))))
            .child(div().w_full().min_w_0().overflow_x_hidden().child(chooser))
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                if self.operation == Operation::Loading { "Loading configuration…" }
                else if self.operation == Operation::Saving { "Saving configuration…" }
                else if inherited { "Inherited from a parent directory · read-only. Choose this project's config to add overrides." }
                else if missing { "No file yet. Create this JSONC configuration when you're ready." }
                else if self.dirty { "Unsaved changes · save or discard before switching files." }
                else { "Edits preserve comments and formatting. JSONC syntax is checked on save; OpenCode validates its settings on reload." }))
            .child(div().h(px(280.)).w_full().rounded_md().border_1().border_color(theme.border)
                .child(Editor::new(&self.editor).bordered(false).h_full()))
            .when_some(self.error.clone(), |el, error| el.child(div().text_xs().text_color(theme.danger).child(error)))
            .when_some(self.message.clone(), |el, message| el.child(div().text_xs().text_color(theme.success).child(message)))
            .child(h_flex().gap_2().flex_wrap()
                .child(Button::new("config-save").label(if missing { "Create configuration" } else { "Save configuration" }).primary().small()
                    .disabled(locked || inherited || self.snapshot.is_none() || (!self.dirty && !missing))
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))))
                .child(Button::new("config-discard").label(if self.dirty { "Discard & reload" } else { "Reload file" }).ghost().small()
                    .disabled(locked || self.snapshot.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| { if let Some(file) = file.clone() { this.read_file(file, window, cx); } })))
                .child(Button::new("config-external").label("External editor").ghost().small()
                    .disabled(locked || missing || self.snapshot.is_none())
                    .on_click(cx.listener(|this, _, _, cx| { if let Some(snapshot) = &this.snapshot {
                        this.controller.update(cx, |c, cx| c.open_agent_file(snapshot.file.path.clone(), cx));
                    } }))))
            .child(h_flex().gap_2().items_center()
                .child(Button::new("config-reload-opencode").label(if self.operation == Operation::Reloading { "Reloading OpenCode…" } else { "Reload OpenCode" }).outline().small()
                    .disabled(locked || self.dirty).on_click(cx.listener(|this, _, _, cx| this.reload_opencode(cx))))
                .child(div().text_xs().text_color(theme.muted_foreground).child("Available when all sessions are idle.")))
    }
}
