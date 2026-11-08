//! File access for the editor tabs and the Files panel. Requests go to
//! opencode's fs API; the offline demo (no sidecar) reads the local disk so
//! the UI can be exercised without opencode.

use std::path::{Path, PathBuf};

use anyhow::Result;
use futures::channel::oneshot;
use gpui_kit::Context;

use super::Controller;
use crate::api::fs::ReadResult;
use crate::api::model::FsEntry;
use crate::store::StoreEvent;

/// Where file requests go.
#[derive(Clone)]
enum Io {
    Api(crate::api::Api),
    Local,
}

impl Io {
    async fn list(&self, dir: &Path, path: &str) -> Result<Vec<FsEntry>> {
        match self {
            Io::Api(api) => api.list_dir(dir, path).await,
            Io::Local => {
                let mut out = Vec::new();
                let mut rd = tokio::fs::read_dir(dir.join(path)).await?;
                while let Some(e) = rd.next_entry().await? {
                    let name = e.file_name().to_string_lossy().into_owned();
                    out.push(FsEntry {
                        path: if path.is_empty() {
                            name
                        } else {
                            format!("{path}/{name}")
                        },
                        is_dir: e.file_type().await?.is_dir(),
                    });
                }
                out.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
                });
                Ok(out)
            }
        }
    }

    async fn read(
        &self,
        dir: &Path,
        rel: &str,
        etag: Option<&str>,
        limit: u64,
    ) -> Result<ReadResult> {
        match self {
            Io::Api(api) => api.read_file(dir, rel, etag, limit).await,
            Io::Local => {
                let path = dir.join(rel);
                let meta = match tokio::fs::metadata(&path).await {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(ReadResult::Missing);
                    }
                    Err(e) => return Err(e.into()),
                };
                // Size + mtime stand in for the server's ETag.
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                let tag = format!("\"{}-{mtime}\"", meta.len());
                if etag == Some(tag.as_str()) {
                    return Ok(ReadResult::NotModified);
                }
                let mut bytes = tokio::fs::read(&path).await?;
                let total = bytes.len() as u64;
                bytes.truncate(limit as usize);
                Ok(ReadResult::Body {
                    bytes,
                    etag: Some(tag),
                    total,
                })
            }
        }
    }

    async fn write(&self, dir: &Path, rel: &str, bytes: Vec<u8>) -> Result<()> {
        match self {
            Io::Api(api) => api.write_file(dir, rel, bytes).await,
            Io::Local => Ok(tokio::fs::write(dir.join(rel), bytes).await?),
        }
    }
}

/// Demo-mode stand-in for `/api/fs/find`: files whose path contains the
/// query's characters in order (case-insensitive), shortest paths first.
pub(super) fn find_local(dir: &Path, query: &str, limit: usize) -> Vec<FsEntry> {
    const SKIP: &[&str] = &["target", "node_modules"];
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let matches = |path: &str| {
        let mut it = q.iter().peekable();
        for c in path.to_lowercase().chars() {
            if it.peek() == Some(&&c) {
                it.next();
            }
        }
        it.peek().is_none()
    };
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    let mut seen = 0;
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(dir.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > 20_000 {
                break;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if matches(&path) {
                out.push(FsEntry {
                    path,
                    is_dir: false,
                });
            }
        }
    }
    out.sort_by_key(|e| (e.path.len(), e.path.clone()));
    out.truncate(limit);
    out
}

impl Controller {
    fn io(&self) -> Option<Io> {
        match (&self.api, self.demo) {
            (Some(api), _) => Some(Io::Api(api.clone())),
            (None, true) => Some(Io::Local),
            (None, false) => None,
        }
    }

    /// Children of `path` (`""` = the folder). `None` when opencode isn't running.
    pub fn fs_list(
        &self,
        dir: PathBuf,
        path: String,
    ) -> Option<oneshot::Receiver<Result<Vec<FsEntry>>>> {
        let io = self.io()?;
        Some(super::reads::spawn_read(&self.rt, "fs_list", async move {
            io.list(&dir, &path).await
        }))
    }

    /// Read a file (see [`crate::api::Api::read_file`]).
    pub fn fs_read(
        &self,
        dir: PathBuf,
        rel: String,
        etag: Option<String>,
    ) -> Option<oneshot::Receiver<Result<ReadResult>>> {
        let io = self.io()?;
        Some(super::reads::spawn_read(&self.rt, "fs_read", async move {
            io.read(&dir, &rel, etag.as_deref(), crate::files::MAX_EDIT_BYTES)
                .await
        }))
    }

    /// Write a file. The caller confines `rel` (see [`crate::files::rel_path`]).
    pub fn fs_write(
        &self,
        dir: PathBuf,
        rel: String,
        bytes: Vec<u8>,
    ) -> Option<oneshot::Receiver<Result<()>>> {
        let io = self.io()?;
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(io.write(&dir, &rel, bytes).await);
        });
        Some(rx)
    }

    /// Start, renew or stop the server watch on an open file. Failures only
    /// cost live reloads (tool events still revalidate), so they're logged.
    pub fn fs_watch(&self, dir: PathBuf, rel: String, on: bool) {
        let Some(api) = self.api.clone() else { return };
        self.rt.spawn(async move {
            if let Err(e) = api.watch_file(&dir, &rel, on).await {
                tracing::warn!(op = "watch_file", path = %rel, on, error = %format!("{e:#}"), "fs watch failed");
            }
        });
    }

    /// Tell open tabs that files may have changed.
    pub(super) fn files_changed(
        &mut self,
        dir: Option<PathBuf>,
        file: Option<String>,
        removed: bool,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |_, cx| {
            cx.emit(StoreEvent::FilesChanged { dir, file, removed })
        });
    }
}
