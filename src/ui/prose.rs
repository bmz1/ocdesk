//! Markdown for message prose.
//!
//! Every prose block (assistant text, reasoning, user prompt) gets its own
//! `TextViewState`, created lazily the first time its row is rendered (the
//! list is virtualized, so only rows near the viewport ever get one).
//!
//! Streaming is incremental for free: the store only ever *appends* to a
//! block's text, and `TextViewState::set_text` turns an extension of the
//! current text into `push_str`, which re-parses only the tail of the
//! document on a background thread. Finished blocks keep their parsed form
//! until the cache is cleared (session switch) or evicted (LRU cap).
//!
//! No explicit re-measure is needed when a background parse commits: GPUI's
//! `list` lays out every visible row each frame, and the state's `notify`
//! redraws the window that painted it.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::text::TextViewState;
use gpui_kit::*;

use crate::api::model::{Block, BlockKind, Message};
use crate::store::Store;

/// Above this many parsed blocks, the least recently rendered half is dropped.
const MAX_ENTRIES: usize = 400;

/// Which prose block of which message.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub message_id: SharedString,
    pub slot: Slot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    User,
    Prose(BlockKind, u64),
}

impl BlockKey {
    /// Stable, window-unique-enough id for elements that belong to a block.
    pub fn element_id(&self, what: &'static str) -> ElementId {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        ElementId::NamedInteger(what.into(), h.finish())
    }
}

/// The prose blocks of `msg` that render as markdown, with their text.
pub fn prose_blocks(msg: &Message) -> impl Iterator<Item = (BlockKey, &str)> {
    let id: SharedString = msg.id().to_string().into();
    let (user, blocks) = match msg {
        Message::User(u) => (Some(u.text.as_str()), &[][..]),
        Message::Assistant(a) => (None, &a.blocks[..]),
        Message::Marker(_) => (None, &[][..]),
    };
    let user_key = id.clone();
    user.map(move |t| {
        (
            BlockKey {
                message_id: user_key,
                slot: Slot::User,
            },
            t,
        )
    })
    .into_iter()
    .chain(blocks.iter().filter_map(move |b| match b {
        Block::Prose {
            kind,
            ordinal,
            text,
            ..
        } => Some((
            BlockKey {
                message_id: id.clone(),
                slot: Slot::Prose(*kind, *ordinal),
            },
            text.as_str(),
        )),
        Block::Tool(_) => None,
    }))
}

struct Entry {
    state: Entity<TextViewState>,
    /// Length of the text last handed to `state`. The store only appends,
    /// so a length change is a content change; `usize::MAX` forces a full
    /// compare (after a reconcile, which may rewrite text in place).
    len: usize,
    last_used: Cell<u64>,
}

pub struct ProseCache {
    store: Entity<Store>,
    scroller: Entity<MessageScrollerState>,
    entries: HashMap<BlockKey, Entry>,
    /// Reasoning blocks the user expanded (collapsed by default).
    expanded: HashSet<BlockKey>,
    clock: Cell<u64>,
}

impl ProseCache {
    pub fn new(store: Entity<Store>, scroller: Entity<MessageScrollerState>) -> Self {
        Self {
            store,
            scroller,
            entries: HashMap::new(),
            expanded: HashSet::new(),
            clock: Cell::new(0),
        }
    }

    /// Whether `key` needs a `sync` before it can be rendered.
    pub fn is_stale(&self, key: &BlockKey, len: usize) -> bool {
        self.entries.get(key).is_none_or(|e| e.len != len)
    }

    /// Hand `text` to the block's state, creating it on first use.
    pub fn sync(&mut self, key: BlockKey, text: &str, cx: &mut Context<Self>) {
        if let Some(e) = self.entries.get_mut(&key) {
            if e.len != text.len() {
                e.len = text.len();
                e.state.update(cx, |s, cx| s.set_text(text, cx));
            }
            return;
        }
        let state = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
        self.entries.insert(
            key,
            Entry {
                state,
                len: text.len(),
                last_used: Cell::new(self.clock.get()),
            },
        );
        self.evict();
    }

    /// The state for a block that was just synced.
    pub fn state(&self, key: &BlockKey) -> Option<&Entity<TextViewState>> {
        let e = self.entries.get(key)?;
        let now = self.clock.get() + 1;
        self.clock.set(now);
        e.last_used.set(now);
        Some(&e.state)
    }

    pub fn is_expanded(&self, key: &BlockKey) -> bool {
        self.expanded.contains(key)
    }

    pub fn toggle_expanded(&mut self, key: BlockKey, cx: &mut Context<Self>) {
        let ix = self
            .store
            .read(cx)
            .selected_conversation()
            .and_then(|c| c.index_of(&key.message_id).ok());
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        if let Some(ix) = ix {
            self.scroller
                .update(cx, |s, cx| s.remeasure_items(ix..ix + 1, cx));
        }
        cx.notify();
    }

    /// Drop every parsed block (the conversation changed).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.expanded.clear();
    }

    /// Text may have been rewritten in place (REST reconcile): compare in
    /// full on the next render instead of trusting lengths.
    pub fn invalidate(&mut self) {
        for e in self.entries.values_mut() {
            e.len = usize::MAX;
        }
    }

    fn evict(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut ages: Vec<u64> = self.entries.values().map(|e| e.last_used.get()).collect();
        let mid = ages.len() / 2;
        let (_, cutoff, _) = ages.select_nth_unstable(mid);
        let cutoff = *cutoff;
        self.entries.retain(|_, e| e.last_used.get() > cutoff);
    }
}

/// Make sure every prose block of row `ix` has an up-to-date state. Only
/// blocks whose text changed are cloned out of the store.
pub fn sync_row(store: &Entity<Store>, cache: &Entity<ProseCache>, ix: usize, cx: &mut App) {
    let stale: Vec<(BlockKey, String)> = {
        let Some(msg) = store
            .read(cx)
            .selected_conversation()
            .and_then(|c| c.messages.get(ix))
        else {
            return;
        };
        let c = cache.read(cx);
        prose_blocks(msg)
            .filter(|(k, t)| !t.is_empty() && c.is_stale(k, t.len()))
            .map(|(k, t)| (k, t.to_string()))
            .collect()
    };
    if !stale.is_empty() {
        cache.update(cx, |c, cx| {
            for (k, t) in stale {
                c.sync(k, &t, cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::{AssistantMessage, ToolCall, ToolStatus, UserMessage};

    #[::core::prelude::v1::test]
    fn prose_blocks_skip_tools_and_key_by_kind_and_ordinal() {
        let msg = Message::Assistant(AssistantMessage {
            id: "m1".into(),
            blocks: vec![
                Block::Prose {
                    kind: BlockKind::Reasoning,
                    ordinal: 0,
                    text: "think".into(),
                    done: true,
                    time: Default::default(),
                },
                Block::Tool(Box::new(ToolCall {
                    call_id: "c".into(),
                    name: "bash".into(),
                    status: ToolStatus::Completed,
                    input: serde_json::Value::Null,
                    output: String::new(),
                    error: None,
                    metadata: serde_json::Value::Null,
                })),
                Block::Prose {
                    kind: BlockKind::Text,
                    ordinal: 0,
                    text: "**hi**".into(),
                    done: false,
                    time: Default::default(),
                },
            ],
            ..Default::default()
        });
        let got: Vec<_> = prose_blocks(&msg).map(|(k, t)| (k.slot, t)).collect();
        assert_eq!(
            got,
            vec![
                (Slot::Prose(BlockKind::Reasoning, 0), "think"),
                (Slot::Prose(BlockKind::Text, 0), "**hi**"),
            ]
        );

        let user = Message::User(UserMessage {
            id: "u1".into(),
            text: "hello".into(),
            attachments: vec![],
            meta: Default::default(),
        });
        let got: Vec<_> = prose_blocks(&user).map(|(k, t)| (k.slot, t)).collect();
        assert_eq!(got, vec![(Slot::User, "hello")]);
    }
}
