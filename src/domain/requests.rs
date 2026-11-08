//! Server and stream identity are intentionally distinct. Reconnecting invalidates
//! stream snapshots, while an HTTP mutation can still finish on the same server.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ServerGeneration(u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnectionEpoch(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestScope {
    Server,
    Connection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Issued {
    generation: ServerGeneration,
    epoch: Option<ConnectionEpoch>,
}

#[derive(Default)]
pub(crate) struct RequestClock {
    generation: ServerGeneration,
    epoch: ConnectionEpoch,
}

impl RequestClock {
    pub(crate) fn generation(&self) -> ServerGeneration {
        self.generation
    }
    pub(crate) fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    pub(crate) fn restart(&mut self) {
        self.generation.0 = self
            .generation
            .0
            .checked_add(1)
            .expect("server generation exhausted");
    }

    pub(crate) fn connection_changed(&mut self) {
        self.epoch.0 = self
            .epoch
            .0
            .checked_add(1)
            .expect("connection epoch exhausted");
    }

    pub(crate) fn issue(&self, scope: RequestScope) -> Issued {
        Issued {
            generation: self.generation,
            epoch: (scope == RequestScope::Connection).then_some(self.epoch),
        }
    }

    pub(crate) fn accepts(&self, issued: Issued) -> bool {
        issued.generation == self.generation && issued.epoch.is_none_or(|epoch| epoch == self.epoch)
    }
}

/// One revision of a keyed resource, unique across every key and every
/// [`Revisions::forget`]: a token from before a folder was removed and re-added
/// (or a session deleted) can never match the current revision again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Revision(u64);

/// Latest-wins revision guard for per-resource async reads.
///
/// A completion is applied only if its resource has not changed since the
/// request was issued: either a newer request superseded it ([`Self::bump`]
/// at issue time), or some other input changed the resource ([`Self::bump`]
/// from a live event, then compare with [`Self::current`]). Per-key counters
/// that restart at zero after removal would let a stale completion alias a
/// fresh request; one shared counter makes that impossible.
pub(crate) struct Revisions<K> {
    next: u64,
    current: std::collections::HashMap<K, Revision>,
}

impl<K> Default for Revisions<K> {
    fn default() -> Self {
        Self {
            next: 0,
            current: std::collections::HashMap::new(),
        }
    }
}

impl<K: std::hash::Hash + Eq> Revisions<K> {
    fn fresh(&mut self) -> Revision {
        self.next = self
            .next
            .checked_add(1)
            .expect("revision counter exhausted");
        Revision(self.next)
    }

    /// Supersede everything issued for `key` so far; returns the new revision.
    pub(crate) fn bump(&mut self, key: K) -> Revision {
        let revision = self.fresh();
        self.current.insert(key, revision);
        revision
    }

    /// The revision `key` is at now, without superseding anything.
    pub(crate) fn current(&mut self, key: K) -> Revision {
        if let Some(revision) = self.current.get(&key) {
            return *revision;
        }
        self.bump(key)
    }

    pub(crate) fn is_current(&self, key: &K, revision: Revision) -> bool {
        self.current.get(key) == Some(&revision)
    }

    /// The resource is gone: every outstanding revision becomes stale.
    pub(crate) fn forget(&mut self, key: &K) {
        self.current.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_revision_supersedes_older_regardless_of_completion_order() {
        let mut revisions = Revisions::default();
        let a = revisions.bump("dir");
        let b = revisions.bump("dir");
        // Response B arrives first, then A: only B is current either way.
        assert!(revisions.is_current(&"dir", b));
        assert!(!revisions.is_current(&"dir", a));
        assert_eq!(revisions.current("dir"), b);
    }

    #[test]
    fn forget_and_recreate_never_resurrects_a_stale_revision() {
        // Regression: per-key counters restarted at 1 after remove(), so the
        // first request of a re-added folder aliased the removed folder's.
        let mut revisions = Revisions::default();
        let stale = revisions.bump("dir");
        revisions.forget(&"dir");
        assert!(!revisions.is_current(&"dir", stale));
        let fresh = revisions.bump("dir");
        assert_ne!(stale, fresh);
        assert!(!revisions.is_current(&"dir", stale));
        assert!(revisions.is_current(&"dir", fresh));
    }

    #[test]
    fn keys_are_isolated_and_current_does_not_supersede() {
        let mut revisions = Revisions::default();
        let a = revisions.bump("A");
        let b = revisions.current("B");
        assert_eq!(revisions.current("B"), b);
        assert!(
            revisions.is_current(&"A", a),
            "B's activity leaves A current"
        );
        assert!(!revisions.is_current(&"A", b), "revisions never cross keys");
        // An input that changes B (live event) invalidates B's reads only.
        revisions.bump("B");
        assert!(!revisions.is_current(&"B", b));
        assert!(revisions.is_current(&"A", a));
    }

    #[test]
    fn disconnect_fences_snapshots_but_not_same_server_mutations() {
        let mut clock = RequestClock::default();
        let initial_generation = clock.generation();
        let initial_epoch = clock.epoch();
        clock.restart();
        assert_ne!(clock.generation(), initial_generation);
        assert_eq!(
            clock.epoch(),
            initial_epoch,
            "server restart and stream epoch are distinct inputs"
        );
        clock.connection_changed();
        assert_ne!(clock.epoch(), initial_epoch);
        let snapshot = clock.issue(RequestScope::Connection);
        let mutation = clock.issue(RequestScope::Server);
        clock.connection_changed(); // disconnect
        assert!(!clock.accepts(snapshot));
        assert!(clock.accepts(mutation));
        clock.connection_changed(); // reconnect
        assert!(!clock.accepts(snapshot));
        assert!(clock.accepts(clock.issue(RequestScope::Connection)));
        clock.restart();
        assert!(!clock.accepts(mutation));
    }
}
