//! The retained topic lists of an account: LRU, generations, the shown forum
//! (plan section 6.1).

use std::collections::HashMap;

use super::TopicList;
use crate::model::ChatId;
use crate::store::Stale;

/// Most topic lists retained (LRU; the shown one is never evicted). Six
/// keeps the worst-case retained capacity at least 10% under the plan's
/// 4 MiB (section 9).
pub const MAX_TOPIC_LISTS: usize = 6;

/// Identifies one incarnation of a forum's topic view. Bumps on open, close,
/// eviction and invalidation; completions tagged with an older one are stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TopicsGen(pub u64);

#[derive(Debug)]
pub struct Topics {
    by_chat: HashMap<ChatId, TopicList>,
    /// Least recently used first.
    lru: Vec<ChatId>,
    shown: Option<ChatId>,
    next_gen: u64,
}

impl Default for Topics {
    /// Sized once for [`MAX_TOPIC_LISTS`]: the containers never grow.
    fn default() -> Self {
        Self {
            by_chat: HashMap::with_capacity(MAX_TOPIC_LISTS),
            lru: Vec::with_capacity(MAX_TOPIC_LISTS + 1),
            shown: None,
            next_gen: 0,
        }
    }
}

impl Topics {
    /// Opens (or reopens) `chat`'s list as the shown one under a new
    /// generation. A retained list keeps its topics, so Back is instant.
    /// Evicts the least recently used other list past [`MAX_TOPIC_LISTS`].
    pub fn open(&mut self, chat: ChatId) -> &mut TopicList {
        let view = self.bump();
        self.lru.retain(|c| *c != chat);
        self.lru.push(chat);
        self.shown = Some(chat);
        while self.lru.len() > MAX_TOPIC_LISTS {
            let Some(at) = self.lru.iter().position(|c| Some(*c) != self.shown) else {
                break;
            };
            let evicted = self.lru.remove(at);
            self.by_chat.remove(&evicted);
        }
        let list = self
            .by_chat
            .entry(chat)
            .or_insert_with(|| TopicList::new(view));
        list.set_view(view);
        list
    }

    /// Closes the view: in-flight fetches become stale; the list stays
    /// retained until eviction.
    pub fn close(&mut self, chat: ChatId) {
        let view = self.bump();
        if let Some(list) = self.by_chat.get_mut(&chat) {
            list.set_view(view);
        }
        if self.shown == Some(chat) {
            self.shown = None;
        }
    }

    /// Drops `chat`'s list (forum flag change, reconcile). Returns whether
    /// one was retained.
    pub fn invalidate(&mut self, chat: ChatId) -> bool {
        self.lru.retain(|c| *c != chat);
        self.by_chat.remove(&chat).is_some()
    }

    /// Drops every list, in LRU order. Returns the chats that had one.
    pub fn invalidate_all(&mut self) -> Vec<ChatId> {
        self.by_chat.clear();
        std::mem::take(&mut self.lru)
    }

    pub fn shown(&self) -> Option<ChatId> {
        self.shown
    }

    pub fn get(&self, chat: ChatId) -> Option<&TopicList> {
        self.by_chat.get(&chat)
    }

    /// Any retained list, for live events (which carry no generation).
    pub fn get_mut(&mut self, chat: ChatId) -> Option<&mut TopicList> {
        self.by_chat.get_mut(&chat)
    }

    /// The list a completion tagged `view` belongs to, if it is current.
    pub fn current(
        &mut self,
        chat: ChatId,
        view: super::TopicsGen,
    ) -> Result<&mut TopicList, Stale> {
        self.by_chat
            .get_mut(&chat)
            .filter(|l| l.view() == view)
            .ok_or(Stale)
    }

    /// Retained chats, least recently used first.
    pub fn chats(&self) -> &[ChatId] {
        &self.lru
    }

    /// The outer containers' capacities (map entries, LRU slots).
    #[cfg(test)]
    pub(super) fn capacities(&self) -> (usize, usize) {
        (self.by_chat.capacity(), self.lru.capacity())
    }

    fn bump(&mut self) -> TopicsGen {
        self.next_gen += 1;
        TopicsGen(self.next_gen)
    }
}
