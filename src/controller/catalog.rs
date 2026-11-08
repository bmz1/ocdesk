//! Per-folder models, agents, `/` menu sources, branches and worktrees.

use super::*;

impl Controller {
    pub(super) fn schedule_catalog_refresh(&mut self, cx: &mut Context<Self>) {
        let exec = cx.background_executor().clone();
        self.catalog_debounce = Some(cx.spawn(async move |this, cx| {
            exec.timer(CATALOG_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.refresh_catalogs(cx));
        }));
    }

    /// Re-read models, default model and agents for every folder.
    pub(super) fn refresh_catalogs(&mut self, cx: &mut Context<Self>) {
        for folder in self.store.read(cx).folders.clone() {
            self.refresh_catalog(folder, cx);
        }
    }

    pub(crate) fn refresh_catalog(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        self.refresh_folder_info(folder.clone(), cx);
        let dir = folder.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (models, default_model, agents, configured_agent) = futures::join!(
                    api.models(&dir),
                    api.default_model(&dir),
                    api.agents(&dir),
                    api.configured_agent(&dir)
                );
                Ok(Catalog {
                    models: models?,
                    // No default configured is not an error.
                    default_model: default_model.unwrap_or_else(|e| {
                        tracing::warn!(op = "default_model", dir = %dir.display(), error = %format!("{e:#}"), "no default model");
                        None
                    }),
                    agents: agents?,
                    configured_agent: configured_agent.unwrap_or_default(),
                })
            },
            move |this, res: Result<Catalog>, cx| match res {
                Ok(c) => this.store.update(cx, |s, cx| s.set_catalog(folder, c, cx)),
                Err(e) => tracing::warn!(
                    op = "catalog",
                    dir = %folder.display(),
                    error = %format!("{e:#}"),
                    "could not read models/agents"
                ),
            },
        );
    }

    /// The folder's `/` menu sources, branch, project and worktrees. Each
    /// part fails on its own (e.g. a folder that isn't a git repository).
    pub(super) fn refresh_folder_info(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let dir = folder.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Server),
            async move {
                let (commands, skills, mcp, vcs, project) = futures::join!(
                    api.commands(&dir),
                    api.skills(&dir),
                    api.mcp_servers(&dir),
                    api.vcs(&dir),
                    api.project_id(&dir),
                );
                let worktrees = match &project {
                    Ok(pid) => api.worktrees(pid).await.ok(),
                    Err(_) => None,
                };
                // Branches of the worktrees too, for the sidebar.
                let mut wt_vcs = Vec::new();
                for w in worktrees.iter().flatten().filter(|w| **w != dir) {
                    if let Ok(v) = api.vcs(w).await {
                        wt_vcs.push((w.clone(), v));
                    }
                }
                let warn = |what: &str, e: &anyhow::Error| {
                    tracing::debug!(op = "folder_info", dir = %dir.display(), error = %format!("{e:#}"), "{what}")
                };
                let tooling = Tooling {
                    commands: commands.unwrap_or_else(|e| {
                        warn("no commands", &e);
                        vec![]
                    }),
                    skills: skills.unwrap_or_else(|e| {
                        warn("no skills", &e);
                        vec![]
                    }),
                    mcp: mcp.unwrap_or_else(|e| {
                        warn("no MCP servers", &e);
                        vec![]
                    }),
                };
                Ok((tooling, vcs.ok(), project.ok(), worktrees, wt_vcs))
            },
            move |this, res: Result<_>, cx| {
                let Ok((tooling, vcs, project, worktrees, wt_vcs)) = res else {
                    return;
                };
                this.store.update(cx, |s, cx| {
                    s.set_tooling(folder.clone(), tooling, cx);
                    if let Some(v) = vcs {
                        s.set_vcs(folder.clone(), v, cx);
                    }
                    for (w, v) in wt_vcs {
                        s.set_vcs(w, v, cx);
                    }
                    if let (Some(pid), Some(wts)) = (project, worktrees) {
                        s.set_worktrees(folder.clone(), pid, wts, cx);
                    }
                });
            },
        );
    }

    /// Connect or disconnect an MCP server of the target folder.
    pub fn set_mcp_connected(&mut self, server: String, connect: bool, cx: &mut Context<Self>) {
        let Some(folder) = self.store.read(cx).target_folder() else {
            return;
        };
        let Some(api) = self.api(cx) else { return };
        let dir = folder.clone();
        self.run(
            cx,
            async move { api.mcp_set_connected(&dir, &server, connect).await },
            move |this, res, cx| match res {
                Ok(()) => this.refresh_folder_info(folder, cx),
                Err(e) => this.notice(format!("MCP: {e:#}"), cx),
            },
        );
    }
}
