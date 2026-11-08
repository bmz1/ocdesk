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

#[cfg(test)]
mod tests {
    use super::*;

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
