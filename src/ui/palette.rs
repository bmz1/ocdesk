//! The command palette (Cmd/Ctrl+K): sessions across every folder, new
//! session in a folder, every app action with its shortcut, models,
//! transcript detail and theme. Cmd/Ctrl+Shift+I opens it on models only.

use std::rc::Rc;

use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IconName, WindowExt as _};
use gpui_kit::*;

use crate::controller::Controller;
use crate::settings::Density;
use crate::store::Store;
use crate::ui::folder_name;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    All,
    Models,
}

type Run = Rc<dyn Fn(&mut Window, &mut App)>;

struct Group {
    label: &'static str,
    items: Vec<(CommandItem, Run)>,
}

/// Run `f` after the dialog closed and focus went back to the app, so
/// actions dispatched from it reach the app view's handlers.
fn after_close(f: impl Fn(&mut Window, &mut App) + 'static) -> Run {
    let f = Rc::new(f);
    Rc::new(move |window, cx| {
        window.close_dialog(cx);
        let f = f.clone();
        window.defer(cx, move |window, cx| f(window, cx));
    })
}

fn groups(mode: Mode, store: &Store, controller: &Entity<Controller>) -> Vec<Group> {
    let mut out = Vec::new();
    if mode == Mode::All {
        // Sessions, most recently updated first, children (subagents) left out.
        let mut sessions: Vec<_> = store
            .sessions
            .iter()
            .filter(|s| s.parent_id.is_none())
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
        let items = sessions
            .into_iter()
            .take(200)
            .map(|s| {
                let folder = store
                    .folder_of(&s.directory)
                    .map(|f| folder_name(&f))
                    .unwrap_or_default();
                let current = store.selected.as_deref() == Some(s.id.as_str());
                let item = CommandItem::new()
                    .label(format!("{}  ·  {folder}", s.display_title()))
                    .icon(crate::ui::icons::message_square())
                    .keywords([folder])
                    .checked(current);
                let (c, id) = (controller.clone(), s.id.clone());
                let run = after_close(move |_, cx| {
                    let id = id.clone();
                    c.update(cx, |c, cx| c.select_session(Some(id), cx));
                });
                (item, run)
            })
            .collect();
        out.push(Group {
            label: "Sessions",
            items,
        });
        let items = store
            .folders
            .iter()
            .map(|f| {
                let item = CommandItem::new()
                    .label(format!("New session in {}", folder_name(f)))
                    .icon(IconName::Plus)
                    .keywords([f.display().to_string()]);
                let (c, f) = (controller.clone(), f.clone());
                let run = after_close(move |_, cx| {
                    let f = f.clone();
                    c.update(cx, |c, cx| c.new_session(Some(f), cx));
                });
                (item, run)
            })
            .collect();
        out.push(Group {
            label: "Projects",
            items,
        });
        let items = super::keys::palette_actions()
            .into_iter()
            .map(|(action, label)| {
                let item = CommandItem::new().label(label).action(action.boxed_clone());
                let run =
                    after_close(move |window, cx| window.dispatch_action(action.boxed_clone(), cx));
                (item, run)
            })
            .collect();
        out.push(Group {
            label: "Actions",
            items,
        });
        let commands: Vec<String> = store
            .catalog_folder()
            .and_then(|f| store.tooling.get(&f))
            .map(|t| t.commands.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        if !commands.is_empty() {
            let items = commands
                .into_iter()
                .map(|name| {
                    let item = CommandItem::new()
                        .label(format!("/{name}"))
                        .icon(crate::ui::icons::slash());
                    let c = controller.clone();
                    let run = after_close(move |_, cx| {
                        let name = name.clone();
                        c.update(cx, |c, cx| c.run_command(name, String::new(), None, cx));
                    });
                    (item, run)
                })
                .collect();
            out.push(Group {
                label: "Commands",
                items,
            });
        }
        let current = store.preferences.density;
        let items = Density::ALL
            .into_iter()
            .map(|d| {
                let item = CommandItem::new()
                    .label(format!("Transcript: {}", d.label()))
                    .checked(d == current);
                let c = controller.clone();
                let run = after_close(move |_, cx| c.update(cx, |c, cx| c.set_density(d, cx)));
                (item, run)
            })
            .chain(["dark", "light", "system"].into_iter().map(|t| {
                let checked = store.preferences.theme.as_deref().unwrap_or("dark") == t;
                let item = CommandItem::new()
                    .label(format!("Theme: {t}"))
                    .icon(if t == "light" {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    })
                    .checked(checked);
                let c = controller.clone();
                let run = after_close(move |_, cx| {
                    c.update(cx, |c, cx| {
                        c.update_settings(cx, |s| s.theme = Some(t.to_string()))
                    })
                });
                (item, run)
            }))
            .collect();
        out.push(Group {
            label: "View",
            items,
        });
    }
    // Models, grouped by provider in the server's first-appearance order.
    if let Some(catalog) = store.target_catalog() {
        let current = store.effective_model();
        let mut models = catalog.models.clone();
        let mut providers: Vec<String> = Vec::new();
        for m in &models {
            if !providers.contains(&m.provider_id) {
                providers.push(m.provider_id.clone());
            }
        }
        models.sort_by_key(|m| providers.iter().position(|p| *p == m.provider_id));
        let items = models
            .into_iter()
            .map(|m| {
                let r = m.as_ref();
                let checked = current.as_ref().is_some_and(|c| c.same_model(&r));
                let item = CommandItem::new()
                    .label(format!("{}  ·  {}", m.name, m.provider_id))
                    .icon(IconName::Cpu)
                    .keywords([m.id.clone(), m.provider_id])
                    .checked(checked);
                let c = controller.clone();
                let run = after_close(move |_, cx| {
                    let r = r.clone();
                    c.update(cx, |c, cx| c.choose_model(r, cx));
                });
                (item, run)
            })
            .collect();
        out.push(Group {
            label: "Models",
            items,
        });
    }
    out
}

pub fn open(controller: Entity<Controller>, mode: Mode, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let store = controller.read(cx).view_store();
    let groups = Rc::new(groups(mode, store.read(cx), &controller));
    let state = cx.new(|cx| CommandState::new(window, cx));
    let initial_focus = std::cell::Cell::new(true);
    let placeholder = match mode {
        Mode::All => "Search sessions, actions, models…",
        Mode::Models => "Choose a model…",
    };
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            state.update(cx, |s, cx| s.focus(window, cx));
        }
        let mut command = Command::new(&state)
            .placeholder(placeholder)
            .max_h(px(420.))
            .bordered(false);
        for g in groups.iter() {
            command = command.group(
                CommandGroup::new()
                    .label(g.label)
                    .items(g.items.iter().map(|(item, _)| item.clone())),
            );
        }
        let groups = groups.clone();
        command = command
            .on_confirm(move |ix, window, cx| {
                if let Some((_, run)) = groups.get(ix.section).and_then(|g| g.items.get(ix.row)) {
                    run(window, cx);
                }
            })
            .on_cancel(|window, cx| window.close_dialog(cx));
        dialog
            .entrance(gpui_kit::component::dialog::DialogEntrance::None)
            .w(px(640.))
            .p_0()
            .child(command)
    });
}
