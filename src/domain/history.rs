//! History availability and request ownership are one state machine. A refresh
//! keeps the loaded baseline usable; failure/cancellation releases its ticket.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoadTicket(u64);

/// Store-owned allocator: tickets are not reused when a conversation is evicted
/// or deleted and recreated. A ticket from another session cannot complete it.
#[derive(Default)]
pub(crate) struct HistoryRequests(u64);

impl HistoryRequests {
    pub(crate) fn issue(&mut self) -> LoadTicket {
        self.0 = self
            .0
            .checked_add(1)
            .expect("history request counter exhausted");
        LoadTicket(self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HistoryLoad {
    #[default]
    Unloaded,
    Loading(LoadTicket),
    Ready,
    Refreshing(LoadTicket),
}

impl HistoryLoad {
    pub(crate) fn loaded(self) -> bool {
        matches!(self, Self::Ready | Self::Refreshing(_))
    }

    pub(crate) fn fetching(self) -> bool {
        matches!(self, Self::Loading(_) | Self::Refreshing(_))
    }

    pub(crate) fn accepts(self, ticket: LoadTicket) -> bool {
        matches!(self, Self::Loading(current) | Self::Refreshing(current) if current == ticket)
    }

    pub(crate) fn begin(&mut self, ticket: LoadTicket) {
        *self = if self.loaded() {
            Self::Refreshing(ticket)
        } else {
            Self::Loading(ticket)
        };
    }

    pub(crate) fn ready(&mut self) {
        *self = Self::Ready;
    }

    /// Abandon the in-flight snapshot, preserving any previously loaded data.
    pub(crate) fn cancel(&mut self) {
        *self = if self.loaded() {
            Self::Ready
        } else {
            Self::Unloaded
        };
    }

    /// The cached projection may have missed events; selection must reload it.
    pub(crate) fn invalidate(&mut self) {
        *self = Self::Unloaded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_failure_retains_baseline_but_first_load_failure_does_not_invent_one() {
        let mut requests = HistoryRequests::default();
        for ready in [false, true] {
            let mut load = HistoryLoad::default();
            if ready {
                load.ready();
            }
            let ticket = requests.issue();
            load.begin(ticket);
            assert_eq!(load.loaded(), ready);
            assert!(load.fetching());
            load.cancel();
            assert_eq!(load.loaded(), ready);
            assert!(!load.fetching());
            assert!(!load.accepts(ticket));
        }
    }

    #[test]
    fn only_latest_in_flight_request_can_complete_in_either_completion_order() {
        for newest_first in [false, true] {
            let mut requests = HistoryRequests::default();
            let mut load = HistoryLoad::default();
            let first = requests.issue();
            load.begin(first);
            let second = requests.issue();
            load.begin(second);
            for ticket in if newest_first {
                [second, first]
            } else {
                [first, second]
            } {
                if load.accepts(ticket) {
                    load.ready();
                }
            }
            assert_eq!(load, HistoryLoad::Ready);
            assert!(
                !load.accepts(second),
                "duplicate completion is obsolete too"
            );
        }
    }

    #[test]
    fn bounded_event_sequences_match_an_independent_availability_and_ticket_model() {
        let mut requests = HistoryRequests::default();
        let tickets = [requests.issue(), requests.issue()];
        // All sequences of four commands, from both a cold and a cached state.
        // No random seed, runtime, clock, filesystem or window is involved.
        for cached in [false, true] {
            for sequence in 0..8usize.pow(4) {
                let mut load = if cached {
                    HistoryLoad::Ready
                } else {
                    HistoryLoad::Unloaded
                };
                let (mut loaded, mut pending) = (cached, None);
                let mut rest = sequence;
                for _ in 0..4 {
                    let command = rest % 8;
                    rest /= 8;
                    match command {
                        0 | 1 => {
                            let ticket = tickets[command];
                            load.begin(ticket);
                            pending = Some(ticket);
                        }
                        2..=5 => {
                            let ticket = tickets[command % 2];
                            assert_eq!(load.accepts(ticket), pending == Some(ticket));
                            if load.accepts(ticket) {
                                if command < 4 {
                                    load.ready();
                                } else {
                                    load.cancel();
                                }
                            }
                            if pending == Some(ticket) {
                                if command < 4 {
                                    loaded = true;
                                }
                                pending = None;
                            }
                        }
                        6 => {
                            load.cancel();
                            pending = None;
                        }
                        7 => {
                            load.invalidate();
                            loaded = false;
                            pending = None;
                        }
                        _ => unreachable!(),
                    }
                    assert_eq!(
                        load.loaded(),
                        loaded,
                        "sequence={sequence}, command={command}"
                    );
                    assert_eq!(load.fetching(), pending.is_some());
                }
            }
        }
    }

    #[test]
    fn invalidation_cancels_even_a_refresh_and_recreated_sessions_use_fresh_tickets() {
        let mut requests = HistoryRequests::default();
        let old = requests.issue();
        let mut load = HistoryLoad::Ready;
        load.begin(old);
        load.invalidate();
        assert!(!load.accepts(old));
        let mut recreated = HistoryLoad::default();
        recreated.begin(requests.issue());
        assert!(!recreated.accepts(old));
    }
}
