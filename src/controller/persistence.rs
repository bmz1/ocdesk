//! Settings persistence: immediate and debounced, ordered snapshot writes.

use super::*;

impl Controller {
    /// Change settings and save them after a short quiet period, for values
    /// that change in bursts (window resizes, sidebar drags).
    pub fn update_settings(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        let before = self.settings.clone();
        f(&mut self.settings);
        if self.settings == before {
            return;
        }
        self.store.update(cx, |s, cx| {
            s.preferences = self.settings.clone();
            cx.notify();
        });
        self.schedule_save(cx);
    }

    /// Layout-only settings (pane widths, window placement): persisted, but
    /// views keep their own copy, so no clone of everything and no store
    /// notify, which would redraw every view per pixel of a drag.
    pub fn update_layout(&mut self, cx: &mut Context<Self>, f: impl Fn(&mut Settings)) {
        f(&mut self.settings);
        self.store.update(cx, |s, _| f(&mut s.preferences));
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let exec = cx.background_executor().clone();
        // Dropping the previous GPUI task cancels its timer.
        self.save_debounce = Some(cx.spawn(async move |this, cx| {
            exec.timer(SETTINGS_DEBOUNCE).await;
            let _ = this.update(cx, |this, _| this.save_settings());
        }));
    }

    /// Write pending settings now (on quit).
    pub fn flush_settings(&mut self) {
        self.save_debounce = None;
        // The newest snapshot: it waits for any write in flight, then wins.
        if let Err(e) = self.settings.snapshot_for_save().write() {
            tracing::warn!(op = "save_settings", error = %format!("{e:#}"), "could not save settings");
        }
    }

    pub(super) fn save_settings(&self) {
        // Numbered here, on the UI thread, so request order decides which
        // write sticks however the background writes interleave.
        let pending = self.settings.snapshot_for_save();
        self.rt.spawn_blocking(move || {
            if let Err(e) = pending.write() {
                tracing::warn!(op = "save_settings", error = %format!("{e:#}"), "could not save settings");
            }
        });
    }
}
