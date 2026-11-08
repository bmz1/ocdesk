//! The Files panel: the current folder as a lazily loaded tree. A click on
//! a directory expands it (listing it on first use), a click on a file
//! opens it in an editor tab.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::lifecycle::{DirectoryReads, RefreshTargets};
use crate::api::model::FsEntry;
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};
use crate::ui::editor_tabs::FileTabs;

/// Tool calls arrive in bursts; refresh listings once they settle.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(600);
const ROW_H: f32 = 26.;
const INDENT: f32 = 14.;

#[derive(Default)]
struct Tree {
    /// Listing per directory (`""` = the folder).
    children: HashMap<String, Vec<FsEntry>>,
    expanded: HashSet<String>,
    loading: DirectoryReads,
    error: Option<String>,
}

#[derive(Clone)]
struct Row {
    entry: FsEntry,
    depth: usize,
    expanded: bool,
}

pub struct FileExplorer {
    store: Entity<Store>,
    controller: Entity<Controller>,
    tabs: Entity<FileTabs>,
    trees: HashMap<PathBuf, Tree>,
    scroll: UniformListScrollHandle,
    refresh_seq: u64,
    refresh_targets: RefreshTargets,
    _subs: Vec<Subscription>,
}

impl FileExplorer {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        tabs: Entity<FileTabs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![
            cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| match ev {
                StoreEvent::FilesChanged { dir, .. } => this.schedule_refresh(dir.clone(), cx),
                StoreEvent::SelectionChanged | StoreEvent::SessionsChanged => cx.notify(),
                _ => {}
            }),
            // The open tab is highlighted.
            cx.observe(&tabs, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            controller,
            tabs,
            trees: HashMap::new(),
            scroll: UniformListScrollHandle::new(),
            refresh_seq: 0,
            refresh_targets: RefreshTargets::default(),
            _subs: subs,
        }
    }

    fn dir(&self, cx: &App) -> Option<PathBuf> {
        self.store.read(cx).target_folder()
    }

    /// List `path` in the current folder.
    fn load(&mut self, dir: PathBuf, path: String, cx: &mut Context<Self>) {
        let tree = self.trees.entry(dir.clone()).or_default();
        if !tree.loading.begin(&path) {
            return;
        }
        let rx = self.controller.read(cx).fs_list(dir.clone(), path.clone());
        cx.spawn(async move |this, cx| {
            let res = match rx {
                Some(rx) => rx
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("cancelled"))),
                None => Err(anyhow::anyhow!("opencode is not running")),
            };
            let _ = this.update(cx, |this, cx| {
                let tree = this.trees.entry(dir.clone()).or_default();
                let again = tree.loading.finish(&path);
                match res {
                    Ok(entries) => {
                        tree.children.insert(path.clone(), entries);
                        tree.error = None;
                    }
                    Err(e) if path.is_empty() => tree.error = Some(format!("{e:#}")),
                    Err(e) => {
                        tracing::warn!("list {path}: {e:#}");
                        tree.expanded.remove(&path);
                    }
                }
                if again {
                    this.load(dir, path, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-list the visible directories of `dir` (all folders for `None`).
    /// Listings under collapsed folders are dropped instead: opening one
    /// lists it again, so a file change doesn't re-read hidden folders.
    fn refresh(&mut self, dir: Option<PathBuf>, cx: &mut Context<Self>) {
        let mut todo: Vec<(PathBuf, String)> = Vec::new();
        for (d, t) in self
            .trees
            .iter_mut()
            .filter(|(d, _)| dir.as_ref().is_none_or(|x| x == *d))
        {
            let visible = |p: &str| {
                p.is_empty()
                    || (t.expanded.contains(p)
                        && p.match_indices('/')
                            .all(|(i, _)| t.expanded.contains(&p[..i])))
            };
            t.children.retain(|p, _| visible(p));
            // The root too, so a failed first listing is retried.
            todo.push((d.clone(), String::new()));
            todo.extend(
                t.children
                    .keys()
                    .filter(|p| !p.is_empty())
                    .map(|p| (d.clone(), p.clone())),
            );
        }
        for (d, p) in todo {
            self.load(d, p, cx);
        }
    }

    fn schedule_refresh(&mut self, dir: Option<PathBuf>, cx: &mut Context<Self>) {
        self.refresh_targets.include(dir);
        self.refresh_seq += 1;
        let seq = self.refresh_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.refresh_seq == seq {
                    match std::mem::take(&mut this.refresh_targets) {
                        RefreshTargets::None => {}
                        RefreshTargets::All => this.refresh(None, cx),
                        RefreshTargets::Folders(folders) => {
                            for dir in folders {
                                this.refresh(Some(dir), cx);
                            }
                        }
                    }
                }
            });
        })
        .detach();
    }

    fn toggle(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(dir) = self.dir(cx) else { return };
        let tree = self.trees.entry(dir.clone()).or_default();
        if !tree.expanded.remove(&path) {
            tree.expanded.insert(path.clone());
            if !tree.children.contains_key(&path) {
                self.load(dir, path, cx);
            }
        }
        cx.notify();
    }

    fn rows(tree: &Tree) -> Vec<Row> {
        fn walk(tree: &Tree, path: &str, depth: usize, out: &mut Vec<Row>) {
            let Some(children) = tree.children.get(path) else {
                return;
            };
            for e in children {
                let expanded = e.is_dir && tree.expanded.contains(&e.path);
                out.push(Row {
                    entry: e.clone(),
                    depth,
                    expanded,
                });
                if expanded {
                    walk(tree, &e.path, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(tree, "", 0, &mut out);
        out
    }
}

impl Render for FileExplorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(dir) = self.dir(cx) else {
            return div().into_any_element();
        };
        // A failed root listing waits for Refresh (or a reconnect) instead
        // of retrying on every frame.
        let loaded = self.trees.get(&dir).is_some_and(|t| {
            t.children.contains_key("") || t.loading.contains("") || t.error.is_some()
        });
        if !loaded {
            self.load(dir.clone(), String::new(), cx);
        }
        let tree = self.trees.entry(dir.clone()).or_default();
        let rows = Self::rows(tree);
        let error = tree.error.clone();
        let loading_root = tree.loading.contains("") && !tree.children.contains_key("");
        let active = self.tabs.read(cx).active_path(cx);
        let header = h_flex()
            .flex_none()
            .h_10()
            .px_3()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(crate::ui::folder_name(&dir)),
            )
            .child(
                Button::new("files-go-to")
                    .icon(IconName::Search)
                    .ghost()
                    .small()
                    .tooltip_with_action("Go to file", &super::keys::GoToFile, None)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(super::keys::GoToFile), cx)
                    }),
            )
            .child(
                Button::new("files-collapse")
                    .icon(super::icons::collapse_all())
                    .ghost()
                    .small()
                    .tooltip("Collapse all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(d) = this.dir(cx)
                            && let Some(t) = this.trees.get_mut(&d)
                        {
                            t.expanded.clear();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("files-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .small()
                    .tooltip("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let d = this.dir(cx);
                        this.refresh(d, cx);
                    })),
            );
        let weak = cx.entity().downgrade();
        let tabs = self.tabs.downgrade();
        let list = uniform_list("file-tree", rows.len(), move |range, _, cx| {
            let theme = cx.theme();
            rows[range]
                .iter()
                .map(|row| {
                    let e = &row.entry;
                    let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
                    let selected = !e.is_dir && active.as_deref() == Some(e.path.as_str());
                    let (weak, tabs, path, is_dir) =
                        (weak.clone(), tabs.clone(), e.path.clone(), e.is_dir);
                    let icon = if e.is_dir {
                        if row.expanded {
                            super::icons::folder_open()
                        } else {
                            Icon::new(IconName::Folder)
                        }
                    } else {
                        Icon::new(IconName::File)
                    };
                    h_flex()
                        .id(SharedString::from(format!("file-row:{}", e.path)))
                        .h(px(ROW_H))
                        .w_full()
                        .pl(px(8. + row.depth as f32 * INDENT))
                        .pr_2()
                        .gap_1p5()
                        .items_center()
                        .text_sm()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |el| el.bg(theme.secondary))
                        .hover(|s| s.bg(theme.secondary.opacity(0.6)))
                        .child(div().w_4().flex_none().when(is_dir, |el| {
                            el.child(
                                Icon::new(if row.expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .text_color(theme.muted_foreground),
                            )
                        }))
                        .child(icon.small().text_color(theme.muted_foreground))
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .on_click(move |_, window, cx| {
                            if is_dir {
                                let _ = weak.update(cx, |this, cx| this.toggle(path.clone(), cx));
                            } else {
                                let _ = tabs.update(cx, |t, cx| t.open(&path, None, window, cx));
                            }
                        })
                })
                .collect()
        })
        .track_scroll(&self.scroll)
        .px_1()
        .py_1()
        .flex_1()
        .min_h_0();
        let body: AnyElement = if let Some(e) = error {
            v_flex()
                .p_4()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(format!("Could not list files: {e}"))
                .into_any_element()
        } else if loading_root {
            div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Loading…")
                .into_any_element()
        } else {
            list.into_any_element()
        };
        v_flex()
            .size_full()
            .child(header)
            .child(body)
            .into_any_element()
    }
}
