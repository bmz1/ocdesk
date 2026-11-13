//! ocdesk's own updates: look for a newer release, download and install it
//! when the user asks, and restart into it.
//!
//! This is app state, independent of the sidecar: none of it is fenced by a
//! server generation or connection epoch. The check is a cancellable read
//! (owned task; a newer check replaces it). The download+install is local
//! work started only by [`Controller::install_update`], which views call
//! from the user's click; it runs to completion and reports through the
//! store. Nothing is downloaded or replaced on a timer.

use std::time::{Duration, Instant};

use futures::SinkExt as _;

use super::*;
use crate::api::update::UpdateFeed;
use crate::domain::update::{Version, newer_release};
use crate::update::{self, Installed};

/// After start, so the check never competes with startup work.
const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Progress reports forwarded to the UI, at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Set to skip automatic checks whatever the saved preference says.
const NO_CHECK_ENV: &str = "OCDESK_NO_UPDATE_CHECK";

enum Progress {
    Received(u64, Option<u64>),
    /// Verified; the installed files are being replaced.
    Downloaded,
}

/// The version this binary was built as.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version is major.minor.patch")
}

impl Controller {
    /// Record how this copy is installed and start the periodic check.
    pub(super) fn start_updates(&mut self, cx: &mut Context<Self>) {
        match &self.installed {
            Ok(installed) => {
                tracing::info!(op = "update", ?installed, "in-place updates available")
            }
            Err(reason) => tracing::info!(op = "update", %reason, "updates are offered as a link"),
        }
        let manual_only = self.installed.as_ref().err().cloned();
        self.store
            .update(cx, |s, _| s.set_update_manual_only(manual_only));
        self.schedule_update_checks(cx);
    }

    /// (Re)start or stop the periodic check to match the preference.
    pub(super) fn schedule_update_checks(&mut self, cx: &mut Context<Self>) {
        // A development build is not a release: check only on request,
        // unless a test feed is configured.
        let automatic = self.settings.check_for_updates
            && !self.demo
            && std::env::var_os(NO_CHECK_ENV).is_none()
            && (!cfg!(debug_assertions) || std::env::var_os("OCDESK_UPDATE_FEED").is_some());
        if !automatic {
            self.update_poll = None;
            return;
        }
        // Replacing the task cancels the previous timer.
        self.update_poll = Some(cx.spawn(async move |this, cx| {
            let mut delay = FIRST_CHECK;
            loop {
                cx.background_executor().timer(delay).await;
                delay = CHECK_EVERY;
                if this
                    .update(cx, |this, cx| this.check_for_updates(false, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn update_feed(&mut self) -> Result<UpdateFeed> {
        if self.update_feed.is_none() {
            self.update_feed = Some(UpdateFeed::from_env()?);
        }
        Ok(self.update_feed.clone().expect("just set"))
    }

    /// Ask the release feed whether a newer version exists. `manual`: the
    /// user asked, so the answer is reported even when there is nothing new
    /// and a skipped version is offered again.
    pub fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.store.read(cx).updates.check().is_loading() && !manual {
            return;
        }
        if manual {
            self.update_settings(cx, |s| s.skipped_update = None);
        }
        self.store.update(cx, |s, cx| s.begin_update_check(cx));
        let feed = match self.update_feed() {
            Ok(feed) => feed,
            Err(e) => {
                self.update_check = None;
                self.finish_update_check(Err(e), manual, cx);
                return;
            }
        };
        let current = current_version();
        let target = self.installed.as_ref().ok().map(Installed::target);
        let reply = reads::spawn_read(&self.rt, "update_check", async move {
            let manifest = feed.manifest().await?;
            newer_release(&manifest, current, target.as_ref()).map_err(|e| anyhow!(e))
        });
        // Owned: a newer check drops this one, which cancels its read.
        self.update_check = Some(cx.spawn(async move |this, cx| {
            if let Ok(result) = reply.await {
                let _ = this.update(cx, |this, cx| this.finish_update_check(result, manual, cx));
            }
        }));
    }

    fn finish_update_check(
        &mut self,
        result: Result<Option<crate::domain::update::Release>>,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        match &result {
            Ok(Some(release)) => tracing::info!(op = "update_check", manual,
                version = %release.version, installable = release.asset.is_some(), "update available"),
            Ok(None) => tracing::info!(op = "update_check", manual, "up to date"),
            Err(e) => tracing::warn!(op = "update_check", manual, error = %format!("{e:#}"),
                "could not check for updates"),
        }
        let result = result.map_err(|e| format!("{e:#}"));
        self.store
            .update(cx, |s, cx| s.finish_update_check(result, manual, cx));
    }

    /// The user approved the offered update: download it, verify it and
    /// replace the installed files. The running process keeps working; the
    /// new version starts with [`Self::restart_into_update`] or the next
    /// launch.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let Ok(installed) = self.installed.clone() else {
            return;
        };
        let feed = match self.update_feed() {
            Ok(feed) => feed,
            Err(e) => {
                self.notice(format!("Could not update: {e:#}"), cx);
                return;
            }
        };
        let Some((version, asset)) = self.store.update(cx, |s, cx| s.begin_update_install(cx))
        else {
            return;
        };
        tracing::info!(op = "update_install", %version, file = %asset.name, "downloading update");
        let (mut tx, mut rx) = mpsc::channel(4);
        let store = self.store.clone();
        self.update_progress = Some(cx.spawn(async move |_, cx| {
            while let Some(progress) = rx.next().await {
                store.update(cx, |s, cx| match progress {
                    Progress::Received(received, total) => {
                        s.update_download_progress(received, total, cx)
                    }
                    Progress::Downloaded => s.update_downloaded(cx),
                });
            }
        }));
        let work = async move {
            let dir = tokio::task::spawn_blocking(update::fresh_staging_dir).await??;
            let download = dir.join(&asset.name);
            let mut reports = tx.clone();
            let mut last: Option<Instant> = None;
            feed.download(version, &asset, &download, |received, total| {
                let done = total == Some(received);
                if done || last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last = Some(Instant::now());
                    // Display only: a full channel just skips a report.
                    let _ = reports.try_send(Progress::Received(received, total));
                }
            })
            .await?;
            let _ = tx.send(Progress::Downloaded).await;
            tokio::task::spawn_blocking(move || {
                let result = update::install(&installed, &download);
                let _ = std::fs::remove_dir_all(&dir);
                result
            })
            .await?
        };
        // Local work that must finish whatever the sidecar does: unfenced.
        self.run(cx, work, move |this, result, cx| {
            this.update_progress = None;
            match &result {
                Ok(()) => tracing::info!(op = "update_install", %version, "update installed"),
                Err(e) => tracing::warn!(op = "update_install", %version,
                    error = %format!("{e:#}"), "update failed"),
            }
            let result = result.map_err(|e| format!("{e:#}"));
            this.store
                .update(cx, |s, cx| s.finish_update_install(result, cx));
        });
    }

    /// Don't offer this release again (a newer one is offered as usual).
    pub fn skip_update(&mut self, cx: &mut Context<Self>) {
        let Some(version) = self.store.read(cx).update_offer().map(|r| r.version) else {
            return;
        };
        self.update_settings(cx, |s| s.skipped_update = Some(version.to_string()));
    }

    pub fn dismiss_update_failure(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.dismiss_update_failure(cx));
    }

    /// Arrange for the installed update to start once this process exits.
    /// The caller quits the app when this returns true.
    pub fn restart_into_update(&mut self, cx: &mut Context<Self>) -> bool {
        use crate::domain::update::Install;
        if !matches!(self.store.read(cx).updates.install(), Install::Ready { .. }) {
            return false;
        }
        let Ok(installed) = &self.installed else {
            return false;
        };
        match update::relaunch_after_exit(installed) {
            Ok(()) => {
                tracing::info!(
                    op = "update_restart",
                    "restarting into the installed update"
                );
                true
            }
            Err(e) => {
                self.notice(
                    format!("{e:#}. Quit and reopen ocdesk to finish updating."),
                    cx,
                );
                false
            }
        }
    }
}
