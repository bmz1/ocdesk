//! The `ocdesk` module a mod imports, and the policy each mod runs under.
//!
//! Boundaries (see AGENTS.md):
//! - Reads come straight from the [`Store`] while the script runs; nothing is
//!   copied into a second snapshot.
//! - Writes are *intents* sent over a channel and run later by the panel
//!   through the [`crate::controller::Controller`]. A host function must not
//!   re-enter the script engine, and a synchronous controller call would: it
//!   updates the store, whose events refresh mods, while the script that made
//!   the call is still on the stack.
//! - Permission replies and question forms are deliberately absent. ocdesk
//!   never answers them on the user's behalf, and neither may a mod.
//! - A mod gets no file, process, network or clipboard access in this spike;
//!   only its own `localStorage` file.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Entity, Hsla, Rgba};
use gpui_shell::policy::Policy;
use gpui_shell::{Capabilities, HostError, HostModule, HostResult, HostValue};

use super::reads::{Key, ReadSet};
use crate::store::{Connection, Store};

/// What a mod asked the app to do. Validated again where it is applied.
#[derive(Debug, PartialEq)]
pub enum Intent {
    SelectSession(String),
}

const DECLARATIONS: &str = r#"
export interface Session {
  id: string;
  title: string;
  directory: string;
  /** Last update, milliseconds since the epoch. */
  updated: number;
  busy: boolean;
  unread: boolean;
}
export type ConnectionState = "starting" | "connecting" | "connected" | "reconnecting" | "down" | null;
/** Top-level sessions, in sidebar order. */
export function sessions(): Session[];
/** The selected session's id. */
export function selected(): string | null;
export function connection(): ConnectionState;
/** Project folders, as absolute paths. */
export function folders(): string[];
/** ocdesk's colours as `#rrggbbaa` strings. */
export function theme(): Record<string, string>;
/** Asks the app to select a session. Unknown ids are ignored. */
export function selectSession(id: string): void;
"#;

/// The policy for one mod copy: its own host module (bound to this copy's
/// read set), its own storage file, and nothing else.
pub fn policy(
    name: &str,
    storage: &Path,
    store: Entity<Store>,
    reads: Rc<Cell<ReadSet>>,
    intents: UnboundedSender<Intent>,
) -> anyhow::Result<Policy> {
    let capabilities = Capabilities::new().storage(true);
    Policy::new()
        .with_application(name)
        .with_capabilities(capabilities)
        .with_storage_path(storage.to_path_buf())
        .with_host_module(module(store, reads, intents))
        .map_err(|e| anyhow::anyhow!("ocdesk host module: {}", e.message()))
}

/// Reads `key` from the app during a script call and records the dependency.
fn read(reads: &Cell<ReadSet>, key: Key, f: impl FnOnce(&App) -> HostValue) -> HostResult {
    let mut set = reads.get();
    set.record(key);
    reads.set(set);
    gpui_shell::with_current_app(|cx| f(cx))
        .ok_or_else(|| HostError::new("ocdesk: called outside a script call"))
}

fn module(
    store: Entity<Store>,
    reads: Rc<Cell<ReadSet>>,
    intents: UnboundedSender<Intent>,
) -> HostModule {
    let reader = |key: Key, f: fn(&Store) -> HostValue| {
        let store = store.clone();
        let reads = reads.clone();
        move |_: &gpui_shell::HostArguments| read(&reads, key, |cx| f(store.read(cx)))
    };
    let theme_reads = reads.clone();
    HostModule::new("ocdesk")
        .declarations(DECLARATIONS)
        .function("sessions", reader(Key::Sessions, sessions))
        .function(
            "selected",
            reader(Key::Selection, |s| {
                s.selected.clone().map_or(HostValue::Null, HostValue::Str)
            }),
        )
        .function("connection", reader(Key::Connection, connection))
        .function(
            "folders",
            reader(Key::Folders, |s| {
                HostValue::Array(
                    s.folders
                        .iter()
                        .map(|f| HostValue::Str(f.display().to_string()))
                        .collect(),
                )
            }),
        )
        .function("theme", move |_| read(&theme_reads, Key::Theme, theme))
        .function("selectSession", move |args| {
            let id = args.string(0)?.to_owned();
            intents
                .unbounded_send(Intent::SelectSession(id))
                .map_err(|_| HostError::new("ocdesk: the mods panel is gone"))?;
            Ok(HostValue::Null)
        })
}

fn sessions(s: &Store) -> HostValue {
    HostValue::Array(
        s.sessions
            .iter()
            .filter(|x| x.parent_id.is_none())
            .map(|x| {
                HostValue::Object(vec![
                    ("id".into(), HostValue::Str(x.id.clone())),
                    ("title".into(), HostValue::Str(x.title.clone())),
                    (
                        "directory".into(),
                        HostValue::Str(x.directory.display().to_string()),
                    ),
                    ("updated".into(), HostValue::Number(x.updated as f64)),
                    ("busy".into(), HostValue::Bool(s.is_busy(&x.id))),
                    ("unread".into(), HostValue::Bool(s.unread.contains(&x.id))),
                ])
            })
            .collect(),
    )
}

fn connection(s: &Store) -> HostValue {
    let name = match &s.connection {
        None => return HostValue::Null,
        Some(Connection::Starting) => "starting",
        Some(Connection::Connecting) => "connecting",
        Some(Connection::Connected) => "connected",
        Some(Connection::Reconnecting) => "reconnecting",
        Some(Connection::SidecarDown { .. }) => "down",
    };
    HostValue::Str(name.into())
}

fn hex(color: Hsla) -> HostValue {
    let c: Rgba = color.into();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    HostValue::Str(format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(c.r),
        byte(c.g),
        byte(c.b),
        byte(c.a)
    ))
}

fn theme(cx: &App) -> HostValue {
    let t = cx.theme();
    HostValue::Object(
        [
            ("background", t.background),
            ("foreground", t.foreground),
            ("muted", t.muted),
            ("mutedForeground", t.muted_foreground),
            ("border", t.border),
            ("primary", t.primary),
            ("primaryForeground", t.primary_foreground),
            ("secondary", t.secondary),
            ("accent", t.accent),
            ("listHover", t.list_hover),
            ("listActive", t.list_active),
            ("danger", t.danger),
            ("success", t.success),
            ("warning", t.warning),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), hex(v)))
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_cross_as_rgba_hex() {
        assert_eq!(
            hex(gpui_kit::hsla(0., 0., 1., 1.)),
            HostValue::Str("#ffffffff".into())
        );
        assert_eq!(
            hex(gpui_kit::hsla(0., 1., 0.5, 0.5)),
            HostValue::Str("#ff000080".into())
        );
    }
}
