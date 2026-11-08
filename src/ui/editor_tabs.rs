//! Editor tabs next to the chat: `[Chat] [main.rs ●] [Cargo.toml]`.
//!
//! Tabs belong to a folder (the selected session's directory), not to a
//! session, so switching sessions in one folder keeps them; another folder
//! has its own strip. Saving is explicit (Cmd/Ctrl+S): the agent edits the
//! same files, and autosave would race it.
//!
//! Disk changes (`filesystem.changed` for watched files, and every
//! successful tool call) revalidate open tabs with `If-None-Match`. A clean
//! tab follows the disk; a tab with unsaved edits is never overwritten and
//! shows a banner instead. Saving re-checks the disk first for the same
//! reason.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Position};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::fs::ReadResult;
use crate::controller::Controller;
use crate::files::{self, Content};
use crate::store::{Store, StoreEvent};

/// Renew server watches well before their 24 h expiry.
const WATCH_RENEW: Duration = Duration::from_secs(12 * 3600);

/// What the strip's Chat tab runs when clicked.
pub type OnChat = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Loading,
    Ready,
    Binary,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Conflict {
    /// The file changed on disk while the tab had unsaved edits.
    Changed {
        text: String,
        etag: Option<String>,
        crlf: bool,
    },
    /// The file was deleted on disk.
    Deleted,
}

struct Buffer {
    id: u64,
    rel: String,
    editor: Entity<EditorState>,
    status: Status,
    etag: Option<String>,
    /// What we believe is on disk (`\n` line endings).
    disk: String,
    crlf: bool,
    /// Only the first part was read: read-only.
    truncated: bool,
    dirty: bool,
    conflict: Option<Conflict>,
    /// A load, save or revalidation is in flight.
    busy: bool,
    saving: bool,
    revalidate_pending: bool,
    close_after_save: bool,
    /// Line to reveal once loaded (1-based).
    pending_line: Option<u32>,
    _sub: Subscription,
}

impl Buffer {
    fn editable(&self) -> bool {
        self.status == Status::Ready && !self.truncated
    }
}

#[derive(Default)]
struct Folder {
    tabs: Vec<Buffer>,
    /// `None` is the Chat tab.
    active: Option<usize>,
}

/// What a save found or did.
enum Saved {
    Done { etag: Option<String> },
    Changed(Conflict),
}

/// Validate the content/version pair returned after a write. A concurrent
/// writer is a conflict, never a successful save with that writer's ETag.
fn saved_readback(read: ReadResult, expected: &str) -> Result<Saved> {
    match read {
        ReadResult::Missing => Ok(Saved::Changed(Conflict::Deleted)),
        ReadResult::NotModified => Err(anyhow!("unexpected 304 while verifying saved file")),
        ReadResult::Body { bytes, etag, total } => match files::decode(&bytes, total) {
            Content::Text {
                text,
                truncated: false,
                ..
            } if text == expected => Ok(Saved::Done { etag }),
            Content::Text { text, crlf, .. } => {
                Ok(Saved::Changed(Conflict::Changed { text, etag, crlf }))
            }
            Content::Binary => Err(anyhow!("file became binary while verifying save")),
        },
    }
}

/// Await a controller reply; `None` means opencode isn't running.
async fn recv<T>(rx: Option<oneshot::Receiver<Result<T>>>) -> Result<T> {
    rx.ok_or_else(|| anyhow!("opencode is not running"))?
        .await
        .map_err(|_| anyhow!("request cancelled"))?
}

pub struct FileTabs {
    store: Entity<Store>,
    controller: Entity<Controller>,
    folders: HashMap<PathBuf, Folder>,
    /// Folders whose saved tabs were reopened this run.
    restored: HashSet<PathBuf>,
    next_id: u64,
    _renew: Task<()>,
    _subs: Vec<Subscription>,
}

impl FileTabs {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs =
            vec![cx.subscribe_in(
                &store,
                window,
                |this, _, ev: &StoreEvent, window, cx| match ev {
                    StoreEvent::SelectionChanged => {
                        this.restore(window, cx);
                        // Picking a session means "show me the conversation".
                        if let Some(f) = this.current_mut(cx) {
                            f.active = None;
                        }
                        cx.notify();
                    }
                    StoreEvent::SessionsChanged | StoreEvent::FoldersChanged => {
                        this.restore(window, cx)
                    }
                    StoreEvent::FilesChanged { dir, file, .. } => {
                        this.revalidate(dir.clone(), file.clone(), window, cx)
                    }
                    _ => {}
                },
            )];
        let renew = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH_RENEW).await;
                let ok = this.update(cx, |this, cx| {
                    for (dir, f) in &this.folders {
                        for b in &f.tabs {
                            this.controller
                                .read(cx)
                                .fs_watch(dir.clone(), b.rel.clone(), true);
                        }
                    }
                });
                if ok.is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            store,
            controller,
            folders: HashMap::new(),
            restored: HashSet::new(),
            next_id: 0,
            _renew: renew,
            _subs: subs,
        };
        this.restore(window, cx);
        this
    }

    fn current_dir(&self, cx: &App) -> Option<PathBuf> {
        self.store.read(cx).target_folder()
    }

    fn current(&self, cx: &App) -> Option<&Folder> {
        self.folders.get(&self.current_dir(cx)?)
    }

    fn current_mut(&mut self, cx: &App) -> Option<&mut Folder> {
        let dir = self.current_dir(cx)?;
        self.folders.get_mut(&dir)
    }

    fn buffer_mut(&mut self, dir: &Path, id: u64) -> Option<&mut Buffer> {
        self.folders
            .get_mut(dir)?
            .tabs
            .iter_mut()
            .find(|b| b.id == id)
    }

    /// The open file tab of the current folder, if a file tab is showing.
    pub fn file_active(&self, cx: &App) -> bool {
        self.current(cx).and_then(|f| f.active).is_some()
    }

    /// Unsaved tabs across every folder, as display paths.
    pub fn dirty_files(&self) -> Vec<String> {
        self.folders
            .values()
            .flat_map(|f| f.tabs.iter())
            .filter(|b| b.dirty)
            .map(|b| b.rel.clone())
            .collect()
    }

    /// The relative path of the tab showing in the current folder.
    pub fn active_path(&self, cx: &App) -> Option<String> {
        let f = self.current(cx)?;
        Some(f.tabs.get(f.active?)?.rel.clone())
    }

    /// Reopen the current folder's saved tabs once per run.
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir(cx) else {
            return;
        };
        if !self.restored.insert(dir.clone()) {
            return;
        }
        let saved = self
            .store
            .read(cx)
            .preferences
            .open_files
            .get(&dir)
            .cloned()
            .unwrap_or_default();
        for rel in saved {
            if let Ok(rel) = files::rel_path(&rel) {
                self.add(dir.clone(), rel, None, false, window, cx);
            }
        }
    }

    fn persist(&self, dir: &Path, cx: &mut Context<Self>) {
        let paths: Vec<String> = self
            .folders
            .get(dir)
            .map(|f| f.tabs.iter().map(|b| b.rel.clone()).collect())
            .unwrap_or_default();
        let dir = dir.to_path_buf();
        self.controller.update(cx, |c, cx| {
            c.update_settings(cx, |s| {
                if paths.is_empty() {
                    s.open_files.remove(&dir);
                } else {
                    s.open_files.insert(dir, paths);
                }
            })
        });
    }

    /// Open `path` (relative to the current folder, or absolute inside it)
    /// in a tab and show it, at `line` (1-based) if given.
    pub fn open(
        &mut self,
        path: &str,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.current_dir(cx) else {
            return;
        };
        self.restore(window, cx);
        let Some(rel) = files::relative_to(&dir, path) else {
            self.controller.update(cx, |c, cx| {
                c.show_notice(format!("{path} is outside this project"), cx)
            });
            return;
        };
        let existing = self
            .folders
            .get(&dir)
            .and_then(|f| f.tabs.iter().position(|b| b.rel == rel));
        match existing {
            Some(ix) => {
                self.activate(Some(ix), window, cx);
                if let Some(line) = line {
                    let b = &mut self.folders.get_mut(&dir).unwrap().tabs[ix];
                    if b.status == Status::Loading {
                        b.pending_line = Some(line);
                    } else {
                        reveal(&b.editor, line, window, cx);
                    }
                }
            }
            None => {
                self.add(dir.clone(), rel, line, true, window, cx);
                self.persist(&dir, cx);
            }
        }
        cx.notify();
    }

    fn add(
        &mut self,
        dir: PathBuf,
        rel: String,
        line: Option<u32>,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lang = files::language_for(&rel);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(lang)
                .line_number(true)
                .searchable(true)
                .soft_wrap(false)
        });
        editor.update(cx, |e, cx| e.set_readonly(true, cx));
        self.next_id += 1;
        let id = self.next_id;
        let d = dir.clone();
        let sub = cx.subscribe_in(&editor, window, move |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.on_edit(&d, id, cx);
            }
        });
        let folder = self.folders.entry(dir.clone()).or_default();
        folder.tabs.push(Buffer {
            id,
            rel: rel.clone(),
            editor,
            status: Status::Loading,
            etag: None,
            disk: String::new(),
            crlf: false,
            truncated: false,
            dirty: false,
            conflict: None,
            busy: false,
            saving: false,
            revalidate_pending: false,
            close_after_save: false,
            pending_line: line,
            _sub: sub,
        });
        if activate {
            folder.active = Some(folder.tabs.len() - 1);
        }
        self.controller.read(cx).fs_watch(dir.clone(), rel, true);
        self.load(dir, id, window, cx);
    }

    fn on_edit(&mut self, dir: &Path, id: u64, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(dir, id) else {
            return;
        };
        if b.status != Status::Ready {
            return;
        }
        let dirty = b.editor.read(cx).value().as_ref() != b.disk;
        if dirty != b.dirty {
            b.dirty = dirty;
            cx.notify();
        }
    }

    fn load(&mut self, dir: PathBuf, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(&dir, id) else {
            return;
        };
        b.status = Status::Loading;
        b.busy = true;
        let rel = b.rel.clone();
        let rx = self.controller.read(cx).fs_read(dir.clone(), rel, None);
        cx.spawn_in(window, async move |this, cx| {
            let res = recv(rx).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(b) = this.buffer_mut(&dir, id) else {
                    return;
                };
                b.busy = false;
                match res {
                    Ok(ReadResult::Body { bytes, etag, total }) => {
                        apply(b, files::decode(&bytes, total), etag, window, cx)
                    }
                    Ok(ReadResult::Missing) => b.status = Status::Failed("File not found".into()),
                    Ok(ReadResult::NotModified) => {
                        b.status = Status::Failed("Unexpected 304 from the server".into())
                    }
                    Err(e) => b.status = Status::Failed(format!("{e:#}")),
                }
                let pending = std::mem::take(&mut b.revalidate_pending);
                let pending_file = b.rel.clone();
                cx.notify();
                if pending {
                    this.revalidate(Some(dir.clone()), Some(pending_file), window, cx);
                }
            });
        })
        .detach();
    }

    /// Re-read open tabs that may have changed on disk.
    fn revalidate(
        &mut self,
        dir: Option<PathBuf>,
        file: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut todo = Vec::new();
        for (d, f) in &mut self.folders {
            if dir.as_ref().is_some_and(|x| x != d) {
                continue;
            }
            for b in &mut f.tabs {
                let matches = file.as_deref().is_none_or(|file| {
                    file == b.rel || files::relative_to(d, file).as_deref() == Some(&b.rel)
                });
                if !matches {
                    continue;
                }
                if b.busy || b.status == Status::Loading {
                    b.revalidate_pending = true;
                    continue;
                }
                b.busy = true;
                todo.push((d.clone(), b.id, b.rel.clone(), b.etag.clone()));
            }
        }
        for (dir, id, rel, etag) in todo {
            let rx = self.controller.read(cx).fs_read(dir.clone(), rel, etag);
            cx.spawn_in(window, async move |this, cx| {
                let res = recv(rx).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(b) = this.buffer_mut(&dir, id) else {
                        return;
                    };
                    b.busy = false;
                    match res {
                        Ok(ReadResult::NotModified) => {}
                        Ok(ReadResult::Missing) => {
                            if b.status == Status::Ready {
                                b.conflict = Some(Conflict::Deleted);
                                b.etag = None;
                            }
                        }
                        Ok(ReadResult::Body { bytes, etag, total }) => {
                            match files::decode(&bytes, total) {
                                Content::Text { text, .. }
                                    if text == b.disk && b.status == Status::Ready =>
                                {
                                    b.etag = etag;
                                    if b.conflict == Some(Conflict::Deleted) {
                                        b.conflict = None;
                                    }
                                }
                                content if !b.dirty => apply(b, content, etag, window, cx),
                                Content::Text { text, crlf, .. } => {
                                    b.conflict = Some(Conflict::Changed { text, etag, crlf })
                                }
                                Content::Binary => {
                                    b.conflict = Some(Conflict::Changed {
                                        text: String::new(),
                                        etag,
                                        crlf: false,
                                    })
                                }
                            }
                        }
                        Err(e) => tracing::warn!("revalidate {}: {e:#}", b.rel),
                    }
                    let pending = std::mem::take(&mut b.revalidate_pending);
                    let pending_file = b.rel.clone();
                    cx.notify();
                    if pending {
                        this.revalidate(Some(dir.clone()), Some(pending_file), window, cx);
                    }
                });
            })
            .detach();
        }
    }

    /// Save the tab showing in the current folder (Cmd/Ctrl+S).
    pub fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir(cx) else {
            return;
        };
        let Some(id) = self
            .current(cx)
            .and_then(|f| f.tabs.get(f.active?))
            .map(|b| b.id)
        else {
            return;
        };
        self.save(dir, id, window, cx);
    }

    fn save(&mut self, dir: PathBuf, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(&dir, id) else {
            return;
        };
        if !b.editable() || b.busy {
            return;
        }
        let deleted = b.conflict == Some(Conflict::Deleted);
        if matches!(b.conflict, Some(Conflict::Changed { .. })) || (!b.dirty && !deleted) {
            // Resolve the banner first; nothing to save otherwise.
            if b.close_after_save && !b.dirty {
                self.remove(&dir, id, cx);
            }
            return;
        }
        let text = b.editor.read(cx).value().to_string();
        let bytes = files::encode(&text, b.crlf);
        let (rel, etag, disk) = (b.rel.clone(), b.etag.clone(), b.disk.clone());
        b.busy = true;
        b.saving = true;
        cx.notify();
        let controller = self.controller.clone();
        cx.spawn_in(window, async move |this, cx| {
            let fut = async {
                // Check contents even when the server supplied no ETag.
                {
                    let rx =
                        controller.update(cx, |c, _| c.fs_read(dir.clone(), rel.clone(), etag));
                    match recv(rx).await? {
                        ReadResult::NotModified => {}
                        ReadResult::Missing if deleted => {}
                        ReadResult::Missing => return Ok(Saved::Changed(Conflict::Deleted)),
                        ReadResult::Body { bytes, etag, total } => {
                            match files::decode(&bytes, total) {
                                Content::Text {
                                    text,
                                    truncated: false,
                                    ..
                                } if text == disk => {}
                                Content::Text { text, crlf, .. } => {
                                    return Ok(Saved::Changed(Conflict::Changed {
                                        text,
                                        etag,
                                        crlf,
                                    }));
                                }
                                Content::Binary => {
                                    return Ok(Saved::Changed(Conflict::Changed {
                                        text: String::new(),
                                        etag,
                                        crlf: false,
                                    }));
                                }
                            }
                        }
                    }
                }
                let rx = controller.update(cx, |c, _| c.fs_write(dir.clone(), rel.clone(), bytes));
                recv(rx).await?;
                // Never attach a version to content it did not describe.
                let rx = controller.update(cx, |c, _| c.fs_read(dir.clone(), rel.clone(), None));
                saved_readback(recv(rx).await?, &text)
            };
            let res = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(b) = this.buffer_mut(&dir, id) else {
                    return;
                };
                b.busy = false;
                b.saving = false;
                let close = std::mem::take(&mut b.close_after_save);
                let pending = std::mem::take(&mut b.revalidate_pending);
                let pending_file = b.rel.clone();
                match res {
                    Ok(Saved::Done { etag }) => {
                        b.disk = text;
                        b.etag = etag;
                        b.conflict = None;
                        b.dirty = b.editor.read(cx).value().as_ref() != b.disk;
                        if close && !b.dirty {
                            this.remove(&dir, id, cx);
                        }
                    }
                    Ok(Saved::Changed(conflict)) => b.conflict = Some(conflict),
                    Err(e) => {
                        let msg = format!("Could not save {}: {e:#}", b.rel);
                        this.controller.update(cx, |c, cx| c.show_notice(msg, cx));
                    }
                }
                cx.notify();
                if pending {
                    this.revalidate(Some(dir.clone()), Some(pending_file), window, cx);
                }
            });
        })
        .detach();
    }

    /// Show `ix` (`None` = Chat) in the current folder and focus it.
    pub fn activate(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.current_mut(cx) else {
            return;
        };
        let ix = ix.filter(|i| *i < f.tabs.len());
        f.active = ix;
        let shown = ix.map(|i| {
            let b = &f.tabs[i];
            (b.editor.read(cx).focus_handle(cx), b.rel.clone())
        });
        if let Some((focus, rel)) = shown {
            focus.focus(window, cx);
            // Events can be missed (no watch, reconnects): check on show.
            let dir = self.current_dir(cx);
            self.revalidate(dir, Some(rel), window, cx);
        }
        cx.notify();
    }

    /// Re-check every open tab against the disk (e.g. on window focus).
    pub fn revalidate_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revalidate(None, None, window, cx);
    }

    /// Next (`+1`) or previous (`-1`) tab, Chat included, wrapping.
    pub fn cycle(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.current(cx) else {
            return;
        };
        let n = f.tabs.len() as isize + 1;
        if n == 1 {
            return;
        }
        let pos = f.active.map_or(0, |i| i as isize + 1);
        let next = (pos + delta).rem_euclid(n);
        self.activate((next > 0).then(|| next as usize - 1), window, cx);
    }

    /// Close the tab showing in the current folder (Cmd/Ctrl+W).
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir(cx) else {
            return;
        };
        if let Some(id) = self
            .current(cx)
            .and_then(|f| f.tabs.get(f.active?))
            .map(|b| b.id)
        {
            self.close(dir, id, window, cx);
        }
    }

    /// Close a tab, asking first when it has unsaved edits.
    fn close(&mut self, dir: PathBuf, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(&dir, id) else {
            return;
        };
        if !b.dirty {
            self.remove(&dir, id, cx);
            return;
        }
        let name = files::file_name(&b.rel).to_string();
        let can_save = b.editable() && b.conflict.is_none();
        let answers: &[&str] = if can_save {
            &["Save", "Don't Save", "Cancel"]
        } else {
            &["Don't Save", "Cancel"]
        };
        let rx = window.prompt(
            PromptLevel::Warning,
            &format!("Save changes to {name}?"),
            Some("Your changes will be lost if you don't save them."),
            answers,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = rx.await else { return };
            let choice = if can_save { choice } else { choice + 1 };
            let _ = this.update_in(cx, |this, window, cx| match choice {
                0 => {
                    if let Some(b) = this.buffer_mut(&dir, id) {
                        b.close_after_save = true;
                    }
                    this.save(dir.clone(), id, window, cx);
                }
                1 => this.remove(&dir, id, cx),
                _ => {}
            });
        })
        .detach();
    }

    fn remove(&mut self, dir: &Path, id: u64, cx: &mut Context<Self>) {
        let Some(f) = self.folders.get_mut(dir) else {
            return;
        };
        let Some(ix) = f.tabs.iter().position(|b| b.id == id) else {
            return;
        };
        let b = f.tabs.remove(ix);
        f.active = match f.active {
            Some(a) if a == ix => {
                if f.tabs.is_empty() {
                    None
                } else {
                    Some(ix.saturating_sub(1))
                }
            }
            Some(a) if a > ix => Some(a - 1),
            a => a,
        };
        self.controller
            .read(cx)
            .fs_watch(dir.to_path_buf(), b.rel, false);
        self.persist(dir, cx);
        cx.notify();
    }

    /// Conflict banner: take the disk version, dropping unsaved edits.
    fn reload(&mut self, dir: PathBuf, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(&dir, id) else {
            return;
        };
        match b.conflict.take() {
            Some(Conflict::Changed { .. }) | None => self.load(dir, id, window, cx),
            Some(Conflict::Deleted) => self.remove(&dir, id, cx),
        }
        cx.notify();
    }

    /// Conflict banner: keep the tab's text; the next save replaces the
    /// disk version (or recreates a deleted file).
    fn keep_mine(&mut self, dir: PathBuf, id: u64, cx: &mut Context<Self>) {
        let Some(b) = self.buffer_mut(&dir, id) else {
            return;
        };
        match b.conflict.take() {
            Some(Conflict::Changed { text, etag, crlf }) => {
                b.disk = text;
                b.etag = etag;
                b.crlf = crlf;
                b.dirty = b.editor.read(cx).value().as_ref() != b.disk;
            }
            Some(Conflict::Deleted) => {
                // Nothing on disk to check against: the next save recreates it.
                b.disk.clear();
                b.etag = None;
                b.dirty = true;
            }
            None => {}
        }
        cx.notify();
    }

    /// The tab strip above the chat/editor, or `None` without file tabs.
    pub fn render_strip(this: &Entity<Self>, on_chat: OnChat, cx: &App) -> Option<AnyElement> {
        let me = this.read(cx);
        let f = me.current(cx)?;
        if f.tabs.is_empty() {
            return None;
        }
        let dir = me.current_dir(cx)?;
        let theme = cx.theme();
        let tab = |id: ElementId, active: bool| {
            h_flex()
                .id(id)
                .h_full()
                .flex_none()
                .items_center()
                .gap_1p5()
                .pl_3()
                .pr_1()
                .text_sm()
                .border_b_2()
                .cursor_pointer()
                .border_color(if active {
                    theme.primary
                } else {
                    gpui_kit::transparent_black()
                })
                .text_color(if active {
                    theme.foreground
                } else {
                    theme.muted_foreground
                })
                .hover(|s| s.bg(theme.secondary.opacity(0.6)))
        };
        let weak = this.downgrade();
        let chat = tab("tab-chat".into(), f.active.is_none())
            .pr_3()
            .child(Icon::new(crate::ui::icons::message_square()).small())
            .child("Chat")
            .on_click(move |_, window, cx| on_chat(window, cx));
        let file_tabs = f.tabs.iter().enumerate().map(|(ix, b)| {
            let (w1, w2) = (weak.clone(), weak.clone());
            let (id, d) = (b.id, dir.clone());
            let path = b.rel.clone();
            let mark = if b.dirty || b.saving { "●" } else { "×" };
            tab(("tab-file", b.id as usize).into(), f.active == Some(ix))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(path.clone()).build(window, cx)
                })
                .on_click(move |_, window, cx| {
                    let _ = w1.update(cx, |t, cx| t.activate(Some(ix), window, cx));
                })
                .child(Icon::new(IconName::File).small())
                .child(files::file_name(&b.rel).to_string())
                .when(b.conflict.is_some(), |el| {
                    el.child(
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(theme.warning),
                    )
                })
                .child(
                    div()
                        .id(("tab-close", b.id as usize))
                        .size_5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_xs()
                        .hover(|s| s.bg(theme.secondary))
                        .child(mark)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            let d = d.clone();
                            let _ = w2.update(cx, |t, cx| t.close(d, id, window, cx));
                        }),
                )
        });
        Some(
            h_flex()
                .id("file-tabs")
                .h_9()
                .flex_none()
                .w_full()
                .px_2()
                .border_b_1()
                .border_color(theme.border)
                .overflow_x_scroll()
                .child(chat)
                .children(file_tabs)
                .into_any_element(),
        )
    }

    fn render_banner(&self, dir: &Path, b: &Buffer, cx: &mut Context<Self>) -> Option<Div> {
        let theme = cx.theme();
        let bar = |text: String, color: Hsla| {
            h_flex()
                .flex_none()
                .w_full()
                .px_4()
                .py_1p5()
                .gap_2()
                .items_center()
                .text_sm()
                .bg(color.opacity(0.12))
                .border_b_1()
                .border_color(color.opacity(0.4))
                .child(Icon::new(IconName::TriangleAlert).small().text_color(color))
                .child(div().flex_1().min_w_0().child(text))
        };
        let (id, d) = (b.id, dir.to_path_buf());
        let button =
            |key: &'static str,
             label: &'static str,
             cx: &mut Context<Self>,
             f: fn(&mut Self, PathBuf, u64, &mut Window, &mut Context<Self>)| {
                let d = d.clone();
                Button::new(key).label(label).small().outline().on_click(
                    cx.listener(move |this, _, window, cx| f(this, d.clone(), id, window, cx)),
                )
            };
        match &b.conflict {
            Some(Conflict::Changed { .. }) => Some(
                bar(
                    "This file changed on disk while you had unsaved edits.".into(),
                    theme.warning,
                )
                .child(button(
                    "conflict-reload",
                    "Reload (discard mine)",
                    cx,
                    Self::reload,
                ))
                .child(button(
                    "conflict-keep",
                    "Keep mine",
                    cx,
                    |t, d, id, _, cx| t.keep_mine(d, id, cx),
                )),
            ),
            Some(Conflict::Deleted) => Some(
                bar("This file was deleted on disk.".into(), theme.warning)
                    .child(button("deleted-close", "Close tab", cx, Self::reload))
                    .child(button(
                        "deleted-keep",
                        "Keep (save recreates it)",
                        cx,
                        |t, d, id, _, cx| t.keep_mine(d, id, cx),
                    )),
            ),
            None if b.truncated => Some(bar(
                format!(
                    "Large file: showing the first {} MB, read-only.",
                    files::MAX_EDIT_BYTES / (1024 * 1024)
                ),
                theme.muted_foreground,
            )),
            None => None,
        }
    }
}

/// Load `content` into the tab as the new disk state.
fn apply(
    b: &mut Buffer,
    content: Content,
    etag: Option<String>,
    window: &mut Window,
    cx: &mut Context<FileTabs>,
) {
    b.etag = etag;
    b.conflict = None;
    b.dirty = false;
    match content {
        Content::Text {
            text,
            crlf,
            truncated,
        } => {
            let line = b.pending_line.take();
            b.editor.update(cx, |e, cx| {
                // Keep the view where it was when the disk version reloads.
                let scroll = e.scroll_offset();
                e.set_value(text.clone(), window, cx);
                e.set_readonly(truncated, cx);
                e.set_scroll_offset(scroll, cx);
            });
            if let Some(line) = line {
                reveal(&b.editor, line, window, cx);
            }
            b.disk = text;
            b.crlf = crlf;
            b.truncated = truncated;
            b.status = Status::Ready;
        }
        Content::Binary => {
            b.disk.clear();
            b.status = Status::Binary;
        }
    }
}

fn reveal(editor: &Entity<EditorState>, line: u32, window: &mut Window, cx: &mut App) {
    editor.update(cx, |e, cx| {
        e.set_cursor_position(
            Position {
                line: line.saturating_sub(1),
                character: 0,
            },
            window,
            cx,
        )
    });
}

impl Render for FileTabs {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dir) = self.current_dir(cx) else {
            return div().into_any_element();
        };
        let Some(b) = self.current(cx).and_then(|f| f.tabs.get(f.active?)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let banner = self.render_banner(&dir, b, cx);
        let (id, d) = (b.id, dir.clone());
        let body: AnyElement = match &b.status {
            Status::Ready => div()
                .flex_1()
                .min_h_0()
                .child(Editor::new(&b.editor).bordered(false).h_full())
                .into_any_element(),
            Status::Loading => centered("Loading…", &theme).into_any_element(),
            Status::Binary => {
                let rel = b.rel.clone();
                centered("Binary file: not shown.", &theme)
                    .child(
                        Button::new("open-external")
                            .label("Open in external editor")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let rel = rel.clone();
                                this.controller.update(cx, |c, cx| c.open_file(rel, cx));
                            })),
                    )
                    .into_any_element()
            }
            Status::Failed(e) => centered(&format!("Could not open {}: {e}", b.rel), &theme)
                .child(
                    Button::new("retry-load")
                        .label("Retry")
                        .small()
                        .outline()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.load(d.clone(), id, window, cx)
                        })),
                )
                .into_any_element(),
        };
        let state = if b.saving {
            "Saving…"
        } else if b.truncated || b.status == Status::Binary {
            "Read-only"
        } else if b.dirty {
            "Unsaved"
        } else {
            "Saved"
        };
        let status = h_flex()
            .flex_none()
            .w_full()
            .h_7()
            .px_4()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(div().flex_1().min_w_0().truncate().child(b.rel.clone()))
            .child(files::language_for(&b.rel))
            .child(if b.crlf { "CRLF" } else { "LF" })
            .child(state);
        v_flex()
            .size_full()
            .bg(theme.background)
            .children(banner)
            .child(body)
            .child(status)
            .into_any_element()
    }
}

fn centered(text: &str, theme: &gpui_kit::component::Theme) -> Div {
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap_3()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Conflict, Saved, saved_readback};
    use crate::api::fs::ReadResult;

    #[test]
    fn concurrent_write_during_save_is_a_conflict() {
        let result = saved_readback(
            ReadResult::Body {
                bytes: b"agent edit".to_vec(),
                etag: Some("agent-version".into()),
                total: 10,
            },
            "user edit",
        )
        .unwrap();
        assert!(
            matches!(result, Saved::Changed(Conflict::Changed { text, etag, .. })
            if text == "agent edit" && etag.as_deref() == Some("agent-version"))
        );
    }

    #[test]
    fn only_matching_content_accepts_the_readback_version() {
        let result = saved_readback(
            ReadResult::Body {
                bytes: b"user edit\r\n".to_vec(),
                etag: Some("user-version".into()),
                total: 11,
            },
            "user edit\n",
        )
        .unwrap();
        assert!(matches!(result, Saved::Done { etag } if etag.as_deref() == Some("user-version")));
        assert!(saved_readback(ReadResult::NotModified, "user edit").is_err());
        assert!(matches!(
            saved_readback(ReadResult::Missing, "user edit").unwrap(),
            Saved::Changed(Conflict::Deleted)
        ));
    }
}
