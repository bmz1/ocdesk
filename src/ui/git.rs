//! Git branch and worktree UI: the branch pill in the conversation header
//! (with a menu to start or move sessions between a repository's
//! checkouts, and to create worktrees) and the dialogs it opens.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::Store;
use crate::ui::folder_name;

/// "feature-x · main": a checkout's name and branch for menus.
pub fn checkout_label(store: &Store, dir: &Path) -> String {
    let name = folder_name(dir);
    match store.vcs.get(dir).map(|v| v.branch.clone()) {
        Some(Some(b)) if b != name => format!("{name} · {b}"),
        Some(None) => format!("{name} · detached"),
        _ => name,
    }
}

/// The repository folder and all its checkouts (main first).
fn checkouts(store: &Store, folder: &Path) -> Vec<PathBuf> {
    std::iter::once(folder.to_path_buf())
        .chain(store.worktrees.get(folder).into_iter().flatten().cloned())
        .collect()
}

/// `⎇ branch` for the directory the conversation targets; `None` outside a
/// git repository.
pub fn branch_pill(
    store: &Entity<Store>,
    controller: &Entity<Controller>,
    cx: &App,
) -> Option<AnyElement> {
    let s = store.read(cx);
    let dir = s
        .selected
        .as_ref()
        .and_then(|id| s.session(id))
        .map(|x| x.directory.clone())
        .or_else(|| s.target_folder())?;
    // A checkout we know about but with no branch is a detached HEAD
    // (opencode creates worktrees detached).
    let branch = s
        .vcs
        .get(&dir)?
        .branch
        .clone()
        .unwrap_or_else(|| "detached".into());
    let folder = s.folder_of(&dir).unwrap_or_else(|| dir.clone());
    let selected = s.selected.clone();
    let worktree_name = (dir != folder).then(|| folder_name(&dir));
    let label = match &worktree_name {
        Some(w) if *w != branch => format!("{branch} · {w}"),
        _ => branch,
    };
    let entries: Vec<(PathBuf, String)> = checkouts(s, &folder)
        .into_iter()
        .map(|d| {
            let l = checkout_label(s, &d);
            (d, l)
        })
        .collect();
    let (store, controller) = (store.clone(), controller.clone());
    Some(
        Button::new("branch-pill")
            .icon(super::icons::git_branch())
            .label(label)
            .ghost()
            .small()
            .tooltip("Branch and worktrees")
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.label("New session in");
                for (d, l) in &entries {
                    let (c, d2) = (controller.clone(), d.clone());
                    menu = menu.item(PopupMenuItem::new(l.clone()).checked(*d == dir).on_click(
                        move |_, _, cx| {
                            let d = d2.clone();
                            c.update(cx, |c, cx| c.new_session(Some(d), cx));
                        },
                    ));
                }
                if let Some(sid) = &selected
                    && entries.len() > 1
                {
                    menu = menu.separator().label("Move this session to");
                    for (d, l) in entries.iter().filter(|(d, _)| *d != dir) {
                        let (c, d2, sid) = (controller.clone(), d.clone(), sid.clone());
                        menu =
                            menu.item(PopupMenuItem::new(l.clone()).on_click(move |_, _, cx| {
                                let (sid, d) = (sid.clone(), d2.clone());
                                c.update(cx, |c, cx| c.move_session(sid, d, cx));
                            }));
                    }
                }
                let (c, f, st) = (controller.clone(), folder.clone(), store.clone());
                menu.separator()
                    .item(
                        PopupMenuItem::new("New worktree…").on_click(move |_, window, cx| {
                            open_new_worktree(c.clone(), st.clone(), f.clone(), window, cx)
                        }),
                    )
            })
            .into_any_element(),
    )
}

/// Ask for a name and create a worktree of `folder` (new session opens in it).
pub fn open_new_worktree(
    controller: Entity<Controller>,
    _store: Entity<Store>,
    folder: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("feature-name"));
    let initial_focus = std::cell::Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        if initial_focus.replace(false) {
            input.update(cx, |i, cx| i.focus(window, cx));
        }
        let (input, controller, folder) = (input.clone(), controller.clone(), folder.clone());
        alert
            .title(format!("New worktree of {}", folder_name(&folder)))
            .description(Input::new(&input))
            .confirm()
            .ok_text("Create")
            .on_ok(move |_, _, cx| {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() {
                    return false;
                }
                let folder = folder.clone();
                controller.update(cx, |c, cx| c.create_worktree(folder, name, cx));
                true
            })
    });
}

/// Remove a worktree, asking again (with force) when it has changes.
pub fn remove_worktree(
    controller: Entity<Controller>,
    folder: PathBuf,
    dir: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let handle = window.window_handle();
    let (c2, f2, d2) = (controller.clone(), folder.clone(), dir.clone());
    let ask_force = move |cx: &mut App| {
        let _ = handle.update(cx, |_, window, cx| {
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (c, f, d) = (c2.clone(), f2.clone(), d2.clone());
                alert
                    .title("Discard the worktree's changes?")
                    .description(format!(
                        "{} has uncommitted changes. Removing it deletes them.",
                        d.display()
                    ))
                    .confirm()
                    .ok_text("Remove anyway")
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, _, cx| {
                        let (f, d) = (f.clone(), d.clone());
                        c.update(cx, |c, cx| c.remove_worktree(f, d, true, |_| {}, cx));
                        true
                    })
            });
        });
    };
    controller.update(cx, |c, cx| {
        c.remove_worktree(folder, dir, false, ask_force, cx)
    });
}

/// Small "+" button that starts a session in `dir` (sidebar rows).
pub fn new_session_button(
    id: SharedString,
    controller: Entity<Controller>,
    dir: PathBuf,
) -> Button {
    Button::new(id)
        .icon(gpui_kit::component::IconName::Plus)
        .ghost()
        .xsmall()
        .tooltip("New session here")
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let d = dir.clone();
            controller.update(cx, |c, cx| c.new_session(Some(d), cx));
        })
}
