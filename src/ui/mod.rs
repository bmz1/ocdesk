pub mod app_view;
pub mod changes;
pub mod composer;
pub mod conversation;
pub mod form_view;
pub mod frame_stats;
pub mod message;
pub mod permission;
pub mod prose;
pub mod sidebar;
pub mod tool_card;

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;

use crate::controller::Controller;

/// "Add folder…": the native folder picker, or a typed path when the
/// platform has no picker (e.g. Linux without an xdg-desktop-portal).
pub fn add_folder(controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
    let force_dialog = std::env::var_os("OCDESK_FOLDER_DIALOG").is_some();
    let rx = (!force_dialog).then(|| {
        cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add folder".into()),
        })
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let picked = match rx {
            Some(rx) => rx.await.map_err(anyhow::Error::from).and_then(|r| r),
            None => Err(anyhow::anyhow!("folder dialog forced")),
        };
        match picked {
            Ok(Some(paths)) => {
                if let Some(p) = paths.into_iter().next() {
                    cx.update(|cx| controller.update(cx, |c, cx| c.add_folder(p, cx)));
                }
            }
            Ok(None) => {} // cancelled
            Err(e) => {
                tracing::info!("native folder picker unavailable ({e:#}); asking for a path");
                let _ = handle.update(cx, |_, window, cx| open_path_dialog(controller, window, cx));
            }
        }
    })
    .detach();
}

fn open_path_dialog(controller: Entity<Controller>, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/project"));
    let initial_focus = std::cell::Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        if initial_focus.replace(false) {
            input.update(cx, |i, cx| i.focus(window, cx));
        }
        let (input, controller) = (input.clone(), controller.clone());
        alert
            .title("Add folder")
            .description(Input::new(&input))
            .confirm()
            .ok_text("Add")
            .on_ok(move |_, _, cx| {
                let raw = input.read(cx).value().trim().to_string();
                if !raw.is_empty() {
                    let path = match raw.strip_prefix("~/") {
                        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                        None => PathBuf::from(raw),
                    };
                    controller.update(cx, |c, cx| c.add_folder(path, cx));
                }
                true
            })
    });
}

/// Last path component, for display.
pub fn folder_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// "now", "5m", "3h", "2d", "Mar 4". `ms` is a Unix epoch timestamp in ms.
pub fn relative_time(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(ms);
    let secs = now.saturating_sub(ms) / 1000;
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn relative_time_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(super::relative_time(now), "now");
        assert_eq!(super::relative_time(now - 5 * 60_000), "5m");
        assert_eq!(super::relative_time(now - 3 * 3_600_000), "3h");
        assert_eq!(super::relative_time(now - 2 * 86_400_000), "2d");
    }
}
