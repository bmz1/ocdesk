//! Sidebar folders (opencode locations), git worktrees and moving a session
//! between checkouts.

use super::*;

impl Controller {
    /// Add a folder (validated by the server as an opencode location).
    pub fn add_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let folder = std::fs::canonicalize(&folder).unwrap_or(folder);
        if !folder.is_dir() {
            self.notice(format!("{} is not a folder", folder.display()), cx);
            return;
        }
        let finish = |this: &mut Self, folder: PathBuf, cx: &mut Context<Self>| {
            if !this.settings.folders.contains(&folder) {
                this.settings.folders.push(folder.clone());
                this.save_settings();
            }
            let folders = this.settings.folders.clone();
            this.store.update(cx, |s, cx| {
                s.set_folders(folders, cx);
                s.collapsed.remove(&folder);
            });
            this.refresh_catalog(folder.clone(), cx);
            this.new_session(Some(folder), cx);
        };
        match self.api.clone() {
            Some(api) => {
                let f = folder.clone();
                self.run(
                    cx,
                    async move { api.location(&f).await },
                    move |this, res, cx| match res {
                        Ok(dir) => finish(this, dir, cx),
                        Err(e) => this.notice(
                            format!("opencode can't open {}: {e:#}", folder.display()),
                            cx,
                        ),
                    },
                );
            }
            None => finish(self, folder, cx),
        }
    }

    /// Hide a folder from the sidebar (its sessions are kept on the server).
    pub fn remove_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.settings.folders.retain(|f| f != &folder);
        self.settings.collapsed.retain(|f| f != &folder);
        self.save_settings();
        let folders = self.settings.folders.clone();
        let selected_in_folder = {
            let s = self.store.read(cx);
            s.selected
                .as_ref()
                .and_then(|id| s.session(id))
                .is_some_and(|s| s.directory == folder)
        };
        if selected_in_folder {
            self.select_session(None, cx);
        }
        let forgotten = self.store.update(cx, |s, cx| {
            let forgotten = s.forget_folder(&folder);
            s.set_folders(folders, cx);
            forgotten
        });
        // Worktree checkouts go with their folder: an in-flight diff or task
        // read for one must not re-create state for a directory now gone.
        for dir in &forgotten {
            self.diff_requests.forget(dir);
            self.task_requests.forget(dir);
        }
    }

    pub fn toggle_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.store
            .update(cx, |s, cx| s.toggle_collapsed(&folder, cx));
        self.settings.collapsed = self.store.read(cx).collapsed.iter().cloned().collect();
        self.save_settings();
    }

    /// Create a worktree of `folder`'s project and open a new session in it.
    pub fn create_worktree(&mut self, folder: PathBuf, name: String, cx: &mut Context<Self>) {
        let Some(project) = self.store.read(cx).projects.get(&folder).cloned() else {
            self.notice(format!("{} is not a git repository", folder.display()), cx);
            return;
        };
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move {
                let dir = api.create_worktree(&project, name.trim()).await?;
                // Register the new checkout as a location before using it.
                api.location(&dir).await?;
                Ok(dir)
            },
            move |this, res, cx| match res {
                Ok(dir) => {
                    this.refresh_folder_info(folder, cx);
                    this.new_session(Some(dir), cx);
                }
                Err(e) => this.notice(format!("Could not create the worktree: {e:#}"), cx),
            },
        );
    }

    /// Remove a worktree. Without `force`, a dirty checkout is refused;
    /// `on_needs_force` is then called so the UI can ask.
    pub fn remove_worktree(
        &mut self,
        folder: PathBuf,
        dir: PathBuf,
        force: bool,
        on_needs_force: impl FnOnce(&mut gpui_kit::App) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.store.read(cx).projects.get(&folder).cloned() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        let target = dir.clone();
        self.run(
            cx,
            async move { api.remove_worktree(&project, &target, force).await },
            move |this, res, cx| match res {
                Ok(()) => {
                    if this.store.read(cx).target_folder().as_ref() == Some(&dir) {
                        this.new_session(Some(folder.clone()), cx);
                    }
                    this.refresh_folder_info(folder, cx);
                }
                Err(e) if !force && e.is::<crate::api::WorktreeDirty>() => on_needs_force(cx),
                Err(e) => this.notice(format!("Could not remove the worktree: {e:#}"), cx),
            },
        );
    }

    /// Re-pin the selected session to another checkout of its project.
    pub fn move_session(&mut self, sid: String, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        let target = dir.clone();
        self.run(
            cx,
            async move {
                api.location(&target).await?;
                api.move_session(&sid, &target).await
            },
            move |this, res, cx| match res {
                // The server confirms with a session update; re-read the list.
                Ok(()) => {
                    this.store.update(cx, |s, cx| s.set_current_folder(dir, cx));
                    this.refresh_sessions(cx);
                }
                Err(e) => this.notice(format!("Could not move the session: {e:#}"), cx),
            },
        );
    }
}
