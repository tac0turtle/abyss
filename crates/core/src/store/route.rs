//! Thread normalization: the one place where the topic the server stated
//! becomes a canonical [`ThreadKey`] (plan section 3).

use crate::model::{ChatId, Message, ThreadKey, TopicId};

/// What core knows about whether a chat is a forum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForumStatus {
    Forum,
    NotForum,
    /// Unknown, or fenced while its kind is re-checked (plan section 6.6).
    Unknown,
}

/// Where a live or loaded message goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Into this thread: windows, the cache (subject to continuity) and events.
    Thread(ThreadKey),
    /// The chat-list bump only: not cached, not put in any window.
    ChatOnly(ChatId),
    /// A topic-headed message in a chat core thinks is not a forum: run the
    /// forum-change path, otherwise as [`Route::ChatOnly`].
    ForumSuspected(ChatId),
}

/// Routes a message of `chat` whose forum header named `raw` (`None` when it
/// had none).
pub fn route(chat: ChatId, raw: Option<TopicId>, forum: ForumStatus) -> Route {
    match (forum, raw) {
        (ForumStatus::Forum, _) => Route::Thread(ThreadKey {
            chat,
            topic: Some(raw.unwrap_or(TopicId::GENERAL)),
        }),
        (ForumStatus::NotForum, Some(_)) => Route::ForumSuspected(chat),
        (ForumStatus::NotForum, None) => Route::Thread(ThreadKey::whole(chat)),
        (ForumStatus::Unknown, _) => Route::ChatOnly(chat),
    }
}

/// Routes `message` and, for [`Route::Thread`], rewrites its `thread` to the
/// key. Normalizing a normalized message is the identity.
pub fn normalize(mut message: Message, forum: ForumStatus) -> (Route, Message) {
    let route = route(message.thread.chat, message.thread.topic, forum);
    if let Route::Thread(key) = route {
        message.thread = key;
    }
    (route, message)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::model::{Body, MsgId, PeerKind};

    fn chat(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::Channel,
            id,
        }
    }

    fn message(chat: ChatId, topic: Option<TopicId>) -> Message {
        Message {
            id: MsgId(10),
            thread: ThreadKey { chat, topic },
            sender: None,
            date: 0,
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body: Body::Unsupported,
        }
    }

    #[test]
    fn every_combination_follows_the_table() {
        let c = chat(5);
        let t = TopicId::try_from(42).ok();
        let general = Some(TopicId::GENERAL);
        let cases = [
            (
                ForumStatus::Forum,
                None,
                Route::Thread(ThreadKey {
                    chat: c,
                    topic: general,
                }),
            ),
            (
                ForumStatus::Forum,
                t,
                Route::Thread(ThreadKey { chat: c, topic: t }),
            ),
            (
                ForumStatus::NotForum,
                None,
                Route::Thread(ThreadKey::whole(c)),
            ),
            (ForumStatus::NotForum, t, Route::ForumSuspected(c)),
            (ForumStatus::Unknown, None, Route::ChatOnly(c)),
            (ForumStatus::Unknown, t, Route::ChatOnly(c)),
        ];
        for (forum, raw, want) in cases {
            assert_eq!(route(c, raw, forum), want, "{forum:?} {raw:?}");
            // Normalizing a normalized message changes nothing.
            let (_, once) = normalize(message(c, raw), forum);
            let (again, twice) = normalize(once.clone(), forum);
            assert_eq!((again, twice), (want, once), "{forum:?} {raw:?}");
        }
    }

    fn status() -> impl Strategy<Value = ForumStatus> {
        prop_oneof![
            Just(ForumStatus::Forum),
            Just(ForumStatus::NotForum),
            Just(ForumStatus::Unknown),
        ]
    }

    fn raw_topic() -> impl Strategy<Value = Option<TopicId>> {
        proptest::option::of(
            (1..=i32::MAX).prop_filter_map("positive", |n| TopicId::try_from(n).ok()),
        )
    }

    proptest! {
        /// A forum key always has a topic, a non-forum key never does, and
        /// normalizing twice changes nothing.
        #[test]
        fn route_is_canonical_and_idempotent(raw in raw_topic(), forum in status(), id in 1i64..100) {
            let (first, once) = normalize(message(chat(id), raw), forum);
            match first {
                Route::Thread(key) => {
                    prop_assert_eq!(once.thread, key);
                    prop_assert_eq!(key.topic.is_some(), forum == ForumStatus::Forum);
                    let (again, twice) = normalize(once.clone(), forum);
                    prop_assert_eq!(again, first);
                    prop_assert_eq!(twice, once);
                }
                Route::ChatOnly(c) | Route::ForumSuspected(c) => {
                    prop_assert_eq!(c, chat(id));
                    prop_assert_eq!(once.thread.topic, raw, "unrouted messages are not rewritten");
                }
            }
        }
    }
}
