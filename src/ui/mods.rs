//! Mods panel (right panel tab). With the `mods` feature it loads every mod
//! in `mods::mods_dir()` on first show, stacks them, hot-reloads them on save
//! and refreshes each one only when state it read changes. Without the
//! feature it is an empty stub and the tab is hidden.

#[cfg(feature = "mods")]
pub use live::ModsPanel;
#[cfg(not(feature = "mods"))]
pub use stub::ModsPanel;

#[cfg(not(feature = "mods"))]
mod stub {
    use gpui_kit::*;

    use crate::controller::Controller;
    use crate::store::Store;

    pub struct ModsPanel;

    impl ModsPanel {
        pub fn new(_: Entity<Store>, _: Entity<Controller>, _: &mut Context<Self>) -> Self {
            Self
        }
    }

    impl Render for ModsPanel {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
}

#[cfg(feature = "mods")]
mod live {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Instant;

    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use gpui_kit::component::{ActiveTheme as _, Theme, h_flex, v_flex};
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::*;
    use gpui_shell::{ShellRoot, ShellRuntime, Watcher};

    use crate::controller::Controller;
    use crate::mods::host::{self, Intent};
    use crate::mods::reads::{Key, ReadSet, keys_for};
    use crate::store::{Store, StoreEvent};

    /// One loaded mod.
    struct Copy {
        name: String,
        reads: Rc<Cell<ReadSet>>,
        root: Option<Entity<ShellRoot>>,
        /// Hot reload; dropping it stops watching.
        _watch: Option<Watcher>,
        error: Option<String>,
    }

    pub struct ModsPanel {
        store: Entity<Store>,
        controller: Entity<Controller>,
        dir: Option<PathBuf>,
        /// Created on first show: an unused tab costs no QuickJS runtime.
        runtime: Option<Rc<ShellRuntime>>,
        runtime_error: Option<String>,
        loaded: bool,
        copies: Vec<Copy>,
        intents: mpsc::UnboundedSender<Intent>,
        _intents: Task<()>,
        _subs: Vec<Subscription>,
    }

    impl ModsPanel {
        pub fn new(
            store: Entity<Store>,
            controller: Entity<Controller>,
            cx: &mut Context<Self>,
        ) -> Self {
            let (intents, mut rx) = mpsc::unbounded();
            // Intents run here, after the script call that sent them returned.
            let task = cx.spawn(async move |this, cx| {
                while let Some(intent) = rx.next().await {
                    if this.update(cx, |this, cx| this.apply(intent, cx)).is_err() {
                        break;
                    }
                }
            });
            let subs = vec![
                cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                    this.refresh(keys_for(event), cx);
                }),
                cx.observe_global::<Theme>(|this, cx| this.refresh(&[Key::Theme], cx)),
            ];
            Self {
                store,
                controller,
                dir: crate::mods::mods_dir(),
                runtime: None,
                runtime_error: None,
                loaded: false,
                copies: Vec::new(),
                intents,
                _intents: task,
                _subs: subs,
            }
        }

        fn apply(&mut self, intent: Intent, cx: &mut Context<Self>) {
            match intent {
                Intent::SelectSession(id) => {
                    if self.store.read(cx).session(&id).is_none() {
                        tracing::debug!(session = %id, "mod: selectSession for unknown session");
                        return;
                    }
                    tracing::debug!(session = %id, "mod: select session");
                    self.controller
                        .update(cx, |c, cx| c.select_session(Some(id), cx));
                }
            }
        }

        /// Re-renders the copies that read any of `keys`.
        fn refresh(&mut self, keys: &[Key], cx: &mut Context<Self>) {
            if keys.is_empty() {
                return;
            }
            let Some(runtime) = &self.runtime else { return };
            for copy in &self.copies {
                if let Some(root) = &copy.root
                    && copy.reads.get().any(keys)
                    && let Err(e) = runtime.refresh(root, cx)
                {
                    tracing::warn!(module = %copy.name, "mod refresh failed: {e:#}");
                }
            }
        }

        fn load_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.loaded = true;
            let Some(dir) = self.dir.clone() else { return };
            let found = crate::mods::discover(&dir);
            if found.is_empty() {
                return;
            }
            // gpui_kit::init already ran gpui_base::init, which is all
            // gpui_shell::init adds apart from warming a lookup table.
            let started = Instant::now();
            let runtime = match ShellRuntime::new(cx) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("mods: script runtime failed: {e:#}");
                    self.runtime_error = Some(format!("{e:#}"));
                    return;
                }
            };
            tracing::info!(
                ms = started.elapsed().as_millis() as u64,
                "mods: runtime created"
            );
            for (name, path) in found {
                let copy = self.load(&runtime, name, path, window, cx);
                self.copies.push(copy);
            }
            // Anything loaded later must not inherit the last mod's grant.
            gpui_shell::policy::set_default(gpui_shell::policy::Policy::new());
            self.runtime = Some(runtime);
        }

        fn load(
            &self,
            runtime: &Rc<ShellRuntime>,
            name: String,
            path: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Copy {
            let reads = Rc::new(Cell::new(ReadSet::default()));
            let mut copy = Copy {
                name: name.clone(),
                reads: reads.clone(),
                root: None,
                _watch: None,
                error: None,
            };
            let Some(storage) = crate::mods::storage_path(&name) else {
                copy.error = Some("no config directory for mod storage".into());
                return copy;
            };
            if let Some(parent) = storage.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // try_load snapshots the default policy, so set it per mod.
            match host::policy(
                &name,
                &storage,
                self.store.clone(),
                reads,
                self.intents.clone(),
            ) {
                Ok(policy) => gpui_shell::policy::set_default(policy),
                Err(e) => {
                    copy.error = Some(format!("{e:#}"));
                    return copy;
                }
            }
            let started = Instant::now();
            match runtime.try_load(&path, window, cx) {
                Ok(root) => {
                    tracing::info!(
                        module = %name,
                        ms = started.elapsed().as_millis() as u64,
                        "mod loaded"
                    );
                    copy._watch = runtime
                        .watch(&root, window, cx)
                        .map_err(|e| tracing::warn!(module = %name, "no hot reload: {e:#}"))
                        .ok();
                    copy.root = Some(root);
                }
                Err(e) => {
                    tracing::warn!(module = %name, "mod failed to load: {e:#}");
                    copy.error = Some(format!("{e:#}"));
                }
            }
            copy
        }
    }

    impl Render for ModsPanel {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.loaded {
                self.load_all(window, cx);
            }
            let theme = cx.theme().clone();
            let note = |text: String| {
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(text)
            };
            let body = v_flex().id("mods").size_full().overflow_y_scroll();
            if let Some(e) = &self.runtime_error {
                return body.child(note(format!("Script runtime failed: {e}")));
            }
            if self.copies.is_empty() {
                let dir = self
                    .dir
                    .as_ref()
                    .map_or("(no config directory)".into(), |d| d.display().to_string());
                return body.child(note(format!(
                    "No mods. Add a folder with a main.js to {dir}"
                )));
            }
            body.children(self.copies.iter().map(|copy| {
                v_flex()
                    .flex_1()
                    .min_h(px(160.))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .h_7()
                            .px_3()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(copy.name.clone()),
                    )
                    .when_some(copy.error.clone(), |el, e| {
                        el.child(
                            div()
                                .p_3()
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.danger)
                                .child(e),
                        )
                    })
                    .when_some(copy.root.clone(), |el, root| {
                        el.child(div().flex_1().min_h_0().child(root))
                    })
            }))
        }
    }
}
