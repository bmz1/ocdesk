//! Pending permission requests: the one owner of their transitions.
//!
//! ```text
//! asked / snapshot ─▶ pending ─ begin_reply ─▶ Sending ─ reply_failed ─▶ Failed
//!                       │                         │                       │
//!                       └── replied / snapshot without it / forget_session ┘
//! ```
//!
//! Invariants (private fields):
//! - Requests are filed under their own session, in ask order; a repeated
//!   ask frame replaces the earlier copy instead of duplicating the card.
//! - A reply state exists only for a pending request, and is Sending or
//!   Failed, never both. Sending refuses a second reply.
//! - Every change bumps the session's [`Revisions`] entry, so a snapshot read
//!   issued earlier is superseded; [`Permissions::forget_session`] forgets it,
//!   so such a read reports [`Freshness::Forgotten`] instead of matching a
//!   re-created session's revision.
//! - A session entry may hold no requests after its last one was replied:
//!   it was shown since the last snapshot. That keeps it eligible for the
//!   turn-end re-read ([`Permissions::is_tracked`]); a snapshot with nothing
//!   pending removes the entry.
//!
//! No GPUI, I/O, tasks or time: the store emits notifications and the
//! controller owns HTTP, retries and the connection policy.

use std::collections::HashMap;

use super::ReplyState;
use crate::api::model::PermissionRequest;
use crate::domain::requests::{Freshness, Revision, Revisions};

#[derive(Default)]
pub(crate) struct Permissions {
    by_session: HashMap<String, Vec<PermissionRequest>>,
    revisions: Revisions<String>,
    replies: HashMap<(String, String), ReplyState>,
}

fn key(sid: &str, rid: &str) -> (String, String) {
    (sid.to_owned(), rid.to_owned())
}

impl Permissions {
    /// The session's pending requests, oldest first.
    pub(crate) fn of_session(&self, sid: &str) -> &[PermissionRequest] {
        self.by_session.get(sid).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn has_pending(&self, sid: &str) -> bool {
        !self.of_session(sid).is_empty()
    }

    pub(crate) fn any_pending(&self) -> bool {
        self.by_session.values().any(|r| !r.is_empty())
    }

    /// Requests were shown for `sid` since its last snapshot (possibly all
    /// replied since): worth re-reading when its turn ends.
    pub(crate) fn is_tracked(&self, sid: &str) -> bool {
        self.by_session.contains_key(sid)
    }

    fn is_pending(&self, sid: &str, rid: &str) -> bool {
        self.of_session(sid).iter().any(|r| r.id == rid)
    }

    pub(crate) fn is_sending(&self, sid: &str, rid: &str) -> bool {
        self.replies.get(&key(sid, rid)) == Some(&ReplyState::Sending)
    }

    pub(crate) fn reply_error(&self, sid: &str, rid: &str) -> Option<&str> {
        match self.replies.get(&key(sid, rid))? {
            ReplyState::Failed(e) => Some(e),
            ReplyState::Sending => None,
        }
    }

    /// The revision a snapshot read of `sid` must still match when it lands.
    pub(crate) fn revision(&mut self, sid: &str) -> Revision {
        self.revisions.current(sid.to_owned())
    }

    pub(crate) fn freshness(&self, sid: &str, revision: Revision) -> Freshness {
        self.revisions.freshness(&sid.to_owned(), revision)
    }

    /// Install a snapshot of `sid`'s pending requests; other sessions'
    /// requests in it are ignored, replies to requests now gone are dropped.
    pub(crate) fn replace(&mut self, sid: &str, requests: Vec<PermissionRequest>) {
        let requests: Vec<_> = requests
            .into_iter()
            .filter(|r| r.session_id == sid)
            .collect();
        self.replies
            .retain(|(s, rid), _| s != sid || requests.iter().any(|r| &r.id == rid));
        if requests.is_empty() {
            self.by_session.remove(sid);
        } else {
            self.by_session.insert(sid.to_owned(), requests);
        }
        self.revisions.bump(sid.to_owned());
    }

    /// `permission.asked` from the stream.
    pub(crate) fn asked(&mut self, request: PermissionRequest) {
        self.revisions.bump(request.session_id.clone());
        let requests = self
            .by_session
            .entry(request.session_id.clone())
            .or_default();
        requests.retain(|r| r.id != request.id);
        requests.push(request);
    }

    /// `permission.replied` from the stream, or our own reply was accepted.
    /// Always supersedes in-flight snapshots: one may predate the reply.
    pub(crate) fn replied(&mut self, sid: &str, rid: &str) {
        if let Some(requests) = self.by_session.get_mut(sid) {
            requests.retain(|r| r.id != rid);
        }
        self.replies.remove(&key(sid, rid));
        self.revisions.bump(sid.to_owned());
    }

    /// Admit a reply: refused when the request isn't pending or a reply to
    /// it is in flight. A previous failure is cleared.
    pub(crate) fn begin_reply(&mut self, sid: &str, rid: &str) -> bool {
        if !self.is_pending(sid, rid) || self.is_sending(sid, rid) {
            return false;
        }
        self.replies.insert(key(sid, rid), ReplyState::Sending);
        true
    }

    /// The reply failed: shown, and retryable, only while still pending.
    pub(crate) fn reply_failed(&mut self, sid: &str, rid: &str, error: String) {
        if self.is_pending(sid, rid) {
            self.replies
                .insert(key(sid, rid), ReplyState::Failed(error));
        } else {
            self.replies.remove(&key(sid, rid));
        }
    }

    /// Replies in flight will never report back (disconnect, restart): stop
    /// showing them as sending. Shown failures stay. Returns whether any did.
    pub(crate) fn abandon_replies(&mut self) -> bool {
        let before = self.replies.len();
        self.replies.retain(|_, s| *s != ReplyState::Sending);
        self.replies.len() != before
    }

    /// The session is gone: drop its requests and replies; reads issued for
    /// it become [`Freshness::Forgotten`].
    pub(crate) fn forget_session(&mut self, sid: &str) {
        self.by_session.remove(sid);
        self.replies.retain(|(s, _), _| s != sid);
        self.revisions.forget(&sid.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(sid: &str, rid: &str) -> PermissionRequest {
        PermissionRequest::from_json(&serde_json::json!({
            "id": rid, "sessionID": sid, "action": "shell", "resources": ["ls"]
        }))
        .unwrap()
    }

    fn ids(p: &Permissions, sid: &str) -> Vec<String> {
        p.of_session(sid).iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn duplicate_asks_keep_one_card_in_ask_order_and_sessions_stay_isolated() {
        let mut p = Permissions::default();
        p.asked(req("A", "1"));
        p.asked(req("A", "2"));
        p.asked(req("B", "3"));
        p.asked(req("A", "1"));
        assert_eq!(ids(&p, "A"), ["2", "1"]);
        assert_eq!(ids(&p, "B"), ["3"]);
        p.replied("A", "2");
        p.replied("A", "1");
        assert!(!p.has_pending("A"));
        assert!(
            p.is_tracked("A"),
            "replied-out sessions stay eligible for a re-read"
        );
        assert!(p.any_pending(), "B still waits");
    }

    #[test]
    fn snapshots_filter_foreign_requests_and_prune_reply_state() {
        let mut p = Permissions::default();
        p.replace("A", vec![req("A", "1"), req("B", "9"), req("A", "2")]);
        assert_eq!(ids(&p, "A"), ["1", "2"]);
        assert!(!p.is_tracked("B"), "a foreign request is not filed");
        assert!(p.begin_reply("A", "1"));
        assert!(p.begin_reply("A", "2"));
        p.replace("A", vec![req("A", "2")]);
        assert!(
            !p.is_sending("A", "1"),
            "gone from the snapshot: no reply state"
        );
        assert!(p.is_sending("A", "2"));
        p.replace("A", vec![]);
        assert!(!p.is_tracked("A"), "an empty snapshot removes the entry");
    }

    #[test]
    fn one_reply_at_a_time_and_failures_only_while_pending() {
        let mut p = Permissions::default();
        assert!(!p.begin_reply("A", "1"), "nothing pending");
        p.asked(req("A", "1"));
        assert!(p.begin_reply("A", "1"));
        assert!(!p.begin_reply("A", "1"), "duplicate admission refused");
        p.reply_failed("A", "1", "boom".into());
        assert_eq!(p.reply_error("A", "1"), Some("boom"));
        assert!(!p.is_sending("A", "1"));
        assert!(p.begin_reply("A", "1"), "a failed reply is retryable");
        assert_eq!(p.reply_error("A", "1"), None);
    }

    #[test]
    fn stream_closure_before_http_completion_then_late_failure_shows_nothing() {
        let mut p = Permissions::default();
        p.asked(req("A", "1"));
        assert!(p.begin_reply("A", "1"));
        p.replied("A", "1"); // SSE permission.replied arrives first
        assert!(!p.is_sending("A", "1"));
        p.reply_failed("A", "1", "late".into());
        assert_eq!(p.reply_error("A", "1"), None);
    }

    #[test]
    fn late_failure_after_session_removal_shows_nothing() {
        let mut p = Permissions::default();
        p.asked(req("A", "1"));
        assert!(p.begin_reply("A", "1"));
        p.forget_session("A");
        p.reply_failed("A", "1", "late".into());
        assert_eq!(p.reply_error("A", "1"), None);
        assert!(!p.is_tracked("A"));
    }

    #[test]
    fn disconnect_abandons_sending_and_keeps_failures() {
        let mut p = Permissions::default();
        p.replace("A", vec![req("A", "1"), req("A", "2")]);
        assert!(p.begin_reply("A", "1"));
        assert!(p.begin_reply("A", "2"));
        p.reply_failed("A", "2", "boom".into());
        assert!(p.abandon_replies());
        assert!(!p.abandon_replies(), "nothing more to abandon");
        assert!(p.begin_reply("A", "1"));
        assert_eq!(p.reply_error("A", "2"), Some("boom"));
    }

    #[test]
    fn reads_are_superseded_by_changes_and_forgotten_with_the_session() {
        let mut p = Permissions::default();
        let read = p.revision("A");
        assert_eq!(p.freshness("A", read), Freshness::Current);
        p.asked(req("A", "1"));
        assert_eq!(p.freshness("A", read), Freshness::Superseded);
        let read = p.revision("A");
        p.replied("A", "x");
        assert_eq!(
            p.freshness("A", read),
            Freshness::Superseded,
            "a reply may race the read"
        );
        // Removed during the read: drop it, success or failure, no re-read.
        let read = p.revision("A");
        p.forget_session("A");
        assert_eq!(p.freshness("A", read), Freshness::Forgotten);
        // Re-created: the old read is stale, and never current again.
        p.asked(req("A", "1"));
        assert_eq!(p.freshness("A", read), Freshness::Superseded);
        let fresh = p.revision("A");
        assert_eq!(p.freshness("A", fresh), Freshness::Current);
    }
}
