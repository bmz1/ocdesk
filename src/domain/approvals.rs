//! Server requests that wait on the user: permission requests and question
//! forms. Both share one lifecycle, so one mechanism owns it:
//!
//! ```text
//!            asked / snapshot                begin_reply
//!   (absent) ───────────────▶ pending ─────────────────────▶ pending + Sending
//!                               ▲  │ resolved (event, own reply,      │
//!                               │  │ snapshot without it,             │ reply_failed
//!                               │  ▼ session forgotten)               ▼
//!                               │ (absent)                    pending + Failed(error)
//!                               └──────── abandon_replies (Sending only) ┘
//! ```
//!
//! Invariants, enforced by private fields:
//! - A session key exists only while it has a pending request (no empty lists),
//!   every request is filed under its own session, and ids are unique per session.
//! - A reply state exists only for a pending request. "Sending" and "failed"
//!   are one enum, so a request can't be both (the old `sending: HashSet` plus
//!   `errors: HashMap` pair relied on every caller clearing the other).
//! - Every change to a session's requests supersedes snapshot reads issued
//!   before it ([`Revisions`]); forgetting a session makes them stale forever,
//!   so a read in flight across delete/re-create can't alias a fresh one.
//!
//! The server stays authoritative: nothing here replies, retries or approves
//! anything on its own; [`PendingRequests::abandon_replies`] only stops showing
//! a reply whose outcome will never be reported (disconnect, restart).

use std::collections::HashMap;

use super::requests::{Freshness, Revision, Revisions};

/// What the lifecycle needs to know about a server request.
pub(crate) trait PendingRequest {
    fn id(&self) -> &str;
    fn session_id(&self) -> &str;
}

/// The user's reply to one pending request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplyState {
    /// The reply is in flight; further replies are refused.
    Sending,
    /// The reply failed; the request stays visible and can be answered again.
    Failed(String),
}

type Key = (String, String);

pub(crate) struct PendingRequests<R> {
    by_session: HashMap<String, Vec<R>>,
    revisions: Revisions<String>,
    replies: HashMap<Key, ReplyState>,
}

impl<R> Default for PendingRequests<R> {
    fn default() -> Self {
        Self {
            by_session: HashMap::new(),
            revisions: Revisions::default(),
            replies: HashMap::new(),
        }
    }
}

fn key(sid: &str, rid: &str) -> Key {
    (sid.to_owned(), rid.to_owned())
}

impl<R: PendingRequest> PendingRequests<R> {
    /// The session's pending requests, oldest first.
    pub(crate) fn of_session(&self, sid: &str) -> &[R] {
        self.by_session.get(sid).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn has_pending(&self, sid: &str) -> bool {
        self.by_session.contains_key(sid)
    }

    pub(crate) fn any_pending(&self) -> bool {
        !self.by_session.is_empty()
    }

    pub(crate) fn is_pending(&self, sid: &str, rid: &str) -> bool {
        self.of_session(sid).iter().any(|r| r.id() == rid)
    }

    /// Every pending request of every session.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &R> {
        self.by_session.values().flatten()
    }

    /// The revision a snapshot read of `sid` must still match when it lands.
    pub(crate) fn revision(&mut self, sid: &str) -> Revision {
        self.revisions.current(sid.to_owned())
    }

    /// See [`Freshness`]: `Forgotten` means the session was removed while
    /// the read was in flight; its result must neither install state nor
    /// trigger another read.
    pub(crate) fn freshness(&self, sid: &str, revision: Revision) -> Freshness {
        self.revisions.freshness(&sid.to_owned(), revision)
    }

    /// Install a snapshot of `sid`'s pending requests. Requests of other
    /// sessions are ignored; replies to requests no longer pending go away.
    pub(crate) fn replace(&mut self, sid: &str, requests: Vec<R>) {
        let mut mine: Vec<R> = Vec::with_capacity(requests.len());
        for r in requests {
            if r.session_id() == sid && !mine.iter().any(|m| m.id() == r.id()) {
                mine.push(r);
            }
        }
        self.replies
            .retain(|(s, rid), _| s != sid || mine.iter().any(|r| r.id() == rid));
        if mine.is_empty() {
            self.by_session.remove(sid);
        } else {
            self.by_session.insert(sid.to_owned(), mine);
        }
        self.revisions.bump(sid.to_owned());
    }

    /// A live "asked" event. A repeated frame replaces the earlier copy
    /// instead of showing the request twice.
    pub(crate) fn asked(&mut self, request: R) {
        let sid = request.session_id().to_owned();
        self.revisions.bump(sid.clone());
        let requests = self.by_session.entry(sid).or_default();
        requests.retain(|r| r.id() != request.id());
        requests.push(request);
    }

    /// The request is answered or withdrawn. Returns it if it was pending.
    /// Always supersedes in-flight snapshots: one may predate the answer.
    pub(crate) fn resolved(&mut self, sid: &str, rid: &str) -> Option<R> {
        self.revisions.bump(sid.to_owned());
        self.replies.remove(&key(sid, rid));
        let requests = self.by_session.get_mut(sid)?;
        let index = requests.iter().position(|r| r.id() == rid)?;
        let removed = requests.remove(index);
        if requests.is_empty() {
            self.by_session.remove(sid);
        }
        Some(removed)
    }

    /// Start a reply. Refused (false) when the request is not pending or a
    /// reply to it is already in flight; a previous failure is cleared.
    pub(crate) fn begin_reply(&mut self, sid: &str, rid: &str) -> bool {
        if !self.is_pending(sid, rid) || self.is_sending(sid, rid) {
            return false;
        }
        self.replies.insert(key(sid, rid), ReplyState::Sending);
        true
    }

    /// The reply failed. Shown only while the request is still pending.
    pub(crate) fn reply_failed(&mut self, sid: &str, rid: &str, error: String) {
        if self.is_pending(sid, rid) {
            self.replies
                .insert(key(sid, rid), ReplyState::Failed(error));
        } else {
            self.replies.remove(&key(sid, rid));
        }
    }

    pub(crate) fn reply_state(&self, sid: &str, rid: &str) -> Option<&ReplyState> {
        self.replies.get(&key(sid, rid))
    }

    pub(crate) fn is_sending(&self, sid: &str, rid: &str) -> bool {
        self.reply_state(sid, rid) == Some(&ReplyState::Sending)
    }

    pub(crate) fn reply_error(&self, sid: &str, rid: &str) -> Option<&str> {
        match self.reply_state(sid, rid)? {
            ReplyState::Failed(e) => Some(e),
            ReplyState::Sending => None,
        }
    }

    /// The outcome of every reply in flight will never be reported (the
    /// stream or the server it was sent to is gone): stop showing them as
    /// sending. Shown failures stay. Returns whether anything changed.
    pub(crate) fn abandon_replies(&mut self) -> bool {
        let before = self.replies.len();
        self.replies
            .retain(|_, state| *state != ReplyState::Sending);
        self.replies.len() != before
    }

    /// The session is gone: drop its requests and replies, and make every
    /// snapshot read issued for it stale. Returns the dropped requests.
    pub(crate) fn forget_session(&mut self, sid: &str) -> Vec<R> {
        self.revisions.forget(&sid.to_owned());
        self.replies.retain(|(s, _), _| s != sid);
        self.by_session.remove(sid).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Req(&'static str, &'static str);

    impl PendingRequest for Req {
        fn id(&self) -> &str {
            self.1
        }
        fn session_id(&self) -> &str {
            self.0
        }
    }

    fn ids(p: &PendingRequests<Req>, sid: &str) -> Vec<&'static str> {
        p.of_session(sid).iter().map(|r| r.1).collect()
    }

    #[test]
    fn duplicate_asks_and_foreign_snapshot_entries_never_duplicate_or_leak() {
        let mut p = PendingRequests::default();
        p.asked(Req("A", "1"));
        p.asked(Req("A", "2"));
        p.asked(Req("A", "1"));
        assert_eq!(
            ids(&p, "A"),
            ["2", "1"],
            "a repeated ask moves, not doubles"
        );
        p.replace("B", vec![Req("B", "3"), Req("A", "4"), Req("B", "3")]);
        assert_eq!(ids(&p, "B"), ["3"]);
        assert_eq!(ids(&p, "A"), ["2", "1"], "B's snapshot can't touch A");
        p.replace("B", vec![]);
        assert!(!p.has_pending("B"), "no empty entries");
        assert!(p.any_pending());
        assert_eq!(p.iter().count(), 2);
    }

    #[test]
    fn a_reply_is_sending_or_failed_never_both_and_only_while_pending() {
        let mut p = PendingRequests::default();
        assert!(!p.begin_reply("A", "1"), "nothing to reply to");
        p.asked(Req("A", "1"));
        assert!(p.begin_reply("A", "1"));
        assert!(!p.begin_reply("A", "1"), "one reply in flight");
        p.reply_failed("A", "1", "boom".into());
        assert!(!p.is_sending("A", "1"));
        assert_eq!(p.reply_error("A", "1"), Some("boom"));
        assert!(p.begin_reply("A", "1"), "a failed reply can be retried");
        assert_eq!(p.reply_error("A", "1"), None, "retrying clears the error");
        // The stream reports the answer before the HTTP reply returns.
        assert_eq!(p.resolved("A", "1"), Some(Req("A", "1")));
        assert_eq!(p.reply_state("A", "1"), None);
        p.reply_failed("A", "1", "late".into());
        assert_eq!(
            p.reply_state("A", "1"),
            None,
            "a late failure for a resolved request shows nothing"
        );
    }

    #[test]
    fn snapshots_prune_replies_to_requests_that_are_gone() {
        let mut p = PendingRequests::default();
        p.replace("A", vec![Req("A", "1"), Req("A", "2")]);
        assert!(p.begin_reply("A", "1"));
        assert!(p.begin_reply("A", "2"));
        p.replace("A", vec![Req("A", "2")]);
        assert_eq!(p.reply_state("A", "1"), None);
        assert!(p.is_sending("A", "2"), "still pending: still sending");
    }

    #[test]
    fn disconnect_abandons_sending_but_keeps_shown_failures() {
        let mut p = PendingRequests::default();
        p.replace("A", vec![Req("A", "1"), Req("A", "2")]);
        assert!(p.begin_reply("A", "1"));
        assert!(p.begin_reply("A", "2"));
        p.reply_failed("A", "2", "boom".into());
        assert!(p.abandon_replies());
        assert!(!p.abandon_replies(), "idempotent");
        assert!(!p.is_sending("A", "1"));
        assert!(p.begin_reply("A", "1"), "the user can answer again");
        assert_eq!(p.reply_error("A", "2"), Some("boom"));
    }

    #[test]
    fn every_change_supersedes_snapshot_reads_issued_before_it() {
        let mut p = PendingRequests::default();
        let read = p.revision("A");
        assert!((p.freshness("A", read) == Freshness::Current));
        assert_eq!(p.revision("A"), read, "issuing a read supersedes nothing");
        p.asked(Req("A", "1"));
        assert!(!(p.freshness("A", read) == Freshness::Current), "asked");
        let read = p.revision("A");
        p.resolved("A", "unknown");
        assert!(
            !(p.freshness("A", read) == Freshness::Current),
            "a reply may race the read"
        );
        let read = p.revision("A");
        let other = p.revision("B");
        p.replace("A", vec![]);
        assert!(
            !(p.freshness("A", read) == Freshness::Current),
            "a newer snapshot wins"
        );
        assert!(
            (p.freshness("B", other) == Freshness::Current),
            "sessions are isolated"
        );
    }

    #[test]
    fn forgotten_session_reads_stay_stale_after_it_comes_back() {
        // Regression: per-session u64 counters were removed with the session
        // and restarted at 0, so a read issued before the removal matched
        // the re-created session's revision after the same number of events.
        let mut p = PendingRequests::default();
        p.asked(Req("A", "1"));
        let stale = p.revision("A");
        assert!(p.begin_reply("A", "1"));
        assert_eq!(p.forget_session("A"), vec![Req("A", "1")]);
        assert!(!p.has_pending("A") && p.reply_state("A", "1").is_none());
        p.asked(Req("A", "1"));
        let fresh = p.revision("A");
        assert_ne!(stale, fresh);
        assert!(!(p.freshness("A", stale) == Freshness::Current));
        assert!(!p.is_sending("A", "1"), "the old reply did not come back");
    }

    #[test]
    fn a_read_overlapping_removal_is_dropped_but_one_after_recreation_rereads() {
        let mut p: PendingRequests<Req> = PendingRequests::default();
        p.asked(Req("A", "1"));
        let read = p.revision("A");
        p.forget_session("A");
        assert_eq!(p.freshness("A", read), Freshness::Forgotten);
        assert!(!p.has_pending("A"), "nothing reinstalled");
        // The session comes back (a live ask) while the old read is still out.
        p.asked(Req("A", "2"));
        assert_eq!(p.freshness("A", read), Freshness::Superseded);
        let fresh = p.revision("A");
        assert_eq!(p.freshness("A", fresh), Freshness::Current);
    }

    /// Reference model: a small set of sessions/requests and every ordering
    /// of five commands. The invariants must hold after each step.
    #[test]
    fn invariants_hold_for_every_short_command_sequence() {
        #[derive(Clone, Copy, Debug)]
        enum Cmd {
            Ask(&'static str, &'static str),
            Resolve(&'static str, &'static str),
            Begin(&'static str, &'static str),
            Fail(&'static str, &'static str),
            Snapshot(&'static str, &'static [&'static str]),
            Abandon,
            Forget(&'static str),
        }
        const CMDS: &[Cmd] = &[
            Cmd::Ask("A", "1"),
            Cmd::Ask("B", "2"),
            Cmd::Resolve("A", "1"),
            Cmd::Begin("A", "1"),
            Cmd::Begin("B", "2"),
            Cmd::Fail("A", "1"),
            Cmd::Snapshot("A", &["1", "3"]),
            Cmd::Snapshot("B", &[]),
            Cmd::Abandon,
            Cmd::Forget("A"),
        ];
        let n = CMDS.len();
        let mut sequences = 0;
        for mut code in 0..n.pow(5) {
            let mut p = PendingRequests::default();
            let mut model: Vec<(&str, &str)> = Vec::new();
            for _ in 0..5 {
                let cmd = CMDS[code % n];
                code /= n;
                match cmd {
                    Cmd::Ask(s, r) => {
                        p.asked(Req(s, r));
                        model.retain(|x| *x != (s, r));
                        model.push((s, r));
                    }
                    Cmd::Resolve(s, r) => {
                        let was = model.contains(&(s, r));
                        assert_eq!(p.resolved(s, r).is_some(), was);
                        model.retain(|x| *x != (s, r));
                    }
                    Cmd::Begin(s, r) => {
                        let allowed = model.contains(&(s, r)) && !p.is_sending(s, r);
                        assert_eq!(p.begin_reply(s, r), allowed);
                    }
                    Cmd::Fail(s, r) => p.reply_failed(s, r, "e".into()),
                    Cmd::Snapshot(s, rs) => {
                        p.replace(s, rs.iter().map(|r| Req(s, r)).collect());
                        model.retain(|x| x.0 != s);
                        model.extend(rs.iter().map(|r| (s, *r)));
                    }
                    Cmd::Abandon => {
                        p.abandon_replies();
                        assert!(p.replies.values().all(|r| *r != ReplyState::Sending));
                    }
                    Cmd::Forget(s) => {
                        p.forget_session(s);
                        model.retain(|x| x.0 != s);
                    }
                }
                // Same pending set as the model.
                for s in ["A", "B"] {
                    let mut want: Vec<&str> =
                        model.iter().filter(|x| x.0 == s).map(|x| x.1).collect();
                    let mut got = ids(&p, s);
                    want.sort_unstable();
                    got.sort_unstable();
                    assert_eq!(got, want, "pending set of {s} after {cmd:?}");
                    assert_eq!(p.has_pending(s), !want.is_empty());
                }
                // Reply state only for pending requests.
                for (s, r) in p.replies.keys() {
                    assert!(p.is_pending(s, r), "orphan reply state {s}/{r}");
                }
                assert!(p.by_session.values().all(|v| !v.is_empty()));
            }
            sequences += 1;
        }
        assert_eq!(sequences, 100_000);
    }
}
