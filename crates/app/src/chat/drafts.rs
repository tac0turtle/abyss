//! Per-thread composer drafts (task 6k): the message box belongs to the
//! open thread, so text typed or put back in chat A never goes to chat B.
//! The shell stashes the box when it switches threads and restores the new
//! thread's draft. Cleared with the account (`ChatShell::reset`).

use std::collections::VecDeque;

use abyss_core::model::ThreadKey;

/// Drafts kept at once: a bound on memory, far above how many chats a
/// person drafts in at once. A draft is never dropped to make room (task
/// 6l): a switch that would need a slot past the bound is refused.
pub(crate) const MAX_DRAFTS: usize = 64;

#[derive(Default)]
pub(crate) struct Drafts {
    /// Oldest stash first; at most one per thread, none empty.
    list: VecDeque<(ThreadKey, String)>,
}

/// Task 6c: no draft text reaches `Debug`.
impl std::fmt::Debug for Drafts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drafts")
            .field("len", &self.list.len())
            .finish_non_exhaustive()
    }
}

impl Drafts {
    /// Keeps `text` as `thread`'s draft, replacing an older one; an empty
    /// text clears it. Full: `Err` hands the text back, nothing dropped.
    pub(crate) fn stash(&mut self, thread: ThreadKey, text: String) -> Result<(), String> {
        self.list.retain(|(t, _)| *t != thread);
        if text.is_empty() {
            return Ok(());
        }
        if self.list.len() >= MAX_DRAFTS {
            return Err(text);
        }
        self.list.push_back((thread, text));
        debug_assert!(self.list.len() <= MAX_DRAFTS);
        Ok(())
    }

    /// Drops the drafts of threads that can no longer be sent to
    /// (`obsolete`), and returns them, oldest first.
    pub(crate) fn discard(
        &mut self,
        obsolete: impl Fn(ThreadKey) -> bool,
    ) -> Vec<(ThreadKey, String)> {
        let (gone, kept) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|(t, _)| obsolete(*t));
        self.list = kept;
        gone.into()
    }

    /// Whether switching from `from` (its box holding `text`) to `to` can
    /// keep every draft: `to`'s draft leaves first (freeing its slot), and
    /// `from`'s replaces its own; an empty box stashes nothing.
    pub(crate) fn can_switch(&self, from: ThreadKey, to: ThreadKey, text: &str) -> bool {
        if text.is_empty() {
            return true;
        }
        let freed = self
            .list
            .iter()
            .filter(|(t, _)| *t == from || *t == to)
            .count();
        self.list.len() - freed < MAX_DRAFTS
    }

    /// Takes `thread`'s draft out.
    pub(crate) fn take(&mut self, thread: ThreadKey) -> Option<String> {
        let at = self.list.iter().position(|(t, _)| *t == thread)?;
        self.list.remove(at).map(|(_, text)| text)
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatId, PeerKind};

    use super::*;

    fn chat(id: i64) -> ThreadKey {
        ThreadKey::whole(ChatId {
            kind: PeerKind::User,
            id,
        })
    }

    #[test]
    fn one_draft_per_chat_bounded_and_never_dropped() {
        let mut d = Drafts::default();
        d.stash(chat(1), "a".into()).unwrap();
        d.stash(chat(1), "a2".into()).unwrap();
        d.stash(chat(2), String::new()).unwrap();
        assert_eq!(d.take(chat(2)), None, "empty clears");
        assert_eq!(d.take(chat(1)).as_deref(), Some("a2"));
        assert_eq!(d.take(chat(1)), None, "taken");
        let max = i64::try_from(MAX_DRAFTS).unwrap();
        for n in 0..max {
            d.stash(chat(n), format!("t{n}")).unwrap();
        }
        assert_eq!(d.stash(chat(max), "new".into()), Err("new".to_owned()));
        assert_eq!(d.take(chat(0)).as_deref(), Some("t0"), "nothing dropped");
        assert!(!format!("{d:?}").contains("t2"), "no text in Debug");
    }

    #[test]
    fn a_switch_needs_a_slot_only_for_new_text() {
        let mut d = Drafts::default();
        let max = i64::try_from(MAX_DRAFTS).unwrap();
        for n in 0..max {
            d.stash(chat(n), format!("t{n}")).unwrap();
        }
        let outside = chat(max);
        assert!(
            d.can_switch(outside, chat(0), "x"),
            "the destination's slot frees"
        );
        assert!(
            d.can_switch(chat(1), chat(max + 1), "x"),
            "replaces its own"
        );
        assert!(
            !d.can_switch(outside, chat(max + 1), "x"),
            "needs a new slot"
        );
        assert!(
            d.can_switch(outside, chat(max + 1), ""),
            "an empty box stashes nothing"
        );
    }
}
