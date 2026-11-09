//! Conversation history: REST snapshot loads (tickets, buffered events) and
//! reconciliation of a snapshot with locally streamed state.

use super::*;

impl Store {
    /// Replace a conversation with the authoritative REST projection,
    /// merging so that text streamed after the snapshot was taken survives.
    pub fn reconcile(&mut self, session_id: &str, history: Vec<Message>, cx: &mut Context<Self>) {
        self.load_errors.remove(session_id);
        let events = self.reconcile_inner(session_id, history);
        self.emit_all(events, cx);
    }

    /// A history fetch for `session_id` starts; returns its ticket for
    /// [`Store::finish_load`]. Events that arrive meanwhile are tracked (and,
    /// for an unloaded conversation, buffered) so the snapshot can be merged
    /// without losing or resurrecting anything.
    pub fn begin_load(&mut self, session_id: &str) -> LoadTicket {
        let ticket = self.history_requests.issue();
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        conv.load.begin(ticket);
        tracing::debug!(
            session_id,
            ?ticket,
            refresh = conv.load.loaded(),
            "history request started"
        );
        conv.live.clear();
        conv.pending.clear();
        ticket
    }

    /// Apply the snapshot of fetch `ticket`, unless a newer fetch started.
    pub fn finish_load(
        &mut self,
        session_id: &str,
        ticket: LoadTicket,
        history: anyhow::Result<Vec<Message>>,
        cx: &mut Context<Self>,
    ) {
        match self.finish_load_inner(session_id, ticket, history) {
            HistoryChange::Ignored => {}
            HistoryChange::Failed => cx.notify(),
            HistoryChange::Reconciled(events) => self.emit_all(events, cx),
        }
    }

    /// Pure completion path: stale and duplicate responses cannot clear errors,
    /// mutate messages, or emit notifications for a newer request.
    pub(super) fn finish_load_inner(
        &mut self,
        session_id: &str,
        ticket: LoadTicket,
        history: anyhow::Result<Vec<Message>>,
    ) -> HistoryChange {
        let Some(conv) = self.conversations.get_mut(session_id) else {
            return HistoryChange::Ignored;
        };
        if !conv.load.accepts(ticket) {
            tracing::debug!(session_id, ?ticket, state = ?conv.load, "ignored obsolete history completion");
            return HistoryChange::Ignored;
        }
        match history {
            Ok(history) => {
                tracing::debug!(
                    session_id,
                    ?ticket,
                    messages = history.len(),
                    buffered_events = conv.pending.len(),
                    "history snapshot accepted"
                );
                self.load_errors.remove(session_id);
                HistoryChange::Reconciled(self.reconcile_inner(session_id, history))
            }
            Err(e) => {
                conv.cancel_load();
                self.load_errors
                    .insert(session_id.to_string(), format!("{e:#}"));
                HistoryChange::Failed
            }
        }
    }

    /// run_on drops obsolete callbacks. Release their loading/buffering state at
    /// the same boundary, rather than leaving an unreachable fetch in progress.
    pub(crate) fn cancel_history_loads(&mut self) {
        for conv in self.conversations.values_mut() {
            conv.cancel_load();
        }
    }

    /// Forget fetched history for every session except `keep`, so it is
    /// re-fetched when next selected (used after a reconnect).
    pub fn invalidate_except(&mut self, keep: Option<&str>) {
        for (id, conv) in self.conversations.iter_mut() {
            if Some(id.as_str()) != keep {
                conv.cancel_load();
                conv.load.invalidate();
            }
        }
    }

    pub(crate) fn reconcile_inner(
        &mut self,
        session_id: &str,
        history: Vec<Message>,
    ) -> Vec<StoreEvent> {
        let conv = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        conv.revision += 1;
        // The snapshot's order is authoritative (delivery order).
        let old = std::mem::take(&mut conv.messages);
        let in_snapshot: HashSet<String> = history.iter().map(|m| m.id().to_string()).collect();
        let mut local: HashMap<String, Message> = HashMap::new();
        let mut newer: Vec<Message> = Vec::new();
        for m in old {
            if in_snapshot.contains(m.id()) {
                local.insert(m.id().to_string(), m);
            } else {
                newer.push(m);
            }
        }
        conv.joined_mid_stream = missed_starts(
            &history,
            &local,
            &conv.pending,
            &std::mem::take(&mut conv.joined_mid_stream),
        );
        let mut merged: Vec<Message> = history
            .into_iter()
            .map(|mut snap| {
                let prev = local.remove(snap.id());
                if let (Message::Assistant(s), Some(Message::Assistant(l))) = (&mut snap, prev) {
                    merge_blocks(&mut s.blocks, l.blocks);
                }
                snap
            })
            .collect();
        // Messages streamed while the fetch was in flight survive, after the
        // snapshot, in the order they arrived; other local-only messages are
        // gone on the server (reverted, compacted, deleted).
        merged.extend(newer.into_iter().filter(|m| conv.live.contains(m.id())));
        conv.messages = merged;
        conv.load.ready();
        conv.live.clear();
        let pending = std::mem::take(&mut conv.pending);
        // Prose blocks the snapshot has: buffered deltas for them partly
        // overlap its text, so they are joined by overlap, not appended.
        let in_snapshot: HashSet<(String, BlockKind, u64)> = conv
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            })
            .flat_map(|a| {
                a.blocks.iter().filter_map(|b| match b {
                    Block::Prose { kind, ordinal, .. } => Some((a.id.clone(), *kind, *ordinal)),
                    _ => None,
                })
            })
            .collect();
        let mut joined: Vec<((String, BlockKind, u64), String)> = Vec::new();
        for env in pending {
            if let Event::BlockDelta {
                message_id,
                kind,
                ordinal,
                delta,
                ..
            } = &env.event
            {
                let key = (message_id.clone(), *kind, *ordinal);
                if in_snapshot.contains(&key) {
                    match joined.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, d)) => d.push_str(delta),
                        None => joined.push((key, delta.clone())),
                    }
                    continue;
                }
            }
            self.apply_inner(env);
        }
        if let Some(conv) = self.conversations.get_mut(session_id) {
            for ((message_id, kind, ordinal), deltas) in joined {
                if let Ok(i) = conv.index_of(&message_id)
                    && let Message::Assistant(a) = &mut conv.messages[i]
                {
                    let (text, _, _) = prose_mut(a, kind, ordinal);
                    append_overlapping(text, &deltas);
                }
            }
        }
        vec![StoreEvent::MessagesReset {
            session_id: session_id.to_string(),
        }]
    }
}

/// Completion effects stay separate from the pure history transition.
pub(super) enum HistoryChange {
    Ignored,
    Failed,
    Reconciled(Vec<StoreEvent>),
}

/// Snapshot blocks win, except where local streamed text strictly extends
/// the snapshot's (the server persists text slightly after streaming it),
/// and local blocks the snapshot doesn't know yet are kept.
/// Prose blocks of still-running snapshot messages whose beginning is lost:
/// the snapshot has no text for them (opencode persists a block's text when
/// it ends), no `BlockStarted` was buffered (it streamed before the fetch),
/// and no local text covers the start (none, or itself only a tail).
fn missed_starts(
    history: &[Message],
    local: &HashMap<String, Message>,
    pending: &[Envelope],
    was: &HashSet<(String, BlockKind, u64)>,
) -> HashSet<(String, BlockKind, u64)> {
    let started: HashSet<(&str, BlockKind, u64)> = pending
        .iter()
        .filter_map(|env| match &env.event {
            Event::BlockStarted {
                message_id,
                kind,
                ordinal,
                ..
            } => Some((message_id.as_str(), *kind, *ordinal)),
            _ => None,
        })
        .collect();
    let mut out = HashSet::new();
    for m in history {
        let Message::Assistant(a) = m else { continue };
        if a.completed {
            continue;
        }
        for b in &a.blocks {
            let Block::Prose {
                kind,
                ordinal,
                text,
                ..
            } = b
            else {
                continue;
            };
            if !text.is_empty() || started.contains(&(a.id.as_str(), *kind, *ordinal)) {
                continue;
            }
            let key = (a.id.clone(), *kind, *ordinal);
            let local_text = match local.get(&a.id) {
                Some(Message::Assistant(l)) => l.blocks.iter().find_map(|lb| match lb {
                    Block::Prose {
                        kind: k,
                        ordinal: o,
                        text,
                        ..
                    } if k == kind && o == ordinal => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            };
            if local_text.is_none_or(str::is_empty) || was.contains(&key) {
                out.insert(key);
            }
        }
    }
    out
}

fn merge_blocks(snapshot: &mut Vec<Block>, local: Vec<Block>) {
    for lb in local {
        let found = snapshot.iter_mut().find(|sb| same_block(sb, &lb));
        match (found, &lb) {
            (Some(Block::Prose { text: s, .. }), Block::Prose { text: l, .. }) => {
                if l.len() > s.len() && l.starts_with(s.as_str()) {
                    *s = l.clone();
                }
            }
            (Some(_), _) => {}
            (None, _) => snapshot.push(lb),
        }
    }
}

fn same_block(a: &Block, b: &Block) -> bool {
    match (a, b) {
        (
            Block::Prose {
                kind: ka,
                ordinal: oa,
                ..
            },
            Block::Prose {
                kind: kb,
                ordinal: ob,
                ..
            },
        ) => ka == kb && oa == ob,
        (Block::Tool(x), Block::Tool(y)) => x.call_id == y.call_id,
        _ => false,
    }
}
