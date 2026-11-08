//! Folders and what they offer: catalogs and draft choices, branches,
//! worktrees, `/` menu sources and working-tree diffs.

use super::*;

impl Store {
    pub fn set_catalog(&mut self, folder: PathBuf, catalog: Catalog, cx: &mut Context<Self>) {
        if self.catalogs.get(&folder) != Some(&catalog) {
            self.catalogs.insert(folder, catalog);
            cx.emit(StoreEvent::ChoicesChanged);
            cx.notify();
        }
    }

    /// The catalog of the folder the composer targets.
    pub fn target_catalog(&self) -> Option<&Catalog> {
        self.catalogs.get(&self.catalog_folder()?)
    }

    /// The sidebar folder whose catalog and `/` menu apply to the target
    /// folder (a worktree uses its repository's).
    pub fn catalog_folder(&self) -> Option<PathBuf> {
        let dir = self.target_folder()?;
        Some(self.folder_of(&dir).unwrap_or(dir))
    }

    /// The model the next prompt runs with: the selected session's, else the
    /// draft pick, else the folder default.
    pub fn effective_model(&self) -> Option<ModelRef> {
        let session = self.selected.as_ref().and_then(|id| self.session(id));
        match session {
            Some(s) => s.model.clone(),
            None => self.draft_model.clone(),
        }
        .or_else(|| self.target_catalog()?.default_model.clone())
    }

    /// The agent the next prompt runs with (same precedence as the model).
    pub fn effective_agent(&self) -> Option<String> {
        let session = self.selected.as_ref().and_then(|id| self.session(id));
        match session {
            Some(s) => s.agent.clone(),
            None => self.draft_agent.clone(),
        }
        .or_else(|| Some(self.target_catalog()?.default_agent()?.id.clone()))
    }

    pub fn set_draft_choice(
        &mut self,
        model: Option<ModelRef>,
        agent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if model.is_some() {
            self.draft_model = model;
        }
        if agent.is_some() {
            self.draft_agent = agent;
        }
        cx.emit(StoreEvent::ChoicesChanged);
        cx.notify();
    }

    /// Drop everything kept about a folder that was removed from the
    /// sidebar, including its worktree checkouts. Returns every directory
    /// forgotten, so request owners can invalidate their reads for them too.
    #[must_use]
    pub(crate) fn forget_folder(&mut self, folder: &Path) -> Vec<PathBuf> {
        let mut dirs = self.worktrees.remove(folder).unwrap_or_default();
        dirs.push(folder.to_path_buf());
        self.projects.remove(folder);
        for dir in &dirs {
            self.pull_requests.remove(dir);
            self.vcs.remove(dir);
            self.tooling.remove(dir);
            self.catalogs.remove(dir);
            self.diffs.remove(dir);
            self.diff_status.remove(dir);
            self.diff_stale.remove(dir);
            self.shell_snapshots.remove(dir);
        }
        dirs
    }

    /// The folder whose working-tree diff the selected session shows.
    pub fn selected_diff_dir(&self) -> Option<PathBuf> {
        self.session(self.selected.as_deref()?)
            .map(|s| s.directory.clone())
    }

    /// The selected session's folder diff, if loaded.
    pub fn selected_diff(&self) -> Option<&Vec<FileDiff>> {
        self.diffs.get(&self.selected_diff_dir()?)
    }

    /// A folder's diff starts loading. Diffs are per folder (opencode's
    /// working-tree diff), shared by every session in it.
    pub fn begin_diff(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let changed = self.diff_status.begin(dir.clone());
        self.diff_stale.remove(&dir);
        if changed {
            cx.emit(StoreEvent::DiffsChanged { dir });
            cx.notify();
        }
    }

    /// A folder's diff finished loading. Emits only when what the Changes
    /// panel shows changed: it is the one heavy reader.
    pub fn set_diff_state(
        &mut self,
        dir: PathBuf,
        diffs: Option<Vec<FileDiff>>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.diff_status.finish(dir.clone(), error);
        if let Some(diffs) = diffs {
            changed |= self.diffs.get(&dir) != Some(&diffs);
            self.diffs.insert(dir.clone(), diffs);
        }
        if changed {
            cx.emit(StoreEvent::DiffsChanged { dir });
            cx.notify();
        }
    }

    pub fn set_vcs(&mut self, dir: PathBuf, info: VcsInfo, cx: &mut Context<Self>) {
        self.vcs.insert(dir, info);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    pub fn set_worktrees(
        &mut self,
        folder: PathBuf,
        project_id: String,
        worktrees: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.projects.insert(folder.clone(), project_id);
        // The main checkout is the folder itself.
        let extra: Vec<PathBuf> = worktrees.into_iter().filter(|w| *w != folder).collect();
        self.worktrees.insert(folder, extra);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    pub fn set_tooling(&mut self, folder: PathBuf, tooling: Tooling, cx: &mut Context<Self>) {
        self.tooling.insert(folder, tooling);
        cx.emit(StoreEvent::FolderInfoChanged);
        cx.notify();
    }

    /// The sidebar folder a directory belongs to: itself, or the folder
    /// whose worktree it is.
    pub fn folder_of(&self, dir: &Path) -> Option<PathBuf> {
        if self.folders.iter().any(|f| f == dir) {
            return Some(dir.to_path_buf());
        }
        self.worktrees
            .iter()
            .find(|(_, wts)| wts.iter().any(|w| w == dir))
            .map(|(f, _)| f.clone())
    }

    /// The folder a new session goes into: the selected session's folder,
    /// else the current folder, else the first folder.
    pub fn target_folder(&self) -> Option<PathBuf> {
        self.selected
            .as_ref()
            .and_then(|id| self.session(id))
            .map(|s| s.directory.clone())
            .or_else(|| self.current_folder.clone())
            .or_else(|| self.folders.first().cloned())
    }

    pub fn set_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.folders = folders;
        if self
            .current_folder
            .as_ref()
            .is_none_or(|c| !self.folders.contains(c))
        {
            self.current_folder = self.folders.first().cloned();
        }
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }

    pub fn set_current_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.current_folder = Some(folder);
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }

    pub fn toggle_collapsed(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_path_buf());
        }
        cx.emit(StoreEvent::FoldersChanged);
        cx.notify();
    }
}
