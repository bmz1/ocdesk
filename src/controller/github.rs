//! Pull request status through the `gh` CLI: availability check at start,
//! a light poll of the current checkout's PR, failure logs, and prompts
//! that hand CI failures and review comments to the agent.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::Context;

use super::Controller;
use crate::api::model::PromptInput;
use crate::github::{self, Check, CheckState, PullRequest};
use crate::store::{CheckLog, StoreEvent};

const TICK: Duration = Duration::from_secs(5);
/// Re-fetch while checks run.
const PENDING_EVERY: Duration = Duration::from_secs(20);
/// Re-fetch otherwise (new comments, a new push).
const IDLE_EVERY: Duration = Duration::from_secs(120);

impl Controller {
    /// Look for `gh`; if it's there, start polling the current checkout's PR.
    pub(super) fn start_github(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            async { anyhow::Ok(github::available().await) },
            |this, res, cx| {
                if !res.unwrap_or(false) {
                    return;
                }
                this.store.update(cx, |s, cx| {
                    s.gh_available = true;
                    cx.notify();
                });
                this.poll_pr(cx);
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(TICK).await;
                        if this.update(cx, |this, cx| this.poll_pr(cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            },
        );
    }

    fn poll_pr(&mut self, cx: &mut Context<Self>) {
        let s = self.store.read(cx);
        if !s.window_active || s.selected.is_none() {
            return;
        }
        let Some(dir) = s.target_folder() else { return };
        let due = match s.pull_requests.get(&dir) {
            None => true,
            Some(e) if e.loading => false,
            Some(e) => {
                let pending = matches!(&e.pr, Some(Some(pr))
                    if pr.state == "OPEN" && pr.count(CheckState::Pending) > 0);
                let every = if pending { PENDING_EVERY } else { IDLE_EVERY };
                e.fetched.is_none_or(|t| t.elapsed() >= every)
            }
        };
        if due {
            self.refresh_pr(dir, cx);
        }
    }

    /// Fetch `dir`'s pull request now.
    pub fn refresh_pr(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let s = self.store.read(cx);
        if !s.gh_available || s.pull_requests.get(&dir).is_some_and(|e| e.loading) {
            return;
        }
        // Only the panel shows fetch progress; the header chip doesn't.
        self.store.update(cx, |s, cx| {
            s.pull_requests.entry(dir.clone()).or_default().loading = true;
            cx.emit(StoreEvent::PullRequestChanged);
        });
        let d = dir.clone();
        let dir_key = dir.clone();
        self.run(
            cx,
            async move { github::pull_request(&d).await },
            move |this, res, cx| {
                this.store.update(cx, |s, cx| {
                    let e = s.pull_requests.entry(dir).or_default();
                    e.loading = false;
                    e.fetched = Some(Instant::now());
                    let before = (e.pr.clone(), e.error.clone());
                    match res {
                        Ok(pr) => {
                            // A new push: the old failure logs no longer apply.
                            let stale: Vec<String> = match (&e.pr, &pr) {
                                (Some(Some(old)), new)
                                    if new.as_ref().map(|p| &p.head_sha) != Some(&old.head_sha) =>
                                {
                                    old.checks.iter().map(|c| c.link.clone()).collect()
                                }
                                _ => Vec::new(),
                            };
                            e.pr = Some(pr);
                            e.error = None;
                            for link in stale {
                                s.check_logs.remove(&link);
                            }
                        }
                        Err(err) => e.error = Some(format!("{err:#}")),
                    }
                    let e = &s.pull_requests[&dir_key];
                    cx.emit(StoreEvent::PullRequestChanged);
                    // A poll that found nothing new redraws only the panel.
                    if (e.pr.clone(), e.error.clone()) != before {
                        cx.notify();
                    }
                });
            },
        );
    }

    /// Fetch the log excerpt of a failed check (once).
    pub fn load_check_log(&mut self, dir: PathBuf, check: Check, cx: &mut Context<Self>) {
        if self.store.read(cx).check_logs.contains_key(&check.link) {
            return;
        }
        let link = check.link.clone();
        self.store.update(cx, |s, cx| {
            s.check_logs.insert(link.clone(), CheckLog::Loading);
            cx.emit(StoreEvent::PullRequestChanged);
        });
        self.run(
            cx,
            async move { github::failure_log(&dir, &check).await },
            move |this, res, cx| {
                this.store.update(cx, |s, cx| {
                    s.check_logs.insert(
                        link,
                        match res {
                            Ok(log) => CheckLog::Ready(log),
                            Err(e) => CheckLog::Failed(format!("{e:#}")),
                        },
                    );
                    cx.emit(StoreEvent::PullRequestChanged);
                });
            },
        );
    }

    /// Ask the agent to fix `checks`, with their log excerpts (fetched as needed).
    pub fn send_ci_failures(
        &mut self,
        dir: PathBuf,
        pr: PullRequest,
        checks: Vec<Check>,
        cx: &mut Context<Self>,
    ) {
        let known: Vec<Option<String>> = {
            let s = self.store.read(cx);
            checks
                .iter()
                .map(|c| match s.check_logs.get(&c.link) {
                    Some(CheckLog::Ready(l)) => Some(l.clone()),
                    _ => None,
                })
                .collect()
        };
        self.run(
            cx,
            async move {
                let mut failures = Vec::new();
                for (check, log) in checks.into_iter().zip(known) {
                    let log = match log {
                        Some(l) => l,
                        None => github::failure_log(&dir, &check).await.unwrap_or_default(),
                    };
                    failures.push((check, log));
                }
                anyhow::Ok(github::ci_prompt(&pr, &failures))
            },
            |this, res, cx| match res {
                Ok(text) => this.send_prompt(
                    PromptInput {
                        text,
                        ..Default::default()
                    },
                    cx,
                ),
                Err(e) => this.notice(format!("Could not read the CI logs: {e:#}"), cx),
            },
        );
    }
}
