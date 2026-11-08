//! Pending question forms and their draft answers.
//!
//! The request lifecycle mirrors [`super::permissions`] (asked/snapshot →
//! pending → reply or cancel Sending → closed, or Failed and retryable;
//! revisions superseded by every change and forgotten with the session).
//! What forms add is the draft:
//!
//! - seeded once from the fields' defaults when a form is first seen (by
//!   event or snapshot), and never reseeded while it stays pending, so the
//!   user's edits survive repeated asks and refreshes;
//! - removed when the form closes (replied, cancelled, missing from a
//!   snapshot) or its session is deleted.
//!
//! Text-input entities stay in the views; only committed answers live here.
//! No GPUI, I/O, tasks or time.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::ReplyState;
use crate::api::model::FormRequest;
use crate::domain::requests::{Freshness, Revision, Revisions};

#[derive(Default)]
pub(crate) struct Forms {
    /// May hold an empty list after the last form closed (see
    /// [`super::permissions`] for why).
    by_session: HashMap<String, Vec<FormRequest>>,
    revisions: Revisions<String>,
    replies: HashMap<(String, String), ReplyState>,
    /// Draft answers by form id (`answer[key] = value`).
    drafts: HashMap<String, Value>,
}

fn key(sid: &str, fid: &str) -> (String, String) {
    (sid.to_owned(), fid.to_owned())
}

fn default_answers(form: &FormRequest) -> Value {
    let mut draft = serde_json::Map::new();
    for field in &form.fields {
        if let Some(d) = &field.default {
            draft.insert(field.key.clone(), d.clone());
        }
    }
    Value::Object(draft)
}

impl Forms {
    /// The session's pending forms, oldest first.
    pub(crate) fn of_session(&self, sid: &str) -> &[FormRequest] {
        self.by_session.get(sid).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn has_pending(&self, sid: &str) -> bool {
        !self.of_session(sid).is_empty()
    }

    pub(crate) fn any_pending(&self) -> bool {
        self.by_session.values().any(|f| !f.is_empty())
    }

    /// Forms were shown for `sid` since its last snapshot.
    pub(crate) fn is_tracked(&self, sid: &str) -> bool {
        self.by_session.contains_key(sid)
    }

    fn is_pending(&self, sid: &str, fid: &str) -> bool {
        self.of_session(sid).iter().any(|f| f.id == fid)
    }

    pub(crate) fn is_sending(&self, sid: &str, fid: &str) -> bool {
        self.replies.get(&key(sid, fid)) == Some(&ReplyState::Sending)
    }

    pub(crate) fn reply_error(&self, sid: &str, fid: &str) -> Option<&str> {
        match self.replies.get(&key(sid, fid))? {
            ReplyState::Failed(e) => Some(e),
            ReplyState::Sending => None,
        }
    }

    pub(crate) fn revision(&mut self, sid: &str) -> Revision {
        self.revisions.current(sid.to_owned())
    }

    pub(crate) fn freshness(&self, sid: &str, revision: Revision) -> Freshness {
        self.revisions.freshness(&sid.to_owned(), revision)
    }

    /// The draft answers of a form (an empty object if none).
    pub(crate) fn draft(&self, fid: &str) -> Value {
        self.drafts
            .get(fid)
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }

    /// Record one answer. Returns whether the draft changed.
    pub(crate) fn set_answer(&mut self, fid: &str, field: &str, value: Value) -> bool {
        let entry = self
            .drafts
            .entry(fid.to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(serde_json::Map::new());
        }
        let map = entry.as_object_mut().expect("draft object");
        if map.get(field) == Some(&value) {
            return false;
        }
        map.insert(field.to_owned(), value);
        true
    }

    /// Install a snapshot of `sid`'s pending forms. Drafts of forms still
    /// pending (in any session) survive; new forms get their defaults.
    pub(crate) fn replace(&mut self, sid: &str, forms: Vec<FormRequest>) {
        let forms: Vec<_> = forms.into_iter().filter(|f| f.session_id == sid).collect();
        for f in &forms {
            self.drafts
                .entry(f.id.clone())
                .or_insert_with(|| default_answers(f));
        }
        self.replies
            .retain(|(s, fid), _| s != sid || forms.iter().any(|f| &f.id == fid));
        if forms.is_empty() {
            self.by_session.remove(sid);
        } else {
            self.by_session.insert(sid.to_owned(), forms);
        }
        let pending: HashSet<&str> = self
            .by_session
            .values()
            .flatten()
            .map(|f| f.id.as_str())
            .collect();
        self.drafts.retain(|fid, _| pending.contains(fid.as_str()));
        self.revisions.bump(sid.to_owned());
    }

    /// `form.asked` from the stream.
    pub(crate) fn asked(&mut self, form: FormRequest) {
        self.drafts
            .entry(form.id.clone())
            .or_insert_with(|| default_answers(&form));
        self.revisions.bump(form.session_id.clone());
        let forms = self.by_session.entry(form.session_id.clone()).or_default();
        forms.retain(|f| f.id != form.id);
        forms.push(form);
    }

    /// Replied or cancelled (stream, or our own request was accepted).
    pub(crate) fn closed(&mut self, sid: &str, fid: &str) {
        if let Some(forms) = self.by_session.get_mut(sid) {
            forms.retain(|f| f.id != fid);
        }
        self.replies.remove(&key(sid, fid));
        self.drafts.remove(fid);
        self.revisions.bump(sid.to_owned());
    }

    /// Admit a reply or cancellation; see `Permissions::begin_reply`.
    pub(crate) fn begin_reply(&mut self, sid: &str, fid: &str) -> bool {
        if !self.is_pending(sid, fid) || self.is_sending(sid, fid) {
            return false;
        }
        self.replies.insert(key(sid, fid), ReplyState::Sending);
        true
    }

    pub(crate) fn reply_failed(&mut self, sid: &str, fid: &str, error: String) {
        if self.is_pending(sid, fid) {
            self.replies
                .insert(key(sid, fid), ReplyState::Failed(error));
        } else {
            self.replies.remove(&key(sid, fid));
        }
    }

    pub(crate) fn abandon_replies(&mut self) -> bool {
        let before = self.replies.len();
        self.replies.retain(|_, s| *s != ReplyState::Sending);
        self.replies.len() != before
    }

    /// The session is gone: its forms, drafts and replies go with it.
    pub(crate) fn forget_session(&mut self, sid: &str) {
        for form in self.by_session.remove(sid).into_iter().flatten() {
            self.drafts.remove(&form.id);
        }
        self.replies.retain(|(s, _), _| s != sid);
        self.revisions.forget(&sid.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(sid: &str, fid: &str) -> FormRequest {
        FormRequest::from_json(&json!({
            "id": fid, "sessionID": sid, "title": "Q",
            "fields": [{"key": "q0", "type": "string", "default": "y"}]
        }))
        .unwrap()
    }

    #[test]
    fn drafts_are_seeded_once_and_survive_repeated_asks_and_snapshots() {
        let mut f = Forms::default();
        f.asked(form("A", "f1"));
        assert_eq!(f.draft("f1"), json!({"q0": "y"}));
        assert!(f.set_answer("f1", "q0", json!("n")));
        assert!(!f.set_answer("f1", "q0", json!("n")), "unchanged");
        f.asked(form("A", "f1"));
        f.replace("A", vec![form("A", "f1")]);
        assert_eq!(
            f.draft("f1"),
            json!({"q0": "n"}),
            "edits kept while pending"
        );
        assert_eq!(f.of_session("A").len(), 1);
    }

    #[test]
    fn closing_or_dropping_a_form_removes_its_draft_and_reply_state() {
        let mut f = Forms::default();
        f.replace("A", vec![form("A", "f1"), form("A", "f2"), form("B", "x")]);
        f.asked(form("B", "f3"));
        assert!(f.begin_reply("A", "f1"));
        f.closed("A", "f1");
        assert_eq!(f.draft("f1"), json!({}));
        assert!(!f.is_sending("A", "f1"));
        f.replace("A", vec![]);
        assert_eq!(f.draft("f2"), json!({}), "gone from the snapshot");
        assert_eq!(
            f.draft("x"),
            json!({}),
            "foreign entries never seeded a kept draft"
        );
        assert_eq!(
            f.draft("f3"),
            json!({"q0": "y"}),
            "other sessions' drafts stay"
        );
        f.forget_session("B");
        assert_eq!(f.draft("f3"), json!({}));
        assert!(!f.any_pending());
    }

    #[test]
    fn replies_and_cancellations_admit_one_at_a_time() {
        let mut f = Forms::default();
        f.asked(form("A", "f1"));
        assert!(f.begin_reply("A", "f1"));
        assert!(!f.begin_reply("A", "f1"));
        f.reply_failed("A", "f1", "boom".into());
        assert_eq!(f.reply_error("A", "f1"), Some("boom"));
        assert!(f.begin_reply("A", "f1"));
        assert!(f.abandon_replies());
        f.closed("A", "f1");
        f.reply_failed("A", "f1", "late".into());
        assert_eq!(f.reply_error("A", "f1"), None);
        assert!(f.is_tracked("A") && !f.has_pending("A"));
    }

    #[test]
    fn reads_overlapping_removal_are_forgotten() {
        let mut f = Forms::default();
        f.asked(form("A", "f1"));
        let read = f.revision("A");
        f.forget_session("A");
        assert_eq!(f.freshness("A", read), Freshness::Forgotten);
        f.asked(form("A", "f1"));
        assert_eq!(f.freshness("A", read), Freshness::Superseded);
    }
}
