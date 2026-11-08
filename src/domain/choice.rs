//! Optimistic per-session selections (model, agent).
//!
//! The controller shows a new choice immediately and asks the server to make
//! it. Several requests for one session can overlap (the user clicks through
//! the picker), and their HTTP completions can arrive in any order. This
//! machine decides what a completion may do to the displayed value:
//!
//! - Only the *latest* choice of a session may roll the display back. An
//!   older choice failing must not clobber a newer one.
//! - The rollback target is the last value the server is known to hold: the
//!   value shown before the first outstanding choice, replaced by anything the
//!   stream reports ([`OptimisticChoices::observed`]) or an older request that
//!   succeeded meanwhile.
//! - The event stream stays authoritative. It overwrites the display directly
//!   whatever this machine thinks, so a server that applies overlapping
//!   requests out of order still converges to its own final value.
//!
//! Entries are dropped when the newest choice settles, the session goes away
//! ([`OptimisticChoices::forget`]) or the server restarts
//! ([`OptimisticChoices::clear`]), because completions from an obsolete server
//! are discarded before they reach [`OptimisticChoices::failed`].

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChoiceToken(u64);

struct Pending<V> {
    latest: ChoiceToken,
    /// Last value the server is known to hold; `None` if it never had one.
    fallback: Option<V>,
}

pub(crate) struct OptimisticChoices<V> {
    next: u64,
    pending: HashMap<String, Pending<V>>,
}

impl<V> Default for OptimisticChoices<V> {
    fn default() -> Self {
        Self {
            next: 0,
            pending: HashMap::new(),
        }
    }
}

impl<V: Clone> OptimisticChoices<V> {
    /// The user chose a value for `session` while `shown` was displayed.
    pub(crate) fn choose(&mut self, session: &str, shown: Option<V>) -> ChoiceToken {
        self.next = self.next.checked_add(1).expect("choice counter exhausted");
        let latest = ChoiceToken(self.next);
        match self.pending.get_mut(session) {
            // `shown` is an optimistic value of an outstanding choice; the
            // server-known fallback from before it is still the right target.
            Some(p) => p.latest = latest,
            None => {
                self.pending.insert(
                    session.to_owned(),
                    Pending {
                        latest,
                        fallback: shown,
                    },
                );
            }
        }
        latest
    }

    /// The stream reported the server's value for `session`.
    pub(crate) fn observed(&mut self, session: &str, value: &V) {
        if let Some(p) = self.pending.get_mut(session) {
            p.fallback = Some(value.clone());
        }
    }

    /// The server accepted the choice `token` (whose value was `value`).
    pub(crate) fn succeeded(&mut self, session: &str, token: ChoiceToken, value: V) {
        let Some(p) = self.pending.get_mut(session) else {
            return;
        };
        if p.latest == token {
            self.pending.remove(session);
        } else {
            p.fallback = Some(value);
        }
    }

    /// The server rejected the choice `token`. Returns the value to display
    /// instead, or `None` to leave the display alone: a newer choice owns it,
    /// or there is nothing known to roll back to.
    pub(crate) fn failed(&mut self, session: &str, token: ChoiceToken) -> Option<V> {
        if self.pending.get(session)?.latest != token {
            return None;
        }
        self.pending.remove(session)?.fallback
    }

    pub(crate) fn forget(&mut self, session: &str) {
        self.pending.remove(session);
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    #[cfg(test)]
    fn is_pending(&self, session: &str) -> bool {
        self.pending.contains_key(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny display + machine pair, applying results like the controller.
    struct Harness {
        choices: OptimisticChoices<&'static str>,
        shown: Option<&'static str>,
    }

    impl Harness {
        fn new(shown: Option<&'static str>) -> Self {
            Self {
                choices: OptimisticChoices::default(),
                shown,
            }
        }
        fn choose(&mut self, value: &'static str) -> ChoiceToken {
            let token = self.choices.choose("s", self.shown);
            self.shown = Some(value);
            token
        }
        fn fail(&mut self, token: ChoiceToken) {
            if let Some(back) = self.choices.failed("s", token) {
                self.shown = Some(back);
            }
        }
        fn stream(&mut self, value: &'static str) {
            self.choices.observed("s", &value);
            self.shown = Some(value);
        }
    }

    #[test]
    fn older_failure_never_clobbers_a_newer_choice() {
        // Regression: each request captured the then-shown value and rolled
        // back to it unconditionally, so A's late failure replaced B with P.
        let mut h = Harness::new(Some("P"));
        let a = h.choose("A");
        let b = h.choose("B");
        h.fail(a);
        assert_eq!(h.shown, Some("B"));
        h.choices.succeeded("s", b, "B");
        assert_eq!(h.shown, Some("B"));
        assert!(!h.choices.is_pending("s"));
    }

    #[test]
    fn newest_failure_rolls_back_past_failed_intermediate_choices() {
        // Rolling back to A would show a value the server refused.
        let mut h = Harness::new(Some("P"));
        let a = h.choose("A");
        let b = h.choose("B");
        h.fail(a);
        h.fail(b);
        assert_eq!(h.shown, Some("P"));
    }

    #[test]
    fn newest_failure_rolls_back_to_an_accepted_older_choice() {
        let mut h = Harness::new(Some("P"));
        let a = h.choose("A");
        let b = h.choose("B");
        h.choices.succeeded("s", a, "A");
        h.fail(b);
        assert_eq!(h.shown, Some("A"));
    }

    #[test]
    fn rollback_targets_the_value_the_stream_last_reported() {
        // Another client changed the session while our request was in flight.
        let mut h = Harness::new(Some("P"));
        let a = h.choose("A");
        h.stream("Q");
        h.fail(a);
        assert_eq!(h.shown, Some("Q"));
    }

    #[test]
    fn nothing_to_roll_back_to_leaves_the_display() {
        let mut h = Harness::new(None);
        let a = h.choose("A");
        h.fail(a);
        assert_eq!(h.shown, Some("A"));
        assert!(!h.choices.is_pending("s"));
    }

    #[test]
    fn duplicate_and_forgotten_completions_are_inert() {
        let mut h = Harness::new(Some("P"));
        let a = h.choose("A");
        h.fail(a);
        assert_eq!(h.shown, Some("P"));
        let b = h.choose("B");
        h.fail(a); // duplicate of an already-settled failure
        assert_eq!(h.shown, Some("B"));
        h.choices.forget("s");
        h.fail(b); // session deleted before its request settled
        assert_eq!(h.shown, Some("B"));
    }

    #[test]
    fn sessions_are_isolated() {
        let mut choices = OptimisticChoices::default();
        let a = choices.choose("A", Some("p"));
        let b = choices.choose("B", Some("q"));
        assert_eq!(choices.failed("B", a), None, "A's token cannot settle B");
        assert_eq!(choices.failed("A", a), Some("p"));
        assert_eq!(choices.failed("B", b), Some("q"));
    }

    #[test]
    fn restart_drops_outstanding_choices() {
        let mut choices = OptimisticChoices::default();
        choices.choose("s", Some("P"));
        choices.clear();
        // The first choice after a restart captures what is shown then.
        let token = choices.choose("s", Some("R"));
        assert_eq!(choices.failed("s", token), Some("R"));
    }

    /// Exhaustive check over every completion order of three overlapping
    /// choices, each succeeding or failing. A response arrives after the
    /// server applied it, so the server's value is the last success to
    /// arrive. Up to the moment the newest choice settles, the display must
    /// match it. Successes arriving after that are the stream's to report.
    #[test]
    fn every_completion_order_settles_on_the_server_value() {
        const VALUES: [&str; 3] = ["A", "B", "C"];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for order in orders {
            for outcomes in 0..8u8 {
                let ok = |i: usize| outcomes & (1 << i) != 0;
                let mut h = Harness::new(Some("P"));
                let tokens: Vec<_> = VALUES.iter().map(|v| h.choose(v)).collect();
                let mut server = "P";
                for &i in &order {
                    if ok(i) {
                        server = VALUES[i];
                        h.choices.succeeded("s", tokens[i], VALUES[i]);
                    } else {
                        h.fail(tokens[i]);
                    }
                    if i == 2 {
                        break;
                    }
                }
                assert_eq!(
                    h.shown,
                    Some(server),
                    "order {order:?} outcomes {outcomes:03b}"
                );
                assert!(
                    !h.choices.is_pending("s"),
                    "the newest settling releases state"
                );
            }
        }
    }
}
