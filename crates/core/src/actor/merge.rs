//! Light refresh of an open window: merges a server tail into the window in
//! place, so the user's view is not reset every few minutes. Falls back to
//! replacing the window when the tail reveals messages missing inside it (a
//! silent gap), which a merge cannot place.

use std::collections::VecDeque;

use super::Actor;
use super::apply::HistoryReply;
use crate::api::{Event, HistoryDir};
use crate::backend::{Backend, HistoryPage};
use crate::model::{Message, MsgId};
use crate::store::Change;

/// How a server tail differs from the window it refreshes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TailDiff {
    /// The tail holds messages the window lacks inside its range.
    pub(super) gap: bool,
    /// Window messages inside the tail's range that the server no longer has.
    pub(super) deleted: Vec<MsgId>,
    /// Server copies newer (later edit) than the window's.
    pub(super) edited: Vec<Message>,
    /// Messages newer than the window's newest.
    pub(super) appended: Vec<Message>,
}

/// Compares a tail `page` with an attached `window`, both ascending by id. An
/// empty window is reported as a gap: replace instead of merging. So is a
/// partial page that starts above the window's newest message: the messages
/// between them (more than a page arrived unseen) are known to neither. And
/// so is a page whose messages all lie below a window it deletes entirely
/// (task 10d): merged, the window would hold none of the tail.
///
/// The page is authoritative over `range`
/// ([`HistoryReply::authoritative_range`]): from the oldest id the server
/// sent (or the start of history) up to the authoritative top. Window
/// messages in that range that the page lacks were deleted, including the
/// newest ones and all of them when the server's history is empty. Window
/// messages outside it (older history, or live messages that arrived after
/// the request) are left alone; with no range, nothing is deleted.
pub(super) fn tail_diff(
    window: &VecDeque<Message>,
    page: &HistoryPage,
    range: Option<(MsgId, MsgId)>,
) -> TailDiff {
    let mut diff = TailDiff::default();
    let (Some(oldest), Some(newest)) = (window.front().map(|m| m.id), window.back().map(|m| m.id))
    else {
        diff.gap = true;
        return diff;
    };
    if page.has_more && range.is_some_and(|(lo, _)| lo.0 > newest.0.saturating_add(1)) {
        diff.gap = true;
    }
    for m in &page.messages {
        match window.binary_search_by_key(&m.id, |w| w.id) {
            Ok(i) => {
                if window.get(i).is_some_and(|w| m.version() > w.version()) {
                    diff.edited.push(m.clone());
                }
            }
            Err(_) if m.id > newest => diff.appended.push(m.clone()),
            Err(_) if m.id > oldest => diff.gap = true,
            // Older than the window: history the user has not loaded.
            Err(_) => {}
        }
    }
    let mut ids: Vec<MsgId> = page.messages.iter().map(|m| m.id).collect();
    ids.sort_unstable();
    if let Some((lo, top)) = range {
        diff.deleted = window
            .iter()
            .map(|w| w.id)
            .filter(|id| (lo..=top).contains(id) && ids.binary_search(id).is_err())
            .collect();
        // Every window message up to the top is gone, yet the server sent
        // history below the window, which a merge ignores: the window would
        // show none of what is left of the tail. Replace it instead.
        let kept = window
            .iter()
            .any(|w| w.id <= top && diff.deleted.binary_search(&w.id).is_err());
        if !kept && page.messages.iter().any(|m| m.id < oldest) {
            diff.gap = true;
        }
    }
    diff
}

impl<B: Backend> Actor<B> {
    /// Applies a refresh tail to the open window (see the module docs).
    pub(super) async fn merge_tail(&mut self, p: HistoryReply) {
        let Some(w) = self.windows.get(p.thread) else {
            return;
        };
        if w.has_newer() {
            return; // detached: scrolling back down loads current pages
        }
        let range = p.authoritative_range();
        let diff = tail_diff(w.messages(), &p.page, range);
        if diff.gap {
            return self.on_tail(p).await;
        }
        let (thread, view) = (p.thread, p.view);
        if let Some((lo, top)) = range {
            self.reconcile(thread, lo, top, &p.page.messages).await;
        }
        let removed = self.windows.delete(thread.chat, &diff.deleted);
        if !removed.is_empty() {
            let chat = thread.chat;
            self.emit(Event::Deleted { chat, ids: removed }).await;
        }
        for m in diff.edited {
            if self.windows.replace(thread, m.clone()) {
                self.emit(Event::Edited(m)).await;
            }
        }
        let mut change = Change::default();
        for m in diff.appended {
            let c = self.windows.upsert(thread, m);
            change.inserted.extend(c.inserted);
            change.removed.extend(c.removed);
        }
        if !change.inserted.is_empty() {
            self.emit_history(thread, view, HistoryDir::Newer, change)
                .await;
        }
        self.emit_reconciled(thread, view).await;
        self.tail_covers(thread, view, p.req, range.map(|(lo, _)| lo))
            .await;
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::actor::apply::MIN_ID;

    use crate::backend::fake::text_message;
    use crate::model::{ChatId, PeerKind};

    const CHAT: ChatId = ChatId {
        kind: PeerKind::User,
        id: 1,
    };

    fn msgs(ids: &[i32]) -> Vec<Message> {
        ids.iter().map(|i| text_message(CHAT, *i, "m")).collect()
    }

    fn ids(v: &[Message]) -> Vec<i32> {
        v.iter().map(|m| m.id.0).collect()
    }

    #[test]
    fn merge_finds_deletions_edits_and_appends() {
        let window: VecDeque<Message> = msgs(&[1, 2, 3, 4, 5]).into();
        let mut page = msgs(&[3, 5, 6]);
        page[1].edit_date = Some(9);
        let diff = tail_diff(
            &window,
            &HistoryPage {
                messages: page,
                has_more: true,
            },
            Some((MsgId(3), MsgId(6))),
        );
        assert!(!diff.gap);
        assert_eq!(diff.deleted, vec![MsgId(4)], "1 and 2 are below the page");
        assert_eq!(ids(&diff.edited), [5]);
        assert_eq!(ids(&diff.appended), [6]);
    }

    #[test]
    fn missing_message_inside_the_window_is_a_gap() {
        let window: VecDeque<Message> = msgs(&[1, 3, 5]).into();
        let page = HistoryPage {
            messages: msgs(&[1, 2, 3, 5]),
            has_more: false,
        };
        assert!(tail_diff(&window, &page, Some((MIN_ID, MsgId(5)))).gap);
    }

    #[test]
    fn deleted_newest_message_is_removed() {
        // The window held 1..=10 when the tail was requested; 10 was deleted
        // and the deletion missed, 11 arrived live after the request.
        let window: VecDeque<Message> = msgs(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]).into();
        let page = HistoryPage {
            messages: msgs(&[1, 2, 3, 4, 5, 6, 7, 8, 9]),
            has_more: false,
        };
        let diff = tail_diff(&window, &page, Some((MIN_ID, MsgId(10))));
        assert!(!diff.gap);
        assert_eq!(
            diff.deleted,
            vec![MsgId(10)],
            "11 arrived after the request"
        );
    }

    #[test]
    fn empty_server_history_removes_everything_requested() {
        let window: VecDeque<Message> = msgs(&[3, 4, 7]).into();
        let page = HistoryPage {
            messages: Vec::new(),
            has_more: false,
        };
        let diff = tail_diff(&window, &page, Some((MIN_ID, MsgId(4))));
        assert!(!diff.gap);
        assert_eq!(diff.deleted, vec![MsgId(3), MsgId(4)]);
    }
}
