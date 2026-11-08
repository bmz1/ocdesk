//! Background tasks: subagent sessions and server shells.

use super::*;

impl Controller {
    /// Re-read a session's subagent sessions and its folder's shells.
    pub fn refresh_tasks(&mut self, sid: String, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else { return };
        let Some(dir) = self
            .store
            .read(cx)
            .session(&sid)
            .map(|s| s.directory.clone())
        else {
            return;
        };
        let request = self.task_requests.bump(dir.clone());
        let revision = self.store.read(cx).tasks_revision;
        let target = sid.clone();
        let folder = dir.clone();
        self.run_on(
            cx,
            self.issued(RequestScope::Connection),
            async move {
                let (kids, shells) = futures::join!(api.children(&sid), api.shells(&dir));
                Ok((kids?, shells?))
            },
            move |this, res, cx| {
                if !this.task_requests.is_current(&folder, request) {
                    return;
                }
                if this.store.read(cx).tasks_revision != revision {
                    this.refresh_tasks(target, cx);
                    return;
                }
                match res {
                    Ok((kids, shells)) => this.store.update(cx, |s, cx| {
                        s.set_children(&target, kids, cx);
                        s.set_shells(&folder, shells, cx);
                    }),
                    Err(e) => tracing::warn!(op = "tasks", session_id = %target, error = %format!("{e:#}"), "could not read tasks"),
                }
            },
        );
    }

    /// Load a subagent's transcript (it isn't in the sidebar, so nothing
    /// else would); live events keep it current afterwards.
    pub fn load_child(&mut self, child: String, cx: &mut Context<Self>) {
        let loaded = self
            .store
            .read(cx)
            .conversations
            .get(&child)
            .is_some_and(|c| c.loaded());
        if !loaded {
            self.load_messages(child, cx);
        }
    }

    /// The tail of a shell's output. `None` when opencode isn't running.
    pub fn shell_output(
        &self,
        dir: PathBuf,
        shell_id: String,
    ) -> Option<oneshot::Receiver<Result<String>>> {
        let api = self.api.clone()?;
        Some(reads::spawn_read(&self.rt, "shell_output", async move {
            api.shell_output(&dir, &shell_id).await
        }))
    }

    pub fn kill_shell(&mut self, dir: PathBuf, shell_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else { return };
        self.run(
            cx,
            async move { api.kill_shell(&dir, &shell_id).await },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notice(format!("Could not stop the command: {e:#}"), cx);
                }
            },
        );
    }
}
