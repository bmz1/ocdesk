//! The main column: header, virtualized message list, status line, composer.
//! With no session selected it shows a centered Codex-style start screen.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::{Connection, Store, StoreEvent};
use crate::ui::composer::Composer;
use crate::ui::folder_name;
use crate::ui::message::{COLUMN_MAX_W, render_message};
use crate::ui::prose::{self, ProseCache};

/// Below this window width the right panel overlays the conversation.
const SPLIT_MIN_WINDOW_W: f32 = 1180.;
const PANEL_W: f32 = 460.;
const PANEL_MIN_W: f32 = 320.;
const PANEL_MAX_W: f32 = 900.;

/// Keyboard focus areas (F6 cycles them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Transcript,
    Composer,
    Panel,
}

/// Dragging the right panel's edge.
struct PanelResize;

impl Render for PanelResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The find bar (Cmd/Ctrl+F): matching rows of the open conversation.
pub struct FindState {
    input: Entity<gpui_kit::component::input::InputState>,
    query: String,
    /// Row indices that match, ascending.
    hits: Vec<usize>,
    /// Tool calls that match, by call id (their cards open).
    calls: std::rc::Rc<std::collections::HashSet<String>>,
    /// Index into `hits` of the row being shown.
    current: usize,
}

pub struct ConversationView {
    find: Option<FindState>,
    /// The todo pill shows its checklist.
    todos_open: bool,
    expanded_tools: std::collections::HashSet<String>,
    tool_scrolls: super::tool_card::ScrollHandles,
    /// Open right-panel tab.
    panel: Option<crate::settings::PanelTab>,
    panel_width: Pixels,
    changes: Entity<super::review::ChangesPanel>,
    terminal: Entity<super::terminal::TerminalView>,
    transcript_focus: FocusHandle,
    density: crate::settings::Density,
    /// Free-text inputs for pending question forms, keyed (form, field).
    form_inputs: super::form_view::FormInputs,
    store: Entity<Store>,
    controller: Entity<Controller>,
    composer: Entity<Composer>,
    tasks: Entity<super::tasks::TasksDrawer>,
    scroller: Entity<MessageScrollerState>,
    prose: Entity<ProseCache>,
    _subs: Vec<Subscription>,
}

impl ConversationView {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| Composer::new(controller.clone(), store.clone(), window, cx));
        // The store may already hold a selected conversation (demo mode).
        let rows = store
            .read(cx)
            .selected_conversation()
            .map_or(0, |c| c.messages.len());
        let scroller = cx.new(|cx| MessageScrollerState::new(rows, cx));
        let prose = cx.new(|_| ProseCache::new(store.clone(), scroller.clone()));
        let subs = vec![
            cx.subscribe_in(&store, window, Self::on_store_event),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Composer::focus(&composer, window, cx);
        let tasks =
            cx.new(|cx| super::tasks::TasksDrawer::new(store.clone(), controller.clone(), cx));
        let changes =
            cx.new(|cx| super::review::ChangesPanel::new(store.clone(), controller.clone(), cx));
        let terminal =
            cx.new(|cx| super::terminal::TerminalView::new(store.clone(), controller.clone(), cx));
        let prefs = store.read(cx).preferences.clone();
        let mut subs = subs;
        // Density changes every row's height.
        subs.push(cx.observe(&store, |this: &mut Self, store, cx| {
            let d = store.read(cx).preferences.density;
            if d != this.density {
                this.density = d;
                let n = this.row_count(cx);
                this.scroller
                    .update(cx, |s, cx| s.remeasure_items(0..n, cx));
            }
        }));
        Self {
            find: None,
            todos_open: false,
            tasks,
            panel: prefs.panel,
            panel_width: px(prefs
                .panel_width
                .unwrap_or(PANEL_W)
                .clamp(PANEL_MIN_W, PANEL_MAX_W)),
            changes,
            terminal,
            transcript_focus: cx.focus_handle(),
            density: prefs.density,
            expanded_tools: Default::default(),
            tool_scrolls: Default::default(),
            form_inputs: Default::default(),
            store,
            controller,
            composer,
            scroller,
            prose,
            _subs: subs,
        }
    }

    /// Create (or drop) the free-text inputs pending forms need, and keep
    /// typed text synced into the form's draft so required-field gating and
    /// submission see it.
    fn sync_form_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> super::form_view::FormInputs {
        // Collect what's needed up front: `cx` is mutably borrowed while
        // entities are created.
        let wanted_fields: Vec<(String, crate::api::model::FormField)> = {
            let store = self.store.read(cx);
            let mut out = Vec::new();
            if let Some(sid) = store.selected.clone()
                && let Some(forms) = store.forms.get(&sid)
            {
                for form in forms {
                    for field in form.fields.iter() {
                        if super::form_view::field_needs_input(field) {
                            out.push((form.id.clone(), field.clone()));
                        }
                    }
                }
            }
            out
        };
        let mut wanted = super::form_view::FormInputs::new();
        for (fid, field) in wanted_fields {
            let k = (fid.clone(), field.key.clone());
            if let Some(state) = self.form_inputs.get(&k) {
                wanted.insert(k.clone(), state.clone());
                continue;
            }
            let placeholder = field
                .placeholder
                .clone()
                .unwrap_or_else(|| "Your answer…".into());
            let state = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).placeholder(placeholder)
            });
            // Typed text becomes the field's answer immediately.
            let (form_id, key) = (fid.clone(), field.key.clone());
            let sub = cx.subscribe_in(
                &state,
                window,
                move |this, input, ev: &gpui_kit::component::input::InputEvent, _, cx| {
                    if !matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                        return;
                    }
                    let text = input.read(cx).value().to_string();
                    let (form_id, key) = (form_id.clone(), key.clone());
                    this.controller.update(cx, |c, cx| {
                        c.set_form_answer(form_id, key, serde_json::Value::String(text), cx);
                    });
                },
            );
            self._subs.push(sub);
            self.form_inputs.insert(k.clone(), state.clone());
            wanted.insert(k, state);
        }
        // Drop inputs of fields that are gone (form answered, cancelled, or
        // snapshot without it).
        self.form_inputs.retain(|k, _| wanted.contains_key(k));
        wanted
    }

    /// Open the find bar, or focus it when it's already open.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).selected.is_none() {
            return;
        }
        if let Some(f) = &self.find {
            f.input.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        let input = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx)
                .placeholder("Find in conversation")
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &gpui_kit::component::input::InputEvent, window, cx| {
                use gpui_kit::component::input::InputEvent;
                match ev {
                    InputEvent::Change => {
                        let q = input.read(cx).value().to_string();
                        this.set_find_query(q, cx);
                    }
                    InputEvent::PressEnter { shift, .. } => {
                        this.step_find(if *shift { -1 } else { 1 }, cx);
                        let _ = window;
                    }
                    _ => {}
                }
            },
        );
        self._subs.push(sub);
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.find = Some(FindState {
            input,
            query: String::new(),
            hits: Vec::new(),
            calls: Default::default(),
            current: 0,
        });
        cx.notify();
    }

    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            let n = self.row_count(cx);
            self.scroller
                .update(cx, |s, cx| s.remeasure_items(0..n, cx));
            Composer::focus(&self.composer, window, cx);
            cx.notify();
        }
    }

    fn set_find_query(&mut self, query: String, cx: &mut Context<Self>) {
        let (hits, calls) = {
            let store = self.store.read(cx);
            store
                .selected_conversation()
                .map(|c| find_rows(&c.messages, &query))
                .unwrap_or_default()
        };
        let Some(f) = self.find.as_mut() else { return };
        f.query = query;
        f.hits = hits;
        f.calls = std::rc::Rc::new(calls);
        f.current = f.hits.len().saturating_sub(1);
        let n = self.row_count(cx);
        self.scroller
            .update(cx, |s, cx| s.remeasure_items(0..n, cx));
        self.reveal_find(cx);
    }

    /// Move to the next (1) or previous (-1) match, wrapping.
    pub fn step_find(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(f) = self.find.as_mut() else { return };
        if f.hits.is_empty() {
            return;
        }
        let n = f.hits.len() as isize;
        f.current = ((f.current as isize + delta).rem_euclid(n)) as usize;
        self.reveal_find(cx);
    }

    fn reveal_find(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .find
            .as_ref()
            .and_then(|f| f.hits.get(f.current).copied())
        {
            self.scroller.update(cx, |s, cx| {
                s.scroll_to_item(ix, cx);
            });
        }
        cx.notify();
    }

    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.find.as_ref()?;
        let theme = cx.theme();
        let count = if f.query.is_empty() {
            String::new()
        } else if f.hits.is_empty() {
            "No matches".into()
        } else {
            format!("{} of {}", f.current + 1, f.hits.len())
        };
        Some(
            h_flex()
                .mx_4()
                .mt_1()
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary.opacity(0.5))
                .capture_action(cx.listener(
                    |this, _: &gpui_kit::component::input::Escape, window, cx| {
                        this.close_find(window, cx);
                        cx.stop_propagation();
                    },
                ))
                .child(
                    Icon::new(IconName::Search)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        gpui_kit::component::input::Input::new(&f.input)
                            .appearance(false)
                            .small(),
                    ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(count),
                )
                .child(
                    Button::new("find-prev")
                        .icon(IconName::ChevronUp)
                        .ghost()
                        .xsmall()
                        .tooltip("Previous match (Shift+Enter)")
                        .on_click(cx.listener(|this, _, _, cx| this.step_find(-1, cx))),
                )
                .child(
                    Button::new("find-next")
                        .icon(IconName::ChevronDown)
                        .ghost()
                        .xsmall()
                        .tooltip("Next match (Enter)")
                        .on_click(cx.listener(|this, _, _, cx| this.step_find(1, cx))),
                )
                .child(
                    Button::new("find-close")
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .tooltip("Close (Esc)")
                        .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
                )
                .into_any_element(),
        )
    }

    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Composer::focus(&self.composer, window, cx);
    }

    pub fn panel_open(&self, cx: &App) -> bool {
        self.panel.is_some() && self.store.read(cx).selected.is_some()
    }

    /// Open `tab` in the right panel, or close the panel if it shows it.
    pub fn toggle_panel(
        &mut self,
        tab: crate::settings::PanelTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::settings::PanelTab;
        let Some(sid) = self.store.read(cx).selected.clone() else {
            return;
        };
        self.panel = if self.panel == Some(tab) {
            None
        } else {
            Some(tab)
        };
        match self.panel {
            Some(PanelTab::Changes) => {
                self.controller.update(cx, |c, cx| c.refresh_diff(sid, cx));
            }
            Some(PanelTab::Tasks) => {
                self.tasks
                    .update(cx, |t, cx| t.show(super::tasks::TaskView::List, cx));
            }
            Some(PanelTab::Terminal) => {
                let focus = self.terminal.read(cx).focus_handle().clone();
                focus.focus(window, cx);
            }
            None => Composer::focus(&self.composer, window, cx),
        }
        let saved = self.panel;
        self.controller
            .update(cx, |c, cx| c.update_settings(cx, |s| s.panel = saved));
        cx.notify();
    }

    pub fn toggle_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_panel(crate::settings::PanelTab::Tasks, window, cx);
    }

    pub fn open_task(&mut self, view: super::tasks::TaskView, cx: &mut Context<Self>) {
        self.panel = Some(crate::settings::PanelTab::Tasks);
        self.tasks.update(cx, |t, cx| t.show(view, cx));
        cx.notify();
    }

    /// The Changes tab (Cmd/Ctrl+Shift+D).
    pub fn toggle_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_panel(crate::settings::PanelTab::Changes, window, cx);
    }

    pub fn focused_area(&self, window: &Window, cx: &App) -> Option<Area> {
        if self
            .composer
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            Some(Area::Composer)
        } else if self.transcript_focus.contains_focused(window, cx) {
            Some(Area::Transcript)
        } else if self
            .terminal
            .read(cx)
            .focus_handle()
            .contains_focused(window, cx)
        {
            Some(Area::Panel)
        } else {
            None
        }
    }

    pub fn focus_area(&mut self, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        match area {
            Area::Composer => Composer::focus(&self.composer, window, cx),
            Area::Transcript => self.transcript_focus.focus(window, cx),
            Area::Panel => {
                if self.panel == Some(crate::settings::PanelTab::Terminal) {
                    let f = self.terminal.read(cx).focus_handle().clone();
                    f.focus(window, cx);
                } else {
                    self.transcript_focus.focus(window, cx);
                }
            }
        }
    }

    fn resize_panel(&mut self, right_edge: Pixels, x: Pixels, cx: &mut Context<Self>) {
        let w = px(f32::from(right_edge - x)
            .clamp(PANEL_MIN_W, PANEL_MAX_W)
            .round());
        if w == self.panel_width {
            return;
        }
        self.panel_width = w;
        let saved: f32 = w.into();
        self.controller.update(cx, |c, cx| {
            c.update_settings(cx, |s| s.panel_width = Some(saved))
        });
        cx.notify();
    }

    fn row_count(&self, cx: &App) -> usize {
        self.store
            .read(cx)
            .selected_conversation()
            .map_or(0, |c| c.messages.len())
    }

    fn on_store_event(
        &mut self,
        _: &Entity<Store>,
        ev: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.store.read(cx).selected.clone();
        let is_selected = |sid: &String| selected.as_ref() == Some(sid);
        match ev {
            StoreEvent::SelectionChanged | StoreEvent::FoldersChanged => {
                self.find = None;
                self.todos_open = false;
                if self.panel == Some(crate::settings::PanelTab::Changes)
                    && let Some(sid) = self.store.read(cx).selected.clone()
                {
                    self.controller.update(cx, |c, cx| c.refresh_diff(sid, cx));
                }
                self.expanded_tools.clear();
                self.tool_scrolls.borrow_mut().clear();
                if matches!(ev, StoreEvent::SelectionChanged) {
                    self.prose.update(cx, |p, _| p.clear());
                }
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
                Composer::focus(&self.composer, window, cx);
            }
            StoreEvent::MessagesReset { session_id }
            | StoreEvent::MessageInserted { session_id, .. }
                if is_selected(session_id) && self.find.is_some() =>
            {
                // Row indices moved: match again (cheap; no allocation per text).
                let q = self
                    .find
                    .as_ref()
                    .map(|f| f.query.clone())
                    .unwrap_or_default();
                if matches!(ev, StoreEvent::MessagesReset { .. }) {
                    self.prose.update(cx, |p, _| p.invalidate());
                }
                let n = self.row_count(cx);
                match ev {
                    StoreEvent::MessageInserted { index, .. } => {
                        let i = *index;
                        self.scroller.update(cx, |s, cx| {
                            s.splice(i..i, 1, cx);
                        });
                    }
                    _ => self.scroller.update(cx, |s, cx| s.reset(n, cx)),
                }
                self.set_find_query(q, cx);
            }
            StoreEvent::MessageChanged { session_id, index }
                if is_selected(session_id) && self.find.is_some() =>
            {
                // Only this row's membership can change.
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.remeasure_items(i..i + 1, cx);
                });
                let store = self.store.read(cx);
                if let (Some(f), Some(m)) = (
                    self.find.as_mut(),
                    store
                        .selected_conversation()
                        .and_then(|c| c.messages.get(i)),
                ) {
                    let q = f.query.trim().to_lowercase();
                    let hit =
                        !q.is_empty() && row_matches(m, &q, std::rc::Rc::make_mut(&mut f.calls));
                    match (f.hits.binary_search(&i), hit) {
                        (Err(at), true) => f.hits.insert(at, i),
                        (Ok(at), false) => {
                            f.hits.remove(at);
                            f.current = f.current.min(f.hits.len().saturating_sub(1));
                        }
                        _ => {}
                    }
                }
            }
            StoreEvent::MessagesReset { session_id } if is_selected(session_id) => {
                self.prose.update(cx, |p, _| p.invalidate());
                let n = self.row_count(cx);
                self.scroller.update(cx, |s, cx| s.reset(n, cx));
            }
            StoreEvent::MessageInserted { session_id, index } if is_selected(session_id) => {
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.splice(i..i, 1, cx);
                });
            }
            StoreEvent::MessageChanged { session_id, index } if is_selected(session_id) => {
                // Only the streaming row is re-measured.
                let i = *index;
                self.scroller.update(cx, |s, cx| {
                    s.remeasure_items(i..i + 1, cx);
                });
            }
            _ => {}
        }
        cx.notify();
    }

    fn render_start(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = self.render_connection_banner(cx);
        let theme = cx.theme();
        let store = self.store.read(cx);
        let folders = store.folders.clone();
        let current = store.target_folder();
        // Each folder followed by its worktrees ("  ⎇ feature-x · branch").
        let choices: Vec<(PathBuf, String)> = folders
            .iter()
            .flat_map(|f| {
                std::iter::once((f.clone(), folder_name(f))).chain(
                    store.worktrees.get(f).into_iter().flatten().map(|w| {
                        (
                            w.clone(),
                            format!("   ⎇ {}", super::git::checkout_label(store, w)),
                        )
                    }),
                )
            })
            .collect();
        let controller = self.controller.clone();

        let chooser = if folders.is_empty() {
            Button::new("start-add-folder")
                .label("Add a folder to start")
                .icon(IconName::Folder)
                .primary()
                .on_click(cx.listener(|this, _, window, cx| {
                    crate::ui::add_folder(this.controller.clone(), window, cx);
                }))
                .into_any_element()
        } else {
            Button::new("folder-chooser")
                .label(current.as_deref().map(folder_name).unwrap_or_default())
                .icon(IconName::Folder)
                .ghost()
                .small()
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for (f, label) in &choices {
                        let (c, f2) = (controller.clone(), f.clone());
                        menu = menu.item(
                            PopupMenuItem::new(label.clone())
                                .checked(current.as_ref() == Some(f))
                                .on_click(move |_, _, cx| {
                                    let f = f2.clone();
                                    c.update(cx, |c, cx| c.new_session(Some(f), cx));
                                }),
                        );
                    }
                    let c = controller.clone();
                    menu.separator()
                        .item(
                            PopupMenuItem::new("Add folder…").on_click(move |_, window, cx| {
                                crate::ui::add_folder(c.clone(), window, cx);
                            }),
                        )
                })
                .into_any_element()
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .gap_6()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("What should we build?"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(chooser),
                    ),
            )
            .when(store.target_folder().is_some(), |el| {
                el.child(
                    v_flex()
                        .w_full()
                        .max_w(px(COLUMN_MAX_W))
                        .gap_2()
                        .children(banner)
                        .child(self.composer.clone()),
                )
            })
    }

    /// Why prompts can't be sent right now, above the composer. The
    /// sidebar footer carries the details and the Restart button.
    fn render_connection_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (color, icon, text, restart) = match self.store.read(cx).connection() {
            Connection::Connected => return None,
            // Startup is brief and the sidebar already says so.
            Connection::Starting | Connection::Connecting => return None,
            Connection::Reconnecting => (
                theme.warning,
                IconName::TriangleAlert,
                "Reconnecting to opencode… prompts are paused.",
                false,
            ),
            Connection::SidecarDown { .. } => (
                theme.danger,
                IconName::CircleAlert,
                "opencode isn't running, so prompts can't be sent.",
                true,
            ),
        };
        Some(
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(color)
                .child(Icon::new(icon).xsmall())
                .child(div().flex_1().min_w_0().child(text))
                .when(restart, |el| {
                    el.child(
                        Button::new("banner-restart")
                            .label("Restart")
                            .icon(IconName::RefreshCw)
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| c.restart(cx));
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    /// In place of the message list while history is loading or failed.
    fn render_placeholder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let store = self.store.read(cx);
        let sid = store.selected.clone()?;
        let has_messages = store
            .selected_conversation()
            .is_some_and(|c| !c.messages.is_empty());
        if has_messages {
            return None;
        }
        let centered = || {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .px_6()
                .text_sm()
                .text_color(theme.muted_foreground)
        };
        if let Some(err) = store.load_errors.get(&sid) {
            let connected = store.connection() == Connection::Connected;
            return Some(
                centered()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_color(theme.danger)
                            .child(Icon::new(IconName::CircleAlert).xsmall())
                            .child("Couldn't load this conversation"),
                    )
                    .child(
                        div()
                            .max_w(px(COLUMN_MAX_W))
                            .text_xs()
                            .text_center()
                            .child(err.clone()),
                    )
                    .child(
                        Button::new("retry-load")
                            .label("Retry")
                            .icon(IconName::RefreshCw)
                            .small()
                            .outline()
                            .disabled(!connected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let sid = sid.clone();
                                this.controller.update(cx, |c, cx| c.load_messages(sid, cx));
                            })),
                    )
                    .into_any_element(),
            );
        }
        store
            .selected_is_loading()
            .then(|| centered().child("Loading conversation…").into_any_element())
    }

    fn render_status(&self, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let s = self.store.read(cx);
        let sid = s.selected.as_ref()?;
        if let Some(message) = s.retry.get(sid) {
            return Some(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::TriangleAlert).xsmall())
                    .child(format!("Retrying: {message}"))
                    .into_any_element(),
            );
        }
        let err = s.errors.get(sid)?;
        Some(
            h_flex()
                .gap_1p5()
                .text_sm()
                .text_color(theme.danger)
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(err.clone())
                .into_any_element(),
        )
    }
}

impl Render for ConversationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let notice = store.notice.clone();
        let session = store.selected.as_ref().and_then(|id| store.session(id));
        let title = session.map(|s| s.display_title());
        let folder = session.map(|s| folder_name(&s.directory));
        let has_selection = store.selected.is_some();
        let branch_pill = super::git::branch_pill(&self.store, &self.controller, cx);
        let store_running = store
            .selected
            .as_ref()
            .map_or(0, |id| store.running_tasks(id));
        let form_inputs = self.sync_form_inputs(window, cx);

        let notice_bar = notice.map(|n| {
            h_flex()
                .mx_4()
                .mt_2()
                .px_3()
                .py_2()
                .gap_2()
                .rounded_lg()
                .bg(theme.secondary)
                .text_sm()
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(div().flex_1().child(n))
                .child(
                    Button::new("dismiss-notice")
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| {
                                s.notice = None;
                                cx.notify();
                            });
                        })),
                )
        });

        let body = if !has_selection {
            self.render_start(cx).into_any_element()
        } else {
            let store = self.store.clone();
            let expanded = self.expanded_tools.clone();
            let scrolls = self.tool_scrolls.clone();
            let view = cx.entity().downgrade();
            let toggle: super::tool_card::ToggleTool = std::rc::Rc::new(move |key, cx| {
                let _ = view.update(cx, |this, cx| {
                    let index = this.store.read(cx).selected_conversation().and_then(|c| {
                        c.messages
                            .iter()
                            .position(|m| key.starts_with(&format!("{}/", m.id())))
                    });
                    if !this.expanded_tools.remove(&key) {
                        this.expanded_tools.insert(key);
                    }
                    if let Some(i) = index {
                        this.scroller
                            .update(cx, |s, cx| s.remeasure_items(i..i + 1, cx));
                    }
                    cx.notify();
                });
            });
            let cache = self.prose.clone();
            let view = cx.entity().downgrade();
            let open_task: super::tasks::OpenTask = std::rc::Rc::new(move |task, cx| {
                let _ = view.update(cx, |this, cx| this.open_task(task, cx));
            });
            let controller = self.controller.clone();
            let on_action: super::message::OnRowAction = std::rc::Rc::new(move |action, _, cx| {
                use super::message::RowAction;
                controller.update(cx, |c, cx| match action {
                    RowAction::Fork { before } => c.fork(before, cx),
                    RowAction::Undo { message_id } => c.undo_to(message_id, cx),
                    RowAction::Retry { text } => c.send_prompt(
                        crate::api::model::PromptInput {
                            text,
                            ..Default::default()
                        },
                        cx,
                    ),
                    RowAction::Implement => c.implement_plan(cx),
                })
            });
            let find_hits = self
                .find
                .as_ref()
                .map(|f| f.hits.clone())
                .unwrap_or_default();
            let find_calls = self.find.as_ref().map(|f| f.calls.clone());
            let find_current = self
                .find
                .as_ref()
                .and_then(|f| f.hits.get(f.current).copied());
            let list = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                prose::sync_row(&store, &cache, ix, cx);
                let s = store.read(cx);
                let density = s.preferences.density;
                let idle = s.selected.as_ref().is_none_or(|id| !s.is_busy(id));
                let row = s
                    .selected_conversation()
                    .and_then(|c| c.messages.get(ix).map(|m| (c, m)))
                    .map(|(c, m)| {
                        use crate::api::model::Message;
                        let tools = super::tool_card::ToolCtx {
                            expanded: &expanded,
                            toggle: toggle.clone(),
                            open_task: &open_task,
                            scrolls: &scrolls,
                            density,
                            found: find_calls.as_deref(),
                        };
                        let next = c.messages.get(ix + 1);
                        let start = c.messages[..ix]
                            .iter()
                            .rposition(|m| matches!(m, Message::User(_)));
                        let prompt = start.and_then(|i| match &c.messages[i] {
                            Message::User(u) => Some(u),
                            _ => None,
                        });
                        let turn = &c.messages[start.map_or(0, |i| i + 1)..=ix];
                        let turn_end = matches!(m, Message::Assistant(_))
                            && match next {
                                Some(Message::User(_)) => true,
                                None => idle,
                                _ => false,
                            };
                        let row_ctx = super::message::RowCtx {
                            turn_end,
                            turn,
                            prompt,
                            next_id: next.map(|n| n.id()),
                            idle,
                            density,
                            on_action: on_action.clone(),
                        };
                        render_message(m, &cache, &tools, &row_ctx, cx)
                    })
                    .unwrap_or_else(|| div().into_any_element());
                let hit = find_hits.binary_search(&ix).is_ok();
                let current = find_current == Some(ix);
                h_flex().w_full().justify_center().px_6().py_3().child(
                    div()
                        .w_full()
                        .max_w(px(COLUMN_MAX_W))
                        .when(hit, |el| {
                            el.border_l_2().pl_2().border_color(if current {
                                cx.theme().warning
                            } else {
                                cx.theme().warning.opacity(0.35)
                            })
                        })
                        .child(row),
                )
            })
            .with_bottom_fade(theme.background);
            let placeholder = self.render_placeholder(cx);
            let content = match placeholder {
                Some(p) => p,
                None => list.into_any_element(),
            };
            v_flex()
                .size_full()
                .child(
                    div()
                        .id("transcript")
                        .track_focus(&self.transcript_focus)
                        .flex_1()
                        .min_h_0()
                        .child(content),
                )
                .child(
                    h_flex()
                        .w_full()
                        .justify_center()
                        .px_6()
                        .pb_2()
                        .pt_1()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(px(COLUMN_MAX_W))
                                .gap_2()
                                .child(super::form_view::render_forms(
                                    self.store.read(cx),
                                    &form_inputs,
                                    self.controller.clone(),
                                    cx,
                                ))
                                .children(self.render_todos(cx))
                                .child(super::permission::render_permissions(
                                    self.store.read(cx),
                                    self.controller.clone(),
                                    cx,
                                ))
                                .children(self.render_connection_banner(cx))
                                .children(self.render_status(cx))
                                .child(self.composer.clone())
                                .child(self.render_status_strip(cx)),
                        ),
                )
                .into_any_element()
        };

        use crate::settings::PanelTab;
        let panel_tab = self.panel.filter(|_| has_selection);
        let weak = cx.entity().downgrade();
        let tab_button =
            |id: &'static str, tab: PanelTab, label: String, icon: Icon, action: &dyn Action| {
                let weak = weak.clone();
                Button::new(id)
                    .icon(icon)
                    .label(label)
                    .ghost()
                    .small()
                    .selected(panel_tab == Some(tab))
                    .tooltip_with_action(
                        match tab {
                            PanelTab::Changes => "Changes: review diffs and comment",
                            PanelTab::Tasks => "Background tasks: subagents and shells",
                            PanelTab::Terminal => "Terminal for this session",
                        },
                        action,
                        None,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_panel(tab, window, cx));
                    })
            };
        let changes_label = {
            let s = self.store.read(cx);
            s.selected
                .as_ref()
                .and_then(|sid| s.diffs.get(sid))
                .filter(|d| !d.is_empty())
                .map(|d| {
                    let (a, r) = d
                        .iter()
                        .fold((0, 0), |(a, r), f| (a + f.additions, r + f.deletions));
                    format!("+{a} −{r}")
                })
                .unwrap_or_else(|| "Changes".into())
        };
        let sidebar_hidden = self.store.read(cx).preferences.sidebar_hidden;
        let header = h_flex()
            .id("conversation-header")
            .h_11()
            .flex_none()
            .px_4()
            .when(sidebar_hidden, |el| {
                el.pl(px(16. + super::app_view::TITLEBAR_INSET_W))
            })
            // The header is the window's drag region (title bar), and a
            // double-click zooms as the system title bar would.
            .window_control_area(WindowControlArea::Drag)
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 && cfg!(target_os = "macos") {
                    window.titlebar_double_click();
                }
            })
            .items_center()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .gap_1p5()
            .child(
                Button::new("toggle-sidebar")
                    .icon(IconName::PanelLeft)
                    .ghost()
                    .small()
                    .tooltip_with_action("Toggle sidebar", &super::keys::ToggleSidebar, None)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(super::keys::ToggleSidebar), cx)
                    }),
            )
            .when_some(folder.clone(), |el, f| {
                el.child(super::kit::monogram(&f, 18., cx))
                    .child(div().text_color(theme.muted_foreground).child(f))
                    .child(div().text_color(theme.muted_foreground).child("/"))
            })
            .child(div().min_w_0().truncate().child(title.unwrap_or_default()))
            .children(branch_pill)
            .child(div().flex_1())
            .when(has_selection, |el| {
                let running = store_running;
                el.child(self.render_usage(cx))
                    .child(tab_button(
                        "toggle-changes",
                        PanelTab::Changes,
                        changes_label,
                        Icon::new(IconName::File),
                        &super::keys::ToggleChanges,
                    ))
                    .child(tab_button(
                        "toggle-tasks",
                        PanelTab::Tasks,
                        if running > 0 {
                            format!("Tasks · {running}")
                        } else {
                            "Tasks".to_string()
                        },
                        super::icons::workflow(),
                        &super::keys::ToggleTasks,
                    ))
                    .child(tab_button(
                        "toggle-terminal",
                        PanelTab::Terminal,
                        "Terminal".into(),
                        super::icons::terminal(),
                        &super::keys::ToggleTerminal,
                    ))
            });

        let panel = panel_tab.map(|tab| {
            let body: AnyElement = match tab {
                // Cached: the conversation redraws per streamed delta; the
                // panels redraw when they notify.
                PanelTab::Changes => self
                    .changes
                    .clone()
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element(),
                PanelTab::Tasks => self
                    .tasks
                    .clone()
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element(),
                PanelTab::Terminal => self
                    .terminal
                    .clone()
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element(),
            };
            div()
                .id("right-panel")
                .relative()
                .w(self.panel_width)
                .h_full()
                .flex_none()
                // The panel can float over the conversation: keep wheel and
                // mouse events from reaching the message list behind it.
                .occlude()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(body)
                .child(
                    div()
                        .id("panel-resize")
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-3.))
                        .w(px(6.))
                        .cursor_col_resize()
                        .hover(|s| s.bg(theme.border.opacity(0.6)))
                        .on_drag(PanelResize, |_, _, _, cx| cx.new(|_| PanelResize))
                        .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                            if e.click_count() == 2 {
                                this.panel_width = px(PANEL_W);
                                cx.notify();
                            }
                        })),
                )
        });

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(header)
            .children(notice_bar)
            .when(self.controller.read(cx).has_failed_submission(), |el| {
                el.child(Button::new("retry-failed-submission")
                    .label("Retry last unconfirmed prompt (check history first)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |c, cx| c.retry_failed_submission(cx));
                    })))
            })
            .children(self.render_find_bar(cx))
            .child({
                // Side by side when there's room for the column and the
                // panel; otherwise the panel floats over the conversation.
                let split = window.viewport_size().width >= px(SPLIT_MIN_WINDOW_W);
                h_flex()
                    .id("conversation-split")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<PanelResize>, _, cx| {
                        this.resize_panel(e.bounds.right(), e.event.position.x, cx);
                    }))
                    .child(div().flex_1().min_w_0().h_full().child(body))
                    .when_some(panel, |el, panel| {
                        if split {
                            el.child(panel)
                        } else {
                            el.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .right_0()
                                    .shadow_lg()
                                    .child(panel),
                            )
                        }
                    })
            })
    }
}

impl ConversationView {
    /// The agent's todo progress above the prompt.
    fn render_todos(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let busy = store.selected.as_ref().is_some_and(|id| store.is_busy(id));
        let todos = super::todos::session_todos(&store.selected_conversation()?.messages);
        let view = cx.entity().downgrade();
        super::todos::pill(
            &todos,
            self.todos_open,
            busy,
            move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.todos_open = !this.todos_open;
                    cx.notify();
                });
            },
            cx,
        )
    }

    /// Header meter: context ring, tokens and cost; opens usage details.
    fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let Some(sid) = store.selected.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let Some(report) = store.usage.get(&sid) else {
            return div().into_any_element();
        };
        let report = Some(report);
        let fraction = report.and_then(|r| match (r.context, r.context_limit) {
            (Some(n), Some(l)) if l > 0 => Some(n as f32 / l as f32),
            _ => None,
        });
        let label = match report {
            Some(r) => format!(
                "{}  ${:.2}",
                super::message::format_tokens(r.tokens.total()),
                r.cost
            ),
            None => String::new(),
        };
        let tooltip: SharedString = match (report, fraction) {
            (Some(r), Some(f)) => format!(
                "Context ~{:.0}% of {} · {} tokens · ${:.4}. Click for details.",
                f * 100.,
                super::message::format_tokens(r.context_limit.unwrap_or(0)),
                r.tokens.total(),
                r.cost
            ),
            (Some(r), None) => format!(
                "{} tokens · ${:.4}. Click for details.",
                r.tokens.total(),
                r.cost
            ),
            _ => "Usage and context".into(),
        }
        .into();
        let compact = fraction.is_some_and(|f| f >= 0.8);
        let controller = self.controller.clone();
        let sid_click = sid.clone();
        h_flex()
            .gap_1()
            .items_center()
            .child(
                h_flex()
                    .id("usage-meter")
                    .gap_1p5()
                    .px_1p5()
                    .h_7()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.bg(theme.secondary))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tooltip.clone())
                            .build(window, cx)
                    })
                    .on_click(move |_, window, cx| {
                        super::usage::open(sid_click.clone(), controller.clone(), window, cx)
                    })
                    .child(super::kit::context_ring(
                        "context-ring",
                        fraction.unwrap_or(0.),
                        cx,
                    ))
                    .child(label),
            )
            .when(compact, |el| {
                el.child(
                    Button::new("compact-now")
                        .label("Compact")
                        .xsmall()
                        .outline()
                        .tooltip("Summarize the conversation to free context")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |c, cx| c.compact(cx))
                        })),
                )
            })
            .into_any_element()
    }

    /// Under the composer: folder path, branch, transcript detail.
    fn render_status_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let store = self.store.read(cx);
        let dir = store
            .selected
            .as_ref()
            .and_then(|id| store.session(id))
            .map(|s| s.directory.clone())
            .or_else(|| store.target_folder());
        let density = store.preferences.density;
        let home = dirs::home_dir();
        let path = dir.as_ref().map(|d| {
            let shown = match &home {
                Some(h) if d.starts_with(h) => {
                    format!("~/{}", d.strip_prefix(h).unwrap_or(d).display())
                }
                _ => d.display().to_string(),
            };
            shown.trim_end_matches('/').to_string()
        });
        let branch = dir
            .as_ref()
            .and_then(|d| store.folder_of(d).map(|f| (d.clone(), f)))
            .and_then(|(d, f)| store.vcs.get(&d).or_else(|| store.vcs.get(&f)))
            .and_then(|v| v.branch.clone());
        let controller = self.controller.clone();
        h_flex()
            .w_full()
            .px_2()
            .gap_3()
            .items_center()
            .text_xs()
            .text_color(theme.muted_foreground)
            .when_some(path, |el, p| {
                el.child(
                    h_flex()
                        .id("status-path")
                        .gap_1()
                        .min_w_0()
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground))
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(
                                "Open the folder in your editor",
                            )
                            .build(window, cx)
                        })
                        .on_click(move |_, _, cx| {
                            controller.update(cx, |c, cx| c.open_file(".".into(), cx))
                        })
                        .child(Icon::new(IconName::Folder).xsmall())
                        .child(div().min_w_0().truncate().child(p)),
                )
            })
            .when_some(branch, |el, b| {
                el.child(
                    h_flex()
                        .gap_1()
                        .flex_none()
                        .child(super::icons::git_branch().xsmall())
                        .child(b),
                )
            })
            .child(div().flex_1())
            .when(density != crate::settings::Density::Normal, |el| {
                el.child(
                    div()
                        .id("density-chip")
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.foreground))
                        .child(density.label())
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(super::keys::CycleDensity), cx)
                        }),
                )
            })
            .into_any_element()
    }
}

/// Rows whose prompt, reply, reasoning or tool call contains `query`
/// (ASCII case-insensitive), and the matching tool calls.
pub fn find_rows(
    messages: &[crate::api::model::Message],
    query: &str,
) -> (Vec<usize>, std::collections::HashSet<String>) {
    let q = query.trim().to_lowercase();
    let mut calls = std::collections::HashSet::new();
    if q.is_empty() {
        return (Vec::new(), calls);
    }
    let rows = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| row_matches(m, &q, &mut calls))
        .map(|(i, _)| i)
        .collect();
    (rows, calls)
}

/// Whether one message matches `q` (lowercase); records matching calls.
fn row_matches(
    m: &crate::api::model::Message,
    q: &str,
    calls: &mut std::collections::HashSet<String>,
) -> bool {
    use super::tool_card::contains_ci;
    use crate::api::model::{Block, Message};
    match m {
        Message::User(u) => contains_ci(&u.text, q),
        Message::Marker(m) => contains_ci(&m.text, q),
        Message::Assistant(a) => {
            let mut hit = false;
            for b in &a.blocks {
                match b {
                    Block::Prose { text, .. } => hit |= contains_ci(text, q),
                    Block::Tool(t) => {
                        if super::tool_card::tool_matches(t, q) {
                            calls.insert(t.call_id.clone());
                            hit = true;
                        }
                    }
                }
            }
            hit
        }
    }
}

