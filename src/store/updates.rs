//! App update state: the latest-release check and the approved install.
//!
//! Two independent machines. The check is a re-fetchable read
//! (`FetchStatus` beside the cached answer), repeated on a timer. The install
//! (`domain::update::Install`) starts only from the user's explicit request
//! and belongs to the app process, not to a sidecar or a connection: nothing
//! here is abandoned on reconnect or restart of opencode.

use gpui_kit::Context;

use super::{Store, StoreEvent};
use crate::domain::fetch::FetchStatus;
use crate::domain::update::{Asset, Install, Release, Version};

#[derive(Default)]
pub(crate) struct Updates {
    check: FetchStatus,
    /// A check has succeeded since start: `latest: None` means "up to date".
    checked: bool,
    /// The newest release, if newer than the running version. Kept across
    /// failed re-checks.
    latest: Option<Release>,
    install: Install,
    /// Why this copy can't replace itself (a source build, a system
    /// package): updates are then offered as a link to the release.
    manual_only: Option<String>,
}

impl Updates {
    pub(crate) fn check(&self) -> &FetchStatus {
        &self.check
    }
    pub(crate) fn checked(&self) -> bool {
        self.checked
    }
    pub(crate) fn latest(&self) -> Option<&Release> {
        self.latest.as_ref()
    }
    pub(crate) fn install(&self) -> &Install {
        &self.install
    }
    pub(crate) fn manual_only(&self) -> Option<&str> {
        self.manual_only.as_deref()
    }
}

impl Store {
    /// The release to offer in the sidebar: the latest one, unless the user
    /// chose to skip exactly that version.
    pub(crate) fn update_offer(&self) -> Option<&Release> {
        let release = self.updates.latest.as_ref()?;
        let skipped = self.preferences.skipped_update.as_deref();
        (skipped.and_then(Version::parse) != Some(release.version)).then_some(release)
    }

    pub(crate) fn set_update_manual_only(&mut self, reason: Option<String>) {
        self.updates.manual_only = reason;
    }

    pub(crate) fn begin_update_check(&mut self, cx: &mut Context<Self>) {
        self.updates.check.begin();
        cx.emit(StoreEvent::UpdateChanged);
        cx.notify();
    }

    /// The check ended. A failure keeps the last known release.
    pub(crate) fn finish_update_check(
        &mut self,
        result: Result<Option<Release>, String>,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(latest) => {
                self.updates.latest = latest;
                self.updates.checked = true;
                self.updates.check.finish(None);
            }
            Err(error) => self.updates.check.finish(Some(error)),
        }
        cx.emit(StoreEvent::UpdateChanged);
        cx.emit(StoreEvent::UpdateChecked { manual });
        cx.notify();
    }

    /// The user asked for the latest release to be installed. `None` when
    /// there is nothing to download or an install already runs or finished.
    pub(crate) fn begin_update_install(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(Version, Asset)> {
        let release = self.updates.latest.as_ref()?;
        let asset = release.asset.clone()?;
        let version = release.version;
        if !self.updates.install.begin(version) {
            return None;
        }
        cx.emit(StoreEvent::UpdateChanged);
        cx.notify();
        Some((version, asset))
    }

    /// Download progress; redraws only when the shown percentage changes.
    pub(crate) fn update_download_progress(
        &mut self,
        received: u64,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let before = self.updates.install.percent();
        if self.updates.install.progress(received, total)
            && self.updates.install.percent() != before
        {
            cx.emit(StoreEvent::UpdateChanged);
            cx.notify();
        }
    }

    pub(crate) fn update_downloaded(&mut self, cx: &mut Context<Self>) {
        if self.updates.install.downloaded() {
            cx.emit(StoreEvent::UpdateChanged);
            cx.notify();
        }
    }

    pub(crate) fn finish_update_install(
        &mut self,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if self.updates.install.finish(result) {
            cx.emit(StoreEvent::UpdateChanged);
            cx.notify();
        }
    }

    pub(crate) fn dismiss_update_failure(&mut self, cx: &mut Context<Self>) {
        if self.updates.install.dismiss() {
            cx.emit(StoreEvent::UpdateChanged);
            cx.notify();
        }
    }
}
