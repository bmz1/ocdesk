//! Working-tree diffs per folder: latest-wins reads and staleness.

use super::*;

impl Controller {
    /// Reload the working-tree diff of `sid`'s folder (shared by all of
    /// its sessions).
    pub fn refresh_diff(&mut self, sid: String, cx: &mut Context<Self>) {
        // An unlisted (subagent) session has no folder entry: nothing to load,
        // and no spinner may be left on.
        let Some(dir) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        self.refresh_folder_diff(dir, cx);
    }

    pub(super) fn refresh_folder_diff(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let Some(api) = self.api(cx) else { return };
        let request = self.diff_requests.bump(dir.clone());
        self.store.update(cx, |s, cx| s.begin_diff(dir.clone(), cx));
        let d = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move { api.working_diff(&d).await },
            move |this, res, cx| {
                if !this.diff_requests.is_current(&dir, request) {
                    return;
                }
                if let Ok(files) = &res {
                    this.prune_viewed(&dir, files, cx);
                }
                this.store.update(cx, |s, cx| match res {
                    Ok(files) => s.set_diff_state(dir, Some(files), None, cx),
                    Err(e) => s.set_diff_state(
                        dir,
                        None,
                        Some(format!("Could not load changes: {e:#}")),
                        cx,
                    ),
                });
            },
        );
    }

    /// A turn may have changed files in `dir`: reload its diff if a view
    /// shows it (the selected session's folder), else mark it stale so it
    /// reloads when next selected. Unseen folders aren't fetched at all.
    pub(super) fn diff_maybe_changed(&mut self, sid: &str, cx: &mut Context<Self>) {
        let s = self.store.read(cx);
        let Some(dir) = s.session(sid).map(|x| x.directory.clone()) else {
            return;
        };
        if !s.diffs.contains_key(&dir) {
            return;
        }
        if s.selected_diff_dir().as_ref() == Some(&dir) {
            self.refresh_folder_diff(dir, cx);
        } else {
            self.store.update(cx, |s, _| {
                s.diff_stale.insert(dir);
            });
        }
    }
}
