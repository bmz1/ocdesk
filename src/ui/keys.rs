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
        ToggleTasks,
        ShowShortcuts,
        OpenSettings,
        Quit,
        CommandPalette,
        FindInThread,
        CycleDensity,
        CyclePermissionMode,
        ChooseModel,
        CycleEffort,
        FocusNextArea,
        FocusPreviousArea,
        NextSessionTab,
        PreviousSessionTab,
        RenameSession,
        CopyLastReply,
        ToggleTerminal,
        ShowActivity,
        PasteAsText,
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
        (
            "secondary-shift-t",
            Box::new(ToggleTasks),
            "Background tasks (subagents, shells)",
        ),
        ("secondary-k", Box::new(CommandPalette), "Command palette"),
        (
            "secondary-f",
            Box::new(FindInThread),
            "Find in conversation",
        ),
        (
            "ctrl-o",
            Box::new(CycleDensity),
            "Transcript detail: normal / verbose / quiet",
        ),
        (
            "secondary-shift-m",
            Box::new(CyclePermissionMode),
            "Next permission mode",
        ),
        ("secondary-shift-i", Box::new(ChooseModel), "Choose model…"),
        (
            "secondary-shift-e",
            Box::new(CycleEffort),
            "Next reasoning effort",
        ),
        ("f6", Box::new(FocusNextArea), "Focus next area"),
        (
            "shift-f6",
            Box::new(FocusPreviousArea),
            "Focus previous area",
        ),
        ("ctrl-tab", Box::new(NextSessionTab), "Next session"),
        (
            "ctrl-shift-tab",
            Box::new(PreviousSessionTab),
            "Previous session",
        ),
        ("f2", Box::new(RenameSession), "Rename session"),
        (
            "secondary-shift-c",
            Box::new(CopyLastReply),
            "Copy the last reply",
        ),
        ("ctrl-`", Box::new(ToggleTerminal), "Toggle terminal"),
        (
            "secondary-shift-a",
            Box::new(ShowActivity),
            "Sessions that need you",
        ),
        (
            "secondary-shift-v",
            Box::new(PasteAsText),
            "Paste large text into the prompt (not as a file)",
        ),
        ("secondary-/", Box::new(ShowShortcuts), "Keyboard shortcuts"),
        ("secondary-,", Box::new(OpenSettings), "Settings"),
        ("secondary-q", Box::new(Quit), "Quit"),
    ]
}

/// The shortcut for `action` as display text ("Ctrl+K", "⌘K"), honouring
/// the user's overrides; `None` when unbound.
pub fn shortcut_label(
    action: &dyn Action,
    overrides: &std::collections::BTreeMap<String, String>,
    cx: &App,
) -> Option<String> {
    let name = action.name().to_string();
    let (default, ..) = table().into_iter().find(|(_, a, _)| a.name() == name)?;
    let key: String = overrides
        .get(&name)
        .cloned()
        .unwrap_or_else(|| default.to_string());
    // `Keystroke::parse` wants a `'static` str; a KeyBinding parses any.
    let binding = KeyBinding::load(&key, action.boxed_clone(), None, false, None, cx).ok()?;
    let stroke = binding.keystrokes().first()?.as_keystroke().clone();
    Some(gpui_kit::component::kbd::Kbd::format(&stroke))
}

/// Every app action for the command palette, except the palette itself.
pub fn palette_actions() -> Vec<(Box<dyn Action>, &'static str)> {
    table()
        .into_iter()
        .filter(|(_, a, _)| a.name() != CommandPalette.name())
        .map(|(_, a, label)| (a, label))
        .collect()
}

pub fn editable() -> Vec<(String, String, &'static str)> {
    table()
        .into_iter()
        .map(|(key, action, label)| (action.name().to_string(), key.to_string(), label))
        .collect()
}
pub fn validate(
    overrides: &std::collections::BTreeMap<String, String>,
    cx: &App,
) -> anyhow::Result<Vec<KeyBinding>> {
    let mut seen = std::collections::HashSet::new();
    table()
        .into_iter()
        .filter_map(|(default, action, _)| {
            let key = overrides
                .get(action.name())
                .map(String::as_str)
                .unwrap_or(default);
            if key.trim().is_empty() {
                return None;
            }
            let result = KeyBinding::load(key, action, None, false, None, cx)
                .map_err(anyhow::Error::from)
                .and_then(|b| {
                    // Compare resolved keystrokes as well (Cmd vs secondary, whitespace).
                    let normalized = b
                        .keystrokes()
                        .iter()
                        .map(|k| format!("{k:?}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !seen.insert(normalized) {
                        anyhow::bail!("Duplicate shortcut: {key}");
                    }
                    Ok(b)
                });
            Some(result)
        })
        .collect()
}
pub fn apply(
    overrides: &std::collections::BTreeMap<String, String>,
    cx: &mut App,
) -> anyhow::Result<()> {
    let bindings = validate(overrides, cx)?;
    let names: std::collections::HashSet<_> =
        editable().into_iter().map(|(name, _, _)| name).collect();
    let retained: Vec<_> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|b| !names.contains(b.action().name()))
        .cloned()
        .collect();
    cx.clear_key_bindings();
    cx.bind_keys(retained);
    cx.bind_keys(bindings);
    Ok(())
}
pub fn bind(cx: &mut App) {
    let prefs = crate::settings::Settings::load();
    if let Err(e) = apply(&prefs.keybindings, cx) {
        tracing::warn!("invalid saved shortcuts: {e:#}");
        let _ = apply(&Default::default(), cx);
    }
}

/// Composer-local keys that aren't actions of ours, for the sheet.
const COMPOSER_KEYS: &[(&str, &str)] = &[
    ("Enter", "Send"),
    ("Shift+Enter", "New line"),
    (
        "Esc",
        "Close the @ popup, deny a pending approval, else stop the running turn (twice)",
    ),
    ("Enter (empty box)", "Allow a pending approval once"),
    (
        "Enter (with text)",
        "Deny a pending approval with that text as feedback",
    ),
    (
        "Up / Down",
        "Earlier / later prompts (at the start of the box)",
    ),
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
                .child(div().flex_1().min_w_0().child(*label))
                .child(
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_md()
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(keys.clone()),
                )
        }));
        dialog.title("Keyboard shortcuts").w(px(560.)).child(
            div()
                .id("shortcuts-scroll")
                .max_h(px(520.))
                .overflow_y_scroll()
                .child(list),
        )
    });
}
