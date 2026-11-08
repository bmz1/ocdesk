//! "Go to file" (Cmd/Ctrl+P): fuzzy file search in the current folder
//! (`/api/fs/find`), opening the pick in an editor tab.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::model::FsEntry;
use crate::controller::Controller;
use crate::ui::editor_tabs::FileTabs;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(100);

pub struct GoToFile {
    input: Entity<InputState>,
    controller: Entity<Controller>,
    tabs: Entity<FileTabs>,
    dir: PathBuf,
    results: Vec<String>,
    selected: usize,
    seq: u64,
    /// Dropping this cancels debounce and any outstanding read reply.
    search_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl GoToFile {
    fn new(
        controller: Entity<Controller>,
        tabs: Entity<FileTabs>,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search files by name…"));
        let subs =
            vec![cx.subscribe_in(
                &input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::Change => this.search(cx),
                    InputEvent::PressEnter { .. } => this.confirm(window, cx),
                    _ => {}
                },
            )];
        Self {
            input,
            controller,
            tabs,
            dir,
            results: Vec::new(),
            selected: 0,
            seq: 0,
            search_task: None,
            _subs: subs,
        }
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        self.search_task = None;
        self.seq += 1;
        let seq = self.seq;
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.results.clear();
            cx.notify();
            return;
        }
        let (controller, dir) = (self.controller.clone(), self.dir.clone());
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            if this.update(cx, |this, _| this.seq != seq).unwrap_or(true) {
                return;
            }
            let Some(rx) = controller.update(cx, |c, _| c.find_files(dir, query)) else {
                return;
            };
            let Ok(Ok(found)) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.seq != seq {
                    return;
                }
                this.results = found
                    .into_iter()
                    .filter(|e: &FsEntry| !e.is_dir)
                    .map(|e| e.path)
                    .collect();
                this.selected = 0;
                cx.notify();
            });
        }));
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.results.get(self.selected).cloned() else {
            return;
        };
        self.open(path, window, cx);
    }

    fn open(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let tabs = self.tabs.clone();
        window.defer(cx, move |window, cx| {
            tabs.update(cx, |t, cx| t.open(&path, None, window, cx));
        });
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.results.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
            cx.notify();
        }
    }
}

impl Render for GoToFile {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let query_empty = self.input.read(cx).value().trim().is_empty();
        let rows = self.results.iter().enumerate().map(|(ix, path)| {
            let (dir, name) = match path.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), path.clone()),
            };
            let p = path.clone();
            h_flex()
                .id(("goto-row", ix))
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .when(ix == self.selected, |el| el.bg(theme.secondary))
                .hover(|s| s.bg(theme.secondary.opacity(0.6)))
                .child(
                    Icon::new(IconName::File)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(div().flex_none().child(name))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(dir),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.open(p.clone(), window, cx)))
        });
        v_flex()
            .gap_2()
            .capture_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                match e.keystroke.key.as_str() {
                    "up" => this.step(-1, cx),
                    "down" => this.step(1, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("goto-results")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(self.results.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(if query_empty {
                                    "Type to search this project"
                                } else {
                                    "No matching files"
                                }),
                        )
                    }),
            )
    }
}

/// Open the dialog for the current folder.
pub fn open(
    controller: Entity<Controller>,
    tabs: Entity<FileTabs>,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let Some(dir) = controller.read(cx).view_store().read(cx).target_folder() else {
        return;
    };
    let view = cx.new(|cx| GoToFile::new(controller, tabs, dir, window, cx));
    let initial_focus = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            let input = view.read(cx).input.clone();
            input.update(cx, |i, cx| i.focus(window, cx));
        }
        dialog
            .entrance(gpui_kit::component::dialog::DialogEntrance::None)
            .title("Go to file")
            .w(px(560.))
            .child(view.clone())
    });
}
