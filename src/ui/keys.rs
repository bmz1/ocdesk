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
        ToggleAgents,
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
        ToggleFiles,
        GoToFile,
        SaveFile,
        CloseTab,
        NextTab,
        PreviousTab,
        ShowChat,
        TogglePullRequest,
    ]
);

/// Open a file in an editor tab (from tool cards, the Changes panel, …).
/// `path` is relative to the current folder or absolute inside it.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = ocdesk, no_json)]
pub struct OpenFile {
    pub path: String,
    /// 1-based line to reveal.
    pub line: Option<u32>,
}

/// (keystroke, action, description) — also the source of the shortcuts sheet.
fn table() -> Vec<(&'static str, Box<dyn Action>, &'static str)> {
    vec![
        ("secondary-n", Box::new(NewSession), "New session"),
        ("secondary-o", Box::new(AddFolder), "Add project…"),
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
        (
            "secondary-shift-a",
            Box::new(ToggleAgents),
            "Agents: edit opencode agents",
        ),
        ("secondary-k", Box::new(CommandPalette), "Command palette"),
        (
            "secondary-f",
            Box::new(FindInThread),
            "Find in conversation",
        ),
        (
            // Ctrl+O as in Claude; Ctrl+O is Add folder off macOS.
            if cfg!(target_os = "macos") {
                "ctrl-o"
            } else {
                "ctrl-shift-o"
            },
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
            "secondary-shift-r",
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
            "secondary-shift-u",
            Box::new(ShowActivity),
            "Sessions that need you",
        ),
        (
            "secondary-shift-v",
            Box::new(PasteAsText),
            "Paste large text into the prompt (not as a file)",
        ),
        (
            "secondary-shift-e",
            Box::new(ToggleFiles),
            "Toggle files panel",
        ),
        ("secondary-p", Box::new(GoToFile), "Go to file…"),
        ("secondary-s", Box::new(SaveFile), "Save file"),
        ("secondary-w", Box::new(CloseTab), "Close file tab"),
        ("secondary-shift-]", Box::new(NextTab), "Next tab"),
        ("secondary-shift-[", Box::new(PreviousTab), "Previous tab"),
        ("secondary-1", Box::new(ShowChat), "Show the chat tab"),
        (
            "secondary-shift-g",
            Box::new(TogglePullRequest),
            "Pull request: checks and review comments",
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
    cx.bind_keys(terminal_passthrough());
    Ok(())
}
/// Key context of the focused terminal.
pub const TERMINAL_CONTEXT: &str = "Terminal";

/// Keys the shell needs (Tab completion, Ctrl+letter, Alt+arrows): inside the
/// terminal they bind to `NoAction`, which masks app shortcuts and focus
/// navigation so the key reaches the terminal's own key handler. Ctrl+Shift,
/// Cmd, F-keys, Ctrl+Tab and Ctrl+` shortcuts still work there.
fn terminal_passthrough() -> Vec<KeyBinding> {
    let mut keys: Vec<String> = ["tab", "shift-tab", "escape", "enter"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    keys.extend(('a'..='z').map(|c| format!("ctrl-{c}")));
    keys.extend(('a'..='z').map(|c| format!("alt-{c}")));
    for k in [
        "/", ".", ",", "[", "]", "\\", "space", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0",
    ] {
        keys.push(format!("ctrl-{k}"));
    }
    for k in ["up", "down", "left", "right", "backspace"] {
        keys.push(format!("alt-{k}"));
    }
    // `KeyBinding::new` wants `'static` strings; build them once.
    static KEYS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        keys.into_iter()
            .map(|k| &*Box::leak(k.into_boxed_str()))
            .collect()
    })
    .iter()
    .map(|k| KeyBinding::new(k, NoAction {}, Some(TERMINAL_CONTEXT)))
    .collect()
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

/// The menu bar (macOS); Edit uses the text inputs' actions so the system
/// routes Cut/Copy/Paste/Undo to the focused field.
pub fn set_menus(cx: &mut App) {
    use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    cx.set_menus([
        Menu::new("ocdesk").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
            MenuItem::separator(),
            MenuItem::action("Quit ocdesk", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Session", NewSession),
            MenuItem::action("Add Project…", AddFolder),
            MenuItem::separator(),
            MenuItem::action("Go to File…", GoToFile),
            MenuItem::action("Save File", SaveFile),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::separator(),
            MenuItem::action("Rename Session", RenameSession),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find in Conversation", FindInThread),
            MenuItem::action("Copy Last Reply", CopyLastReply),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", CommandPalette),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Sessions That Need You", ShowActivity),
            MenuItem::separator(),
            MenuItem::action("Changes", ToggleChanges),
            MenuItem::action("Background Tasks", ToggleTasks),
            MenuItem::action("Agents", ToggleAgents),
            MenuItem::action("Terminal", ToggleTerminal),
            MenuItem::action("Files", ToggleFiles),
            MenuItem::separator(),
            MenuItem::action("Chat Tab", ShowChat),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PreviousTab),
            MenuItem::separator(),
            MenuItem::action("Transcript Detail", CycleDensity),
        ]),
        Menu::new("Session").items([
            MenuItem::action("Choose Model…", ChooseModel),
            MenuItem::action("Next Reasoning Effort", CycleEffort),
            MenuItem::action("Next Permission Mode", CyclePermissionMode),
            MenuItem::separator(),
            MenuItem::action("Stop", StopTurn),
            MenuItem::separator(),
            MenuItem::action("Next Session", NextSessionTab),
            MenuItem::action("Previous Session", PreviousSessionTab),
        ]),
    ]);
}

#[cfg(test)]
mod tests {
    #[::core::prelude::v1::test]
    fn default_shortcuts_are_unique_on_this_platform() {
        let secondary = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        let mut seen = std::collections::HashSet::new();
        for (key, _, label) in super::table() {
            let mut parts: Vec<String> = key
                .replace("secondary", secondary)
                .split('-')
                .map(str::to_string)
                .collect();
            let last = parts.pop().unwrap();
            parts.sort();
            parts.push(last);
            assert!(
                seen.insert(parts.join("-")),
                "duplicate default shortcut {key} ({label})"
            );
        }
    }
}
