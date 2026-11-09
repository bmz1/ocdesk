//! What a mod read, and which store changes must re-render it.
//!
//! A mod's `render` calls host functions (`sessions()`, `theme()`, …). Each
//! call records its [`Key`] in the copy's [`ReadSet`]. A [`StoreEvent`] maps
//! to the keys it can change ([`keys_for`]), and only copies that read one of
//! them are refreshed: a mod that lists sessions does not re-render for every
//! streamed transcript delta.
//!
//! Reads in callbacks and child views count too, so a set only grows until
//! the copy is unloaded (a reload starts a new set).

use crate::store::StoreEvent;

/// A piece of host state a mod can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Session list, titles and busy state.
    Sessions,
    /// Which session is selected.
    Selection,
    /// Sidecar / SSE connection state.
    Connection,
    /// Project folders.
    Folders,
    /// The colour theme.
    Theme,
}

impl Key {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The keys one mod copy has read since it was loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadSet(u8);

impl ReadSet {
    pub fn record(&mut self, key: Key) {
        self.0 |= key.bit();
    }

    pub fn contains(self, key: Key) -> bool {
        self.0 & key.bit() != 0
    }

    /// Whether any of `keys` was read.
    pub fn any(self, keys: &[Key]) -> bool {
        keys.iter().any(|k| self.contains(*k))
    }
}

/// The keys `event` may change. Transcript and diff events map to nothing:
/// the host API does not expose them, so no mod needs to re-render for them.
pub fn keys_for(event: &StoreEvent) -> &'static [Key] {
    match event {
        StoreEvent::SessionsChanged | StoreEvent::StatusChanged { .. } => &[Key::Sessions],
        StoreEvent::SelectionChanged => &[Key::Selection],
        StoreEvent::ConnectionChanged => &[Key::Connection],
        StoreEvent::FoldersChanged => &[Key::Folders, Key::Sessions],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_refreshes_only_for_keys_it_read() {
        let mut reads = ReadSet::default();
        reads.record(Key::Sessions);

        let status = StoreEvent::StatusChanged {
            session_id: "s1".into(),
        };
        assert!(reads.any(keys_for(&status)));
        assert!(reads.any(keys_for(&StoreEvent::FoldersChanged)));
        assert!(!reads.any(keys_for(&StoreEvent::SelectionChanged)));
        assert!(!reads.any(keys_for(&StoreEvent::ConnectionChanged)));
    }

    #[test]
    fn streamed_transcript_events_refresh_nothing() {
        let mut reads = ReadSet::default();
        for key in [
            Key::Sessions,
            Key::Selection,
            Key::Connection,
            Key::Folders,
            Key::Theme,
        ] {
            reads.record(key);
        }
        let streamed = [
            StoreEvent::MessageChanged {
                session_id: "s1".into(),
                index: 3,
            },
            StoreEvent::MessageInserted {
                session_id: "s1".into(),
                index: 4,
            },
            StoreEvent::MessagesReset {
                session_id: "s1".into(),
            },
        ];
        for event in &streamed {
            assert!(!reads.any(keys_for(event)), "{event:?}");
        }
    }

    #[test]
    fn keys_are_independent_bits() {
        let mut reads = ReadSet::default();
        reads.record(Key::Theme);
        assert!(reads.contains(Key::Theme));
        assert!(!reads.contains(Key::Sessions));
        reads.record(Key::Theme);
        assert_eq!(reads, {
            let mut r = ReadSet::default();
            r.record(Key::Theme);
            r
        });
    }
}
