//! App-wide keyboard shortcuts. `secondary` is Cmd on macOS, Ctrl elsewhere.
//!
//! These are window-level bindings (no key context), so a focused text
//! input's own bindings win on conflicts; none of the keys below are bound
//! by gpui-component's inputs except `secondary-.`, which the composer
//! claims explicitly (see `Composer`).

use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

actions!(
    ocdesk,
    [
        NewSession,
        AddFolder,
        ToggleSidebar,
        PreviousSession,
        NextSession,
        FocusComposer,
        StopTurn,
        ToggleChanges,
        ShowShortcuts,
        Quit,
    ]
);

/// (keystroke, action, description) — also the source of the shortcuts sheet.
fn table() -> Vec<(&'static str, Box<dyn Action>, &'static str)> {
    vec![
        ("secondary-n", Box::new(NewSession), "New session"),
        ("secondary-o", Box::new(AddFolder), "Add folder…"),
        ("secondary-b", Box::new(ToggleSidebar), "Show/hide sidebar"),
        ("alt-up", Box::new(PreviousSession), "Previous session"),
        ("alt-down", Box::new(NextSession), "Next session"),
        ("secondary-l", Box::new(FocusComposer), "Focus the prompt"),
        ("secondary-.", Box::new(StopTurn), "Stop the running turn"),
        (
            "secondary-shift-d",
            Box::new(ToggleChanges),
            "Toggle Changes / Conversation",
        ),
        ("secondary-/", Box::new(ShowShortcuts), "Keyboard shortcuts"),
        ("secondary-q", Box::new(Quit), "Quit"),
    ]
}

pub fn bind(cx: &mut App) {
    let bindings: Vec<KeyBinding> = table()
        .into_iter()
        .map(|(keys, action, _)| KeyBinding::load(keys, action, None, false, None, cx))
        .filter_map(|b| match b {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::error!("bad key binding: {e:#}");
                None
            }
        })
        .collect();
    cx.bind_keys(bindings);
}

/// Composer-local keys that aren't actions of ours, for the sheet.
const COMPOSER_KEYS: &[(&str, &str)] = &[
    ("Enter", "Send"),
    ("Shift+Enter", "New line"),
    ("Esc", "Close the @ popup, else stop the running turn"),
    ("Up / Down", "Earlier / later prompts (at the start of the box)"),
    ("@", "Mention a file or subagent"),
];

pub fn show_shortcuts(window: &mut Window, cx: &mut App) {
    let rows: Vec<(String, &'static str)> = table()
        .into_iter()
        .map(|(_, action, label)| (window.keystroke_text_for(action.as_ref(), cx), label))
        .chain(COMPOSER_KEYS.iter().map(|(k, l)| (k.to_string(), *l)))
        .collect();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let list = v_flex().gap_1().children(rows.iter().map(|(keys, label)| {
            h_flex()
                .justify_between()
                .gap_6()
                .text_sm()
                .child(div().child(*label))
                .child(
                    div()
                        .px_1p5()
                        .rounded_md()
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(keys.clone()),
                )
        }));
        dialog.title("Keyboard shortcuts").child(list)
    });
}
