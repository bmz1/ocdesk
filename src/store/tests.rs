use super::history::HistoryChange;
use super::*;
use crate::api::model::Delivery;

fn fixture() -> Vec<Envelope> {
    include_str!("../../tests/fixtures/v2-events.jsonl")
        .lines()
        .filter_map(Envelope::from_json)
        .collect()
}

#[test]
fn forget_folder_reports_worktree_checkouts_it_dropped() {
    let mut s = Store::default();
    let folder = PathBuf::from("/repo");
    let tree = PathBuf::from("/repo-wt");
    s.worktrees.insert(folder.clone(), vec![tree.clone()]);
    s.diff_status.begin(tree.clone());
    s.diff_stale.insert(folder.clone());
    let forgotten = s.forget_folder(&folder);
    assert_eq!(forgotten, vec![tree.clone(), folder.clone()]);
    assert!(!s.diff_status.is_loading(&tree));
    assert!(!s.diff_stale.contains(&folder));
    assert!(!s.worktrees.contains_key(&folder));
}

fn sid() -> String {
    fixture()
        .into_iter()
        .find_map(|e| match e.event {
            Event::SessionUpserted(s) => Some(s.id),
            _ => None,
        })
        .unwrap()
}

fn session_in(id: &str, dir: &str, updated: u64) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        title: String::new(),
        directory: PathBuf::from(dir),
        created: updated,
        updated,
        parent_id: None,
        model: None,
        agent: None,
    }
}

fn wire(json: &str) -> Envelope {
    Envelope::from_json(json).unwrap()
}

#[test]
fn deleting_a_session_forgets_it_and_its_subagents_everywhere() {
    let mut s = Store::default();
    let kid = SessionInfo::from_json(
        &serde_json::json!({"id":"ses_kid","parentID":"ses_a","location":{"directory":"/r"}}),
    )
    .unwrap();
    s.children.insert("ses_a".into(), vec![kid]);
    for id in ["ses_a", "ses_kid"] {
        s.reconcile_inner(id, vec![]);
        s.busy.insert(id.into());
        s.retry.insert(id.into(), "x".into());
        s.errors.insert(id.into(), "x".into());
        s.needs_reload.insert(id.into());
        s.usage.insert(id.into(), Default::default());
        s.session_modes.insert(id.into(), None);
        s.unread.insert(id.into());
    }
    let inbox_reads: Vec<_> = ["ses_a", "ses_kid"]
        .map(|id| (id, s.inbox_revision.current(id.into())))
        .into();
    s.apply_inner(Envelope::bare(Event::SessionRemoved {
        session_id: "ses_a".into(),
    }));
    assert!(s.conversations.is_empty() && s.children.is_empty());
    assert!(s.busy.is_empty() && s.retry.is_empty() && s.errors.is_empty());
    assert!(s.needs_reload.is_empty() && s.usage.is_empty() && s.session_modes.is_empty());
    assert!(s.unread.is_empty());
    for (id, revision) in inbox_reads {
        assert!(
            !s.inbox_revision.is_current(&id.to_string(), revision),
            "an inbox read in flight for {id} is stale"
        );
    }
}

#[test]
fn conversation_revision_ignores_text_deltas_but_tracks_tools() {
    let mut s = Store::default();
    s.reconcile_inner("ses_a", vec![]);
    let rev = |s: &Store| s.conversations["ses_a"].revision;
    s.apply_inner(wire(r#"{"type":"session.step.started","data":{"sessionID":"ses_a","assistantMessageID":"msg_a"}}"#));
    let before = rev(&s);
    s.apply_inner(wire(r#"{"type":"session.text.delta","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","ordinal":0,"delta":"Hi"}}"#));
    assert_eq!(rev(&s), before, "streamed text keeps memoized views valid");
    s.apply_inner(wire(r#"{"type":"session.tool.input.started","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","id":"c1","name":"todowrite"}}"#));
    assert!(rev(&s) > before, "a tool call invalidates them");
}

#[test]
fn old_transcripts_are_evicted_but_live_ones_stay() {
    let mut s = Store::default();
    for i in 0..20 {
        s.reconcile_inner(&format!("ses_{i}"), vec![]);
    }
    s.busy.insert("ses_0".into());
    for i in 5..20 {
        let id = format!("ses_{i}");
        s.selected = Some(id.clone());
        s.recent.retain(|r| r != &id);
        s.recent.push_back(id);
        if s.recent.len() > KEEP_CONVERSATIONS {
            s.recent.pop_front();
        }
    }
    s.evict_conversations();
    assert!(s.conversations.contains_key("ses_0"), "busy stays");
    assert!(!s.conversations.contains_key("ses_1"));
    assert!(!s.conversations.contains_key("ses_9"));
    for i in 10..20 {
        assert!(s.conversations.contains_key(&format!("ses_{i}")));
    }
    assert_eq!(s.conversations.len(), KEEP_CONVERSATIONS + 1);
}

#[test]
fn assistant_event_for_a_user_message_id_reloads_instead_of_panicking() {
    let mut s = Store::default();
    s.reconcile_inner(
        "ses_a",
        vec![Message::User(UserMessage {
            id: "msg_1".into(),
            text: "hi".into(),
            attachments: vec![],
            meta: Meta::default(),
        })],
    );
    let events = s.apply_inner(wire(
        r#"{"type":"session.text.delta","data":{"sessionID":"ses_a","assistantMessageID":"msg_1","ordinal":0,"delta":"x"}}"#,
    ));
    assert!(events.is_empty());
    assert!(s.needs_reload.contains("ses_a"));
    let conv = &s.conversations["ses_a"];
    assert!(matches!(conv.messages.as_slice(), [Message::User(_)]));
}

#[test]
fn immediate_send_feedback_is_scoped_and_tracks_concurrent_requests() {
    let mut s = Store {
        current_folder: Some("/repo".into()),
        ..Default::default()
    };
    s.submitting
        .insert(1, SubmissionTarget::Draft("/repo".into()));
    assert_eq!(s.response_activity(), Some("Sending…"));
    s.current_folder = Some("/other".into());
    assert!(!s.is_submitting(), "another draft must not look busy");
    s.selected = Some("ses_a".into());
    s.submitting
        .insert(2, SubmissionTarget::Session("ses_a".into()));
    s.submitting
        .insert(3, SubmissionTarget::Session("ses_a".into()));
    s.submitting.remove(&2);
    assert!(
        s.is_submitting(),
        "one completed request cannot hide another"
    );
    s.submitting.remove(&3);
    assert_eq!(
        s.response_activity(),
        None,
        "failed admissions must clear feedback"
    );
    s.current_folder = Some("/repo".into());
    assert!(
        !s.is_submitting(),
        "draft admissions cannot affect an open session"
    );
}

#[test]
fn response_activity_covers_model_latency_and_ends_with_execution() {
    let mut s = Store {
        selected: Some("ses_a".into()),
        ..Default::default()
    };
    s.reconcile_inner("ses_a", vec![]);
    s.apply_inner(wire(
        r#"{"type":"session.execution.started","data":{"sessionID":"ses_a"}}"#,
    ));
    assert_eq!(s.response_activity(), Some("Thinking…"));
    s.apply_inner(wire(r#"{"type":"session.step.started","data":{"sessionID":"ses_a","assistantMessageID":"msg_a"}}"#));
    assert_eq!(
        s.response_activity(),
        Some("Thinking…"),
        "empty assistant messages still need feedback"
    );
    s.apply_inner(wire(r#"{"type":"session.text.delta","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","ordinal":0,"delta":"Hello"}}"#));
    assert_eq!(s.response_activity(), Some("Working…"));
    s.interrupting.insert("ses_a".into());
    assert_eq!(s.response_activity(), Some("Stopping…"));
    s.apply_inner(Envelope::bare(Event::ExecutionEnded {
        session_id: "ses_a".into(),
        outcome: Outcome::Interrupted {
            reason: "user".into(),
        },
    }));
    assert_eq!(s.response_activity(), None);
    assert!(s.interrupting.is_empty());
}

#[test]
fn response_activity_yields_to_approval_and_retry_messages() {
    let mut s = Store {
        selected: Some("ses_a".into()),
        ..Default::default()
    };
    s.busy.insert("ses_a".into());
    s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(
        "ses_a", "perm_a",
    ))));
    assert_eq!(
        s.response_activity(),
        None,
        "waiting for the user is not thinking"
    );
    s.apply_inner(Envelope::bare(Event::PermissionReplied {
        session_id: "ses_a".into(),
        request_id: "perm_a".into(),
    }));
    assert_eq!(s.response_activity(), Some("Thinking…"));
    s.retry
        .insert("ses_a".into(), "Provider unavailable".into());
    assert_eq!(
        s.response_activity(),
        None,
        "the retry banner explains the wait"
    );
}

#[test]
fn queued_prompt_reaches_transcript_only_on_delivery() {
    let mut s = Store::default();
    s.reconcile_inner("ses_a", vec![]);
    let enq = |id: &str, text: &str, d: &str| {
        wire(&format!(
            r#"{{"type":"session.inbox.enqueued","data":{{"inboxID":"{id}","sessionID":"ses_a","item":{{"type":"user","payload":{{"text":"{text}"}},"delivery":"{d}"}}}}}}"#
        ))
    };
    s.apply_inner(enq("msg_q", "later", "queue"));
    s.apply_inner(enq("msg_x", "drop me", "queue"));
    assert_eq!(s.pending_prompts("ses_a").len(), 2);
    assert!(
        s.conversations["ses_a"].messages.is_empty(),
        "pending isn't transcript"
    );
    // Promote, cancel, deliver.
    s.apply_inner(wire(r#"{"type":"session.inbox.delivery.changed","data":{"sessionID":"ses_a","inboxID":"msg_q","delivery":"steer"}}"#));
    assert_eq!(s.pending_prompts("ses_a")[0].delivery, Delivery::Steer);
    s.apply_inner(wire(
        r#"{"type":"session.inbox.cancelled","data":{"sessionID":"ses_a","inboxID":"msg_x"}}"#,
    ));
    let events = s.apply_inner(wire(
        r#"{"type":"session.inbox.delivered","data":{"sessionID":"ses_a","inboxID":"msg_q"}}"#,
    ));
    assert!(s.pending_prompts("ses_a").is_empty());
    assert!(events.contains(&StoreEvent::InboxChanged {
        session_id: "ses_a".into()
    }));
    assert!(matches!(&s.conversations["ses_a"].messages[..],
        [Message::User(u)] if u.id == "msg_q" && u.text == "later"));
    // A delivery we never saw enqueued is left to the next reconcile.
    s.apply_inner(wire(r#"{"type":"session.inbox.delivered","data":{"sessionID":"ses_a","inboxID":"msg_unknown"}}"#));
    assert_eq!(s.conversations["ses_a"].messages.len(), 1);
}

#[test]
fn transcript_keeps_delivery_order_not_id_order() {
    // Observed on 2.2.6: a queued prompt (smaller id) sent before a steer
    // is delivered after it; the REST projection uses delivery order.
    let mut s = Store::default();
    s.reconcile_inner("ses_a", vec![]);
    for (id, d) in [("msg_1_queue", "queue"), ("msg_2_steer", "steer")] {
        s.apply_inner(wire(&format!(
            r#"{{"type":"session.inbox.enqueued","data":{{"inboxID":"{id}","sessionID":"ses_a","item":{{"type":"user","payload":{{"text":"{id}"}},"delivery":"{d}"}}}}}}"#
        )));
    }
    for id in ["msg_2_steer", "msg_1_queue"] {
        s.apply_inner(wire(&format!(
            r#"{{"type":"session.inbox.delivered","data":{{"sessionID":"ses_a","inboxID":"{id}"}}}}"#
        )));
    }
    let ids = |s: &Store| -> Vec<String> {
        s.conversations["ses_a"]
            .messages
            .iter()
            .map(|m| m.id().to_string())
            .collect()
    };
    assert_eq!(ids(&s), ["msg_2_steer", "msg_1_queue"]);
    // A snapshot in the same order changes nothing; a message streamed
    // after the snapshot stays after it.
    let user = |id: &str| {
        Message::User(UserMessage {
            id: id.into(),
            text: id.into(),
            attachments: vec![],
            meta: Default::default(),
        })
    };
    s.begin_load("ses_a");
    s.apply_inner(wire(r#"{"type":"session.step.started","data":{"sessionID":"ses_a","assistantMessageID":"msg_0_reply"}}"#));
    s.reconcile_inner("ses_a", vec![user("msg_2_steer"), user("msg_1_queue")]);
    assert_eq!(ids(&s), ["msg_2_steer", "msg_1_queue", "msg_0_reply"]);
    // Local messages no event touched during the fetch are gone on the
    // server (an undo): the next snapshot drops them.
    s.begin_load("ses_a");
    s.reconcile_inner("ses_a", vec![user("msg_2_steer")]);
    assert_eq!(ids(&s), ["msg_2_steer"]);
}

#[test]
fn completion_during_loaded_fetch_survives_snapshot() {
    let mut store = Store::default();
    store.reconcile_inner("ses_a", vec![]);
    store.apply_inner(wire(r#"{"type":"session.tool.called","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","id":"tool_a","input":{}}}"#));
    let snapshot = store.conversations["ses_a"].messages.clone();
    store.begin_load("ses_a");
    store.apply_inner(wire(r#"{"type":"session.tool.success","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","id":"tool_a","content":[{"type":"text","text":"done"}]}}"#));
    store.apply_inner(wire(r#"{"type":"session.step.ended","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","finish":"stop"}}"#));
    store.reconcile_inner("ses_a", snapshot);
    let Message::Assistant(message) = &store.conversations["ses_a"].messages[0] else {
        panic!()
    };
    assert!(message.completed);
    let Block::Tool(tool) = &message.blocks[0] else {
        panic!()
    };
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.output, "done");
}

#[test]
fn snapshot_revision_tracks_session_execution_and_inbox_events() {
    let mut store = Store::default();
    for json in [
        r#"{"type":"session.execution.started","data":{"sessionID":"ses_a"}}"#,
        r#"{"type":"session.execution.succeeded","data":{"sessionID":"ses_a"}}"#,
        r#"{"type":"session.inbox.cancelled","data":{"sessionID":"ses_a","inboxID":"msg_a"}}"#,
    ] {
        let before = store.snapshot_revision;
        store.apply_inner(wire(json));
        assert!(store.snapshot_revision > before);
    }
}

#[test]
fn loading_overflow_invalidates_snapshot_instead_of_silently_losing_events() {
    let mut store = Store::default();
    let ticket = store.begin_load("ses_a");
    for _ in 0..=MAX_PENDING {
        store.apply_inner(wire(r#"{"type":"session.text.delta","data":{"sessionID":"ses_a","assistantMessageID":"msg_a","ordinal":0,"delta":"x"}}"#));
    }
    let conversation = &store.conversations["ses_a"];
    assert!(!conversation.load.accepts(ticket));
    assert!(!conversation.load.fetching());
    assert!(conversation.pending.is_empty());
    assert!(store.needs_reload.contains("ses_a"));
}

#[test]
fn deltas_buffered_during_a_load_join_the_snapshot_without_gaps_or_repeats() {
    let mut s = Store::default();
    let sid = "ses_b";
    s.begin_load(sid);
    let delta = |d: &str| {
        wire(&format!(
            r#"{{"type":"session.text.delta","data":{{"sessionID":"{sid}","assistantMessageID":"msg_a","ordinal":0,"delta":"{d}"}}}}"#
        ))
    };
    // Arrive while the fetch is in flight; the snapshot saw "Hel".
    for d in ["He", "l", "lo", " world"] {
        s.apply_inner(delta(d));
    }
    let snap = Message::Assistant(AssistantMessage {
        id: "msg_a".into(),
        blocks: vec![Block::Prose {
            kind: BlockKind::Text,
            ordinal: 0,
            text: "Hel".into(),
            done: true,
            time: Default::default(),
        }],
        ..Default::default()
    });
    s.reconcile_inner(sid, vec![snap]);
    let Message::Assistant(a) = &s.conversations[sid].messages[0] else {
        panic!()
    };
    let Block::Prose { text, .. } = &a.blocks[0] else {
        panic!()
    };
    assert_eq!(text, "Hello world");
}

#[test]
fn a_stale_load_is_ignored() {
    let mut s = Store::default();
    let first = s.begin_load("ses_c");
    let second = s.begin_load("ses_c");
    assert_ne!(first, second);
    assert!(s.conversations["ses_c"].load.accepts(second));
}

fn history_message(id: &str) -> Message {
    Message::User(UserMessage {
        id: id.into(),
        text: id.into(),
        attachments: vec![],
        meta: Default::default(),
    })
}

#[test]
fn history_completion_orders_and_duplicate_responses_preserve_newest_snapshot() {
    for newest_first in [false, true] {
        let mut s = Store::default();
        let a = s.begin_load("ses_a");
        let b = s.begin_load("ses_a");
        let replies = if newest_first {
            [(b, "new"), (a, "old")]
        } else {
            [(a, "old"), (b, "new")]
        };
        for (ticket, id) in replies {
            let result = s.finish_load_inner("ses_a", ticket, Ok(vec![history_message(id)]));
            if ticket == a {
                assert!(matches!(result, HistoryChange::Ignored));
            }
        }
        let before = s.conversations["ses_a"].revision;
        assert!(matches!(
            s.finish_load_inner("ses_a", b, Ok(vec![])),
            HistoryChange::Ignored
        ));
        assert_eq!(s.conversations["ses_a"].revision, before);
        assert_eq!(s.conversations["ses_a"].messages[0].id(), "new");
    }
}

#[test]
fn histories_are_session_isolated_even_when_selection_changes_and_a_arrives_last() {
    let mut s = Store {
        selected: Some("ses_a".into()),
        ..Default::default()
    };
    let a = s.begin_load("ses_a");
    s.selected = Some("ses_b".into());
    let b = s.begin_load("ses_b");
    assert!(matches!(
        s.finish_load_inner("ses_b", a, Ok(vec![])),
        HistoryChange::Ignored
    ));
    s.finish_load_inner("ses_b", b, Ok(vec![history_message("b")]));
    let result = s.finish_load_inner("ses_a", a, Ok(vec![history_message("a")]));
    assert!(
        matches!(result, HistoryChange::Reconciled(events) if events == vec![StoreEvent::MessagesReset { session_id: "ses_a".into() }])
    );
    assert_eq!(s.selected.as_deref(), Some("ses_b"));
    assert_eq!(s.conversations["ses_b"].messages[0].id(), "b");
    assert_eq!(s.conversations["ses_a"].messages[0].id(), "a");
}

#[test]
fn deleted_then_recreated_session_rejects_old_success_and_failure() {
    let mut s = Store::default();
    let old = s.begin_load("ses_a");
    s.apply_inner(Envelope::bare(Event::SessionRemoved {
        session_id: "ses_a".into(),
    }));
    assert!(matches!(
        s.finish_load_inner("ses_a", old, Ok(vec![])),
        HistoryChange::Ignored
    ));
    let new = s.begin_load("ses_a");
    assert_ne!(old, new);
    s.finish_load_inner("ses_a", new, Ok(vec![history_message("new")]));
    assert!(matches!(
        s.finish_load_inner("ses_a", old, Ok(vec![history_message("old")])),
        HistoryChange::Ignored
    ));
    assert!(matches!(
        s.finish_load_inner("ses_a", old, Err(anyhow::anyhow!("old failure"))),
        HistoryChange::Ignored
    ));
    assert!(!s.load_errors.contains_key("ses_a"));
    assert_eq!(s.conversations["ses_a"].messages[0].id(), "new");
}

#[test]
fn refresh_failure_keeps_data_and_stale_success_cannot_clear_new_error() {
    let mut s = Store::default();
    s.reconcile_inner("ses_a", vec![history_message("cached")]);
    let old = s.begin_load("ses_a");
    let current = s.begin_load("ses_a");
    assert!(matches!(
        s.finish_load_inner("ses_a", current, Err(anyhow::anyhow!("offline"))),
        HistoryChange::Failed
    ));
    assert!(s.conversations["ses_a"].load.loaded());
    assert!(!s.conversations["ses_a"].load.fetching());
    assert!(matches!(
        s.finish_load_inner("ses_a", old, Ok(vec![])),
        HistoryChange::Ignored
    ));
    assert_eq!(s.load_errors["ses_a"], "offline");
    assert_eq!(s.conversations["ses_a"].messages[0].id(), "cached");
}

#[test]
fn disconnect_releases_buffered_loads_and_reconnect_uses_fresh_snapshot() {
    use crate::domain::requests::{RequestClock, RequestScope};
    let mut clock = RequestClock::default();
    let mut s = Store::default();
    s.reconcile_inner("ses_a", vec![history_message("cached")]);
    let a = s.begin_load("ses_a");
    let b = s.begin_load("ses_b");
    let issued = clock.issue(RequestScope::Connection);
    s.apply_inner(wire(r#"{"type":"session.text.delta","data":{"sessionID":"ses_b","assistantMessageID":"msg_a","ordinal":0,"delta":"missed"}}"#));
    clock.connection_changed();
    s.cancel_history_loads();
    assert!(!clock.accepts(issued));
    for (id, ticket) in [("ses_a", a), ("ses_b", b)] {
        assert!(!s.conversations[id].load.fetching());
        assert!(s.conversations[id].pending.is_empty());
        assert!(matches!(
            s.finish_load_inner(id, ticket, Ok(vec![])),
            HistoryChange::Ignored
        ));
    }
    assert!(s.conversations["ses_a"].load.loaded());
    clock.restart();
    clock.connection_changed();
    s.invalidate_except(Some("ses_a"));
    let current = s.begin_load("ses_a");
    s.finish_load_inner("ses_a", current, Ok(vec![history_message("fresh")]));
    assert_eq!(s.conversations["ses_a"].messages[0].id(), "fresh");
    assert!(!s.conversations["ses_b"].load.loaded());
}

#[test]
fn shell_snapshots_remove_missing_entries_only_in_their_folder() {
    let mut s = Store::default();
    let shell = |id: &str| ShellInfo {
        id: id.into(),
        status: "running".into(),
        command: "sleep".into(),
        session_id: None,
        exit: None,
        started: 0,
        completed: None,
    };
    s.replace_shells(Path::new("/a"), vec![shell("a")]);
    s.replace_shells(Path::new("/b"), vec![shell("b")]);
    s.replace_shells(Path::new("/a"), vec![]);
    assert!(!s.shells.contains_key("a"));
    assert!(s.shells.contains_key("b"));
    // Location-tagged events without session metadata are also reconciled.
    s.apply_inner(wire(r#"{"type":"shell.created","location":{"directory":"/a"},"data":{"info":{"id":"event","status":"running"}}}"#));
    assert!(s.shells.contains_key("event"));
    s.replace_shells(Path::new("/a"), vec![]);
    assert!(!s.shells.contains_key("event"));
}

#[test]
fn live_task_events_invalidate_in_flight_task_snapshots() {
    let mut s = Store::default();
    for json in [
        r#"{"type":"shell.created","data":{"info":{"id":"a","status":"running"}}}"#,
        r#"{"type":"shell.exited","data":{"id":"a","status":"exited"}}"#,
        r#"{"type":"shell.deleted","data":{"id":"a"}}"#,
        r#"{"type":"session.created","data":{"sessionID":"child","parentID":"parent","location":{"directory":"/a"}}}"#,
    ] {
        let revision = s.tasks_revision;
        s.apply_inner(wire(json));
        assert_ne!(revision, s.tasks_revision);
    }
}

#[test]
fn subagents_and_shells_are_tracked_per_parent() {
    let mut s = Store::default();
    s.apply_inner(wire(r#"{"type":"session.created","created":5,"data":{"sessionID":"ses_c","parentID":"ses_p","title":"Look around","agent":"general","location":{"directory":"/p"}}}"#));
    assert!(s.sessions.is_empty(), "children stay out of the sidebar");
    assert_eq!(s.children_of("ses_p")[0].title, "Look around");
    s.apply_inner(wire(
        r#"{"type":"session.execution.started","data":{"sessionID":"ses_c"}}"#,
    ));
    s.apply_inner(wire(r#"{"type":"shell.created","data":{"info":{"id":"sh_1","status":"running","command":"sleep 20","metadata":{"sessionID":"ses_p"},"time":{"started":1}}}}"#));
    assert_eq!(s.running_tasks("ses_p"), 2);
    s.apply_inner(wire(
        r#"{"type":"shell.exited","data":{"id":"sh_1","status":"killed"}}"#,
    ));
    s.apply_inner(wire(
        r#"{"type":"session.execution.succeeded","data":{"sessionID":"ses_c"}}"#,
    ));
    assert_eq!(s.running_tasks("ses_p"), 0);
    assert_eq!(s.shells_of("ses_p")[0].status, "killed");
    // Branch updates are keyed by the event's location.
    s.apply_inner(wire(
        r#"{"type":"vcs.branch.updated","location":{"directory":"/p"},"data":{"branch":"feat"}}"#,
    ));
    assert_eq!(s.vcs[Path::new("/p")].branch.as_deref(), Some("feat"));
}

#[test]
fn adjacent_session_follows_sidebar_order() {
    let mut s = Store {
        folders: vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ],
        // Sidebar order: folders in order, sessions newest first.
        sessions: vec![
            session_in("a2", "/a", 2),
            session_in("a1", "/a", 1),
            session_in("b1", "/b", 1),
            session_in("c1", "/c", 1),
        ],
        ..Default::default()
    };
    assert_eq!(s.adjacent_session(1).as_deref(), Some("a2"));
    assert_eq!(s.adjacent_session(-1).as_deref(), Some("c1"));
    s.selected = Some("a1".into());
    assert_eq!(s.adjacent_session(1).as_deref(), Some("b1"));
    assert_eq!(s.adjacent_session(-1).as_deref(), Some("a2"));
    // Collapsed folders are skipped; the ends wrap around.
    s.collapsed.insert(PathBuf::from("/b"));
    assert_eq!(s.adjacent_session(1).as_deref(), Some("c1"));
    s.selected = Some("c1".into());
    assert_eq!(s.adjacent_session(1).as_deref(), Some("a2"));
    s.collapsed
        .extend([PathBuf::from("/a"), PathBuf::from("/c")]);
    assert_eq!(s.adjacent_session(1), None);
}

#[test]
fn loading_and_load_errors() {
    let mut s = Store::default();
    assert!(!s.selected_is_loading());
    s.selected = Some("x".into());
    assert!(s.selected_is_loading());
    s.load_errors.insert("x".into(), "boom".into());
    assert!(!s.selected_is_loading());
    s.load_errors.clear();
    s.reconcile_inner("x", vec![]);
    assert!(!s.selected_is_loading());
}

fn loaded_store() -> Store {
    let mut s = Store::default();
    s.reconcile_inner(&sid(), vec![]);
    s
}

fn summary(m: &Message) -> Vec<String> {
    match m {
        Message::User(u) => vec![u.text.clone()],
        Message::Assistant(a) => a
            .blocks
            .iter()
            .map(|b| match b {
                Block::Prose { text, .. } => text.clone(),
                Block::Tool(t) => format!("{}:{:?}:{}", t.name, t.status, t.output),
            })
            .chain(a.error.clone())
            .collect(),
        Message::Marker(m) => vec![m.text.clone()],
    }
}

fn permission(sid: &str, rid: &str) -> PermissionRequest {
    PermissionRequest::from_json(&serde_json::json!({
        "id":rid, "sessionID":sid, "action":"shell", "resources":["echo hello"],
        "save":["echo *"], "metadata":{"command":"echo hello"}
    }))
    .unwrap()
}

#[test]
fn forms_are_session_scoped_and_close_cleanly() {
    let mut s = Store::default();
    let form = || {
        FormRequest::from_json(&serde_json::json!({
            "id": "frm_1", "sessionID": "ses_a", "title": "Questions",
            "fields": [
                {"key": "q0", "type": "string", "options": [{"value": "y", "label": "Yes"}], "custom": true},
                {"key": "q1", "type": "string", "options": [{"value": "startup", "label": "Startup"}]}
            ]
        }))
        .unwrap()
    };
    s.apply_inner(Envelope::bare(Event::FormAsked(form())));
    s.apply_inner(Envelope::bare(Event::FormAsked(form())));
    assert_eq!(
        s.forms.of_session("ses_a").len(),
        1,
        "duplicate ask frames must not duplicate cards"
    );
    assert_eq!(s.forms.draft("frm_1"), serde_json::json!({}));
    let before = s.forms.revision("ses_a");
    s.apply_inner(Envelope::bare(Event::FormReplied {
        session_id: "ses_a".into(),
        form_id: "frm_1".into(),
    }));
    assert!(s.forms.of_session("ses_a").is_empty());
    assert!(
        s.forms.freshness("ses_a", before) != crate::domain::requests::Freshness::Current,
        "in-flight snapshots must be invalidated"
    );
    // The draft and scratch of the closed form go away with it.
    assert_eq!(s.forms.draft("frm_1"), serde_json::json!({}));
}

#[test]
fn permissions_are_session_scoped_and_survive_unloaded_history() {
    let mut s = Store::default();
    for (sid, rid) in [
        ("ses_a", "perm_1"),
        ("ses_b", "perm_2"),
        ("ses_a", "perm_1"),
    ] {
        s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(sid, rid))));
    }
    assert_eq!(
        s.permissions.of_session("ses_a").len(),
        1,
        "duplicate frames must not duplicate cards"
    );
    assert_eq!(s.permissions.of_session("ses_b").len(), 1);
    let before = s.permissions.revision("ses_a");
    s.apply_inner(Envelope::bare(Event::PermissionReplied {
        session_id: "ses_a".into(),
        request_id: "perm_1".into(),
    }));
    assert!(s.permissions.of_session("ses_a").is_empty());
    assert!(
        s.permissions.freshness("ses_a", before) != crate::domain::requests::Freshness::Current,
        "in-flight snapshots must be invalidated"
    );
    assert_eq!(s.permissions.of_session("ses_b").len(), 1);
    assert!(s.conversations.is_empty());
}

#[test]
fn deleting_session_cleans_pending_replies_and_diffs() {
    let mut s = Store::default();
    s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(
        "ses_a", "perm_1",
    ))));
    assert!(s.permissions.begin_reply("ses_a", "perm_1"));
    s.apply_inner(Envelope::bare(Event::SessionRemoved {
        session_id: "ses_a".into(),
    }));
    assert!(!s.permissions.has_pending("ses_a"));
    assert!(!s.permissions.is_sending("ses_a", "perm_1"));
}

#[test]
fn deleting_a_parent_cleans_its_subagents_approvals_and_strands_their_reads() {
    let mut s = Store::default();
    let kid = SessionInfo::from_json(
        &serde_json::json!({"id":"ses_kid","parentID":"ses_a","location":{"directory":"/r"}}),
    )
    .unwrap();
    s.children.insert("ses_a".into(), vec![kid]);
    let form = FormRequest::from_json(&serde_json::json!({
        "id": "frm_k", "sessionID": "ses_kid", "title": "Q",
        "fields": [{"key": "q0", "type": "string", "default": "y"}]
    }))
    .unwrap();
    s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(
        "ses_kid", "perm_k",
    ))));
    s.apply_inner(Envelope::bare(Event::FormAsked(form)));
    assert!(s.permissions.begin_reply("ses_kid", "perm_k"));
    let (p_read, f_read) = (
        s.permissions.revision("ses_kid"),
        s.forms.revision("ses_kid"),
    );
    s.apply_inner(Envelope::bare(Event::SessionRemoved {
        session_id: "ses_a".into(),
    }));
    assert!(!s.permissions.is_tracked("ses_kid") && !s.forms.is_tracked("ses_kid"));
    assert!(!s.permissions.is_sending("ses_kid", "perm_k"));
    assert_eq!(s.forms.draft("frm_k"), serde_json::json!({}));
    use crate::domain::requests::Freshness::Forgotten;
    assert_eq!(s.permissions.freshness("ses_kid", p_read), Forgotten);
    assert_eq!(s.forms.freshness("ses_kid", f_read), Forgotten);
}

#[test]
fn approval_events_report_only_their_sessions_status() {
    let mut s = Store::default();
    let status = |sid: &str| {
        vec![StoreEvent::StatusChanged {
            session_id: sid.into(),
        }]
    };
    assert_eq!(
        s.apply_inner(Envelope::bare(Event::PermissionAsked(permission(
            "ses_a", "p1"
        )))),
        status("ses_a")
    );
    assert_eq!(
        s.apply_inner(Envelope::bare(Event::PermissionReplied {
            session_id: "ses_a".into(),
            request_id: "p1".into(),
        })),
        status("ses_a")
    );
    assert!(
        s.permissions.is_tracked("ses_a") && !s.permissions.has_pending("ses_a"),
        "replied-out session stays eligible for the turn-end re-read"
    );
    assert_eq!(
        s.apply_inner(Envelope::bare(Event::FormCancelled {
            session_id: "ses_b".into(),
            form_id: "missing".into(),
        })),
        status("ses_b")
    );
    assert!(s.conversations.is_empty(), "no transcript side effects");
}

#[test]
fn tool_failure_preserves_output_and_metadata_from_wire() {
    let mut s = loaded_store();
    let sid = sid();
    let wire = serde_json::json!({"type":"session.tool.failed", "data":{
        "sessionID":sid, "assistantMessageID":"msg_z", "id":"call_z",
        "error":{"type":"shell","message":"Command failed"},
        "content":[{"type":"text","text":"stderr output"}],
        "metadata":{"exit":2,"truncated":true}
    }});
    s.apply_inner(Envelope::from_json(&wire.to_string()).unwrap());
    let Message::Assistant(a) = s.conversations[&sid].messages.last().unwrap() else {
        panic!()
    };
    let Block::Tool(t) = &a.blocks[0] else {
        panic!()
    };
    assert_eq!(t.status, ToolStatus::Error);
    assert_eq!(t.output, "stderr output");
    assert_eq!(t.metadata["exit"], 2);
    assert_eq!(t.error.as_deref(), Some("Command failed"));
    let projected = Message::from_v2_json(&serde_json::json!({
        "id":"msg_z","type":"assistant","content":[{"type":"tool","id":"call_z","name":"",
        "state":{"status":"error","input":null,"error":{"message":"Command failed"},
        "content":[{"type":"text","text":"stderr output"}],"metadata":{"exit":2,"truncated":true}}}]
    }))
    .unwrap();
    let Message::Assistant(remote) = projected else {
        panic!()
    };
    assert_eq!(a.blocks, remote.blocks);
}

#[test]
fn replaying_captured_events_matches_rest_projection() {
    let mut s = loaded_store();
    for env in fixture() {
        s.apply_inner(env);
    }
    // The REST projection of the same session after both turns (the
    // second was interrupted by the user).
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/v2-messages.json")).unwrap();
    let rest: Vec<Message> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Message::from_v2_json)
        .collect();
    let live = &s.conversations[&sid()].messages;
    assert_eq!(live.len(), rest.len());
    for (l, r) in live.iter().zip(&rest) {
        assert_eq!(l.id(), r.id());
        assert_eq!(summary(l), summary(r));
    }
    assert!(!s.is_busy(&sid()));
    assert!(
        !s.errors.contains_key(&sid()),
        "an interrupt is not an error"
    );
    assert!(
        !s.session(&sid()).unwrap().title.is_empty(),
        "session.renamed sets the title"
    );
}

#[test]
fn busy_follows_execution_lifecycle() {
    let mut s = loaded_store();
    let mut busy_during_tool = false;
    for env in fixture() {
        let tool_step =
            matches!(&env.event, Event::StepEnded { finish, .. } if finish == "tool-calls");
        s.apply_inner(env);
        if tool_step {
            busy_during_tool = s.is_busy(&sid());
        }
    }
    assert!(
        busy_during_tool,
        "busy until session.execution.* ends the turn"
    );
    assert!(!s.is_busy(&sid()));
}

#[test]
fn events_for_unloaded_sessions_only_touch_session_state() {
    let mut s = Store::default();
    for env in fixture().into_iter().take(12) {
        s.apply_inner(env);
    }
    assert!(!s.conversations.contains_key(&sid()));
    assert!(s.is_busy(&sid()));
    assert_eq!(s.sessions.len(), 1);
}

#[test]
fn failed_execution_records_error() {
    let mut s = loaded_store();
    let id = sid();
    s.apply_inner(Envelope::bare(Event::ExecutionStarted {
        session_id: id.clone(),
    }));
    s.apply_inner(Envelope::bare(Event::ExecutionEnded {
        session_id: id.clone(),
        outcome: Outcome::Failed {
            error: "Model not found".into(),
        },
    }));
    assert!(!s.is_busy(&id));
    assert_eq!(s.errors[&id], "Model not found");
}

#[test]
fn reconcile_keeps_local_text_that_extends_snapshot() {
    let mut s = loaded_store();
    let id = sid();
    let block = |t: &str| Block::Prose {
        kind: BlockKind::Text,
        ordinal: 0,
        text: t.into(),
        done: false,
        time: Default::default(),
    };
    let msg = |t: &str| {
        Message::Assistant(AssistantMessage {
            id: "msg_2".into(),
            blocks: vec![block(t)],
            ..Default::default()
        })
    };
    let text = |s: &Store| match &s.conversations[&id].messages[0] {
        Message::Assistant(a) => summary(&Message::Assistant(a.clone()))[0].clone(),
        _ => unreachable!(),
    };
    s.reconcile_inner(&id, vec![msg("Hello wor")]);
    s.reconcile_inner(&id, vec![msg("Hello")]);
    assert_eq!(text(&s), "Hello wor");
    s.reconcile_inner(&id, vec![msg("Hello world")]);
    assert_eq!(text(&s), "Hello world");
}

#[test]
fn target_folder_prefers_selected_session() {
    let mut s = Store {
        folders: vec!["/a".into(), "/b".into()],
        current_folder: Some("/a".into()),
        ..Default::default()
    };
    s.sessions.push(SessionInfo {
        id: "ses_x".into(),
        title: String::new(),
        directory: "/b".into(),
        created: 0,
        updated: 0,
        parent_id: None,
        model: None,
        agent: None,
    });
    assert_eq!(s.target_folder(), Some(PathBuf::from("/a")));
    s.selected = Some("ses_x".into());
    assert_eq!(s.target_folder(), Some(PathBuf::from("/b")));
}

#[test]
fn model_and_agent_choices_follow_events_and_precedence() {
    use crate::api::model::{AgentInfo, AgentMode};
    let mref = |id: &str| ModelRef {
        provider_id: "p".into(),
        id: id.into(),
        variant: None,
    };
    let mut s = Store {
        folders: vec!["/a".into()],
        ..Default::default()
    };
    s.catalogs.insert(
        "/a".into(),
        Catalog {
            models: vec![],
            default_model: Some(mref("default")),
            configured_agent: None,
            agents: vec![
                AgentInfo {
                    id: "plan".into(),
                    name: "Plan".into(),
                    mode: AgentMode::Primary,
                    hidden: false,
                },
                AgentInfo {
                    id: "build".into(),
                    name: "Build".into(),
                    mode: AgentMode::Primary,
                    hidden: false,
                },
            ],
        },
    );
    // Nothing chosen: folder defaults, `build` preferred.
    assert_eq!(s.effective_model(), Some(mref("default")));
    assert_eq!(s.effective_agent().as_deref(), Some("build"));
    s.catalogs
        .get_mut(std::path::Path::new("/a"))
        .unwrap()
        .configured_agent = Some("plan".into());
    assert_eq!(s.effective_agent().as_deref(), Some("plan"));
    s.catalogs
        .get_mut(std::path::Path::new("/a"))
        .unwrap()
        .configured_agent = None;
    // A draft pick applies to new sessions.
    s.draft_model = Some(mref("draft"));
    assert_eq!(s.effective_model(), Some(mref("draft")));

    let session = SessionInfo {
        id: "ses_x".into(),
        title: "t".into(),
        directory: "/a".into(),
        created: 0,
        updated: 0,
        parent_id: None,
        model: None,
        agent: None,
    };
    s.apply_inner(Envelope::bare(Event::SessionUpserted(session.clone())));
    s.selected = Some("ses_x".into());
    // A session without its own model shows the folder default, not the draft.
    assert_eq!(s.effective_model(), Some(mref("default")));
    let ev = s.apply_inner(Envelope::bare(Event::ModelSelected {
        session_id: "ses_x".into(),
        model: mref("chosen"),
    }));
    assert_eq!(ev, vec![StoreEvent::ChoicesChanged]);
    s.apply_inner(Envelope::bare(Event::AgentSelected {
        session_id: "ses_x".into(),
        agent: "plan".into(),
    }));
    assert_eq!(s.effective_model(), Some(mref("chosen")));
    assert_eq!(s.effective_agent().as_deref(), Some("plan"));
    // `session.created` replays carry no choices; keep the known ones.
    s.apply_inner(Envelope::bare(Event::SessionUpserted(session)));
    assert_eq!(s.effective_model(), Some(mref("chosen")));
    assert_eq!(s.session("ses_x").unwrap().title, "t");
}
