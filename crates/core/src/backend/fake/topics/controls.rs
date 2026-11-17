//! Test-side controls of the fake server's forums. Each change updates the
//! server state first, then emits the update Telegram would send, which
//! [`FakeHandle::drop_next_update`] can make lost.

use super::super::{FakeHandle, State};
use super::{forum_status, next_message, raw_thread, rejected, server_order, stored_mut};
use crate::backend::{BackendError, RejectReason, Update};
use crate::model::{
    Body, ChatId, ChatKind, Freshness, Message, MsgId, ThreadKey, Topic, TopicId, TopicOrder,
    TopicSnapshot, Visibility,
};
use crate::sanitize::display_name;
use crate::store::{ForumStatus, PostClosed, TopicEdit};

impl FakeHandle {
    /// `chat`'s topics as the server would serve them now, in its order.
    pub fn server_topics(&self, chat: ChatId) -> Vec<TopicSnapshot> {
        self.shared
            .with(|s| server_order(s, chat, super::order_of(s, chat)))
    }

    /// Changes the order the forum's topic list is served in. No update is
    /// sent: the next page simply reports the new order.
    pub fn set_topic_order(&self, chat: ChatId, order: TopicOrder) {
        self.shared.with(|s| {
            s.topic_order.insert(chat, order);
        });
    }

    /// `peer_dialogs` calls as they reached the server, oldest first.
    pub fn peer_calls(&self) -> Vec<super::super::PeerCallRecord> {
        self.shared.with(|s| s.peer_log.clone())
    }

    /// The topic is read up to `max_id` on another device: the server's
    /// read mark moves, and `ReadTopicMax` is emitted
    /// (`updateReadChannelDiscussionInbox`).
    pub async fn read_remotely(
        &self,
        thread: ThreadKey,
        max_id: MsgId,
    ) -> Result<(), BackendError> {
        let topic = thread.topic.ok_or(rejected(RejectReason::Other))?;
        self.shared.with(|s| {
            let snap =
                stored_mut(s, thread.chat, topic).ok_or(rejected(RejectReason::TopicDeleted))?;
            snap.topic.read_inbox_max = snap.topic.read_inbox_max.max(max_id);
            Ok::<(), BackendError>(())
        })?;
        self.push_update(Update::ReadTopicMax {
            chat: thread.chat,
            topic,
            max_id,
        })
        .await;
        Ok(())
    }

    /// Topics calls as they reached the server, oldest first.
    pub fn topics_calls(&self) -> Vec<super::super::TopicsCallRecord> {
        self.shared.with(|s| s.topics_log.clone())
    }

    /// From now on, every later page of the forum repeats its first page
    /// while still reporting more topics (a server whose `count` is
    /// inflated): only core's duplicate-page stop ends the traversal.
    pub fn repeat_topic_pages(&self, chat: ChatId) {
        self.shared.with(|s| {
            if !s.repeat_pages.contains(&chat) {
                s.repeat_pages.push(chat);
            }
        });
    }

    /// Whether the account may post in the forum's closed topics.
    pub fn set_post_closed(&self, chat: ChatId, post_closed: PostClosed) {
        self.shared.with(|s| {
            s.post_closed.insert(chat, post_closed);
        });
    }

    /// Changes a chat's kind (for example turns a group into a forum and
    /// back) and emits `ChatChanged`, as `updateChannel` would.
    pub async fn set_kind(&self, chat: ChatId, kind: ChatKind) {
        self.shared.with(|s| {
            if let Some(summary) = s.dialogs.iter_mut().find(|c| c.id == chat) {
                summary.kind = kind;
            }
        });
        self.push_update(Update::ChatChanged { chat }).await;
    }

    /// Someone else creates a topic: its creation message (a service message
    /// whose topic is its own id) is added and emitted as a new message.
    pub async fn create_topic(&self, chat: ChatId, title: &str) -> Result<TopicId, BackendError> {
        let (topic, message) = self.shared.with(|s| create(s, chat, title))?;
        self.push_update(Update::NewMessage(message)).await;
        Ok(topic)
    }

    /// Someone else posts `text` into `thread` (General for a whole-chat key
    /// of a forum): the message is added and emitted.
    pub async fn incoming(&self, thread: ThreadKey, text: &str) -> Result<Message, BackendError> {
        let message = self.shared.with(|s| {
            let message = next_message(s, raw_thread(thread), text)?;
            s.history
                .entry(thread.chat)
                .or_default()
                .push(message.clone());
            Ok::<_, BackendError>(message)
        })?;
        self.push_update(Update::NewMessage(message.clone())).await;
        Ok(message)
    }

    /// Edits a topic's title, closed flag or visibility (General only), and
    /// emits `TopicEdited` with the changed fields.
    pub async fn edit_topic(
        &self,
        chat: ChatId,
        topic: TopicId,
        edit: TopicEdit,
    ) -> Result<(), BackendError> {
        let title = edit.title.as_deref().map(display_name);
        self.shared.with(|s| {
            let snap = stored_mut(s, chat, topic).ok_or(rejected(RejectReason::TopicDeleted))?;
            if let Some(title) = &title {
                snap.topic.title.clone_from(title);
            }
            if let Some(closed) = edit.closed {
                snap.topic.closed = closed;
            }
            if let Some(visibility) = edit.visibility {
                snap.visibility = visibility;
            }
            Ok(())
        })?;
        self.push_update(Update::TopicEdited {
            chat,
            topic,
            title,
            closed: edit.closed,
            visibility: edit.visibility,
        })
        .await;
        Ok(())
    }

    pub async fn close_topic(&self, chat: ChatId, topic: TopicId) -> Result<(), BackendError> {
        let edit = TopicEdit {
            closed: Some(true),
            ..TopicEdit::default()
        };
        self.edit_topic(chat, topic, edit).await
    }

    pub async fn reopen_topic(&self, chat: ChatId, topic: TopicId) -> Result<(), BackendError> {
        let edit = TopicEdit {
            closed: Some(false),
            ..TopicEdit::default()
        };
        self.edit_topic(chat, topic, edit).await
    }

    /// Hides or shows General (`updateEditForumTopic.hidden`).
    pub async fn set_general_visibility(
        &self,
        chat: ChatId,
        visibility: Visibility,
    ) -> Result<(), BackendError> {
        let edit = TopicEdit {
            visibility: Some(visibility),
            ..TopicEdit::default()
        };
        self.edit_topic(chat, TopicId::GENERAL, edit).await
    }

    /// Deletes a topic with all its messages, its root included, and emits
    /// the deletion of those messages (there is no "topic deleted" update).
    /// By-id calls then report the topic deleted. General can't be deleted.
    pub async fn delete_topic(&self, chat: ChatId, topic: TopicId) -> Result<(), BackendError> {
        let ids = self.shared.with(|s| delete(s, chat, topic))?;
        self.push_update(Update::Deleted {
            chat: Some(chat),
            ids,
        })
        .await;
        Ok(())
    }

    /// Pins a topic (it leads the pinned topics) and emits `TopicsPinned`.
    pub async fn pin_topic(&self, chat: ChatId, topic: TopicId) -> Result<(), BackendError> {
        self.shared
            .with(|s| set_pinned(s, chat, topic, Pin::Pinned))?;
        self.push_update(Update::TopicsPinned { chat }).await;
        Ok(())
    }

    /// Unpins a topic and emits `TopicsPinned`.
    pub async fn unpin_topic(&self, chat: ChatId, topic: TopicId) -> Result<(), BackendError> {
        self.shared
            .with(|s| set_pinned(s, chat, topic, Pin::Unpinned))?;
        self.push_update(Update::TopicsPinned { chat }).await;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pin {
    Pinned,
    Unpinned,
}

fn create(s: &mut State, chat: ChatId, title: &str) -> Result<(TopicId, Message), BackendError> {
    if forum_status(s, chat) != ForumStatus::Forum {
        return Err(rejected(RejectReason::NotForum));
    }
    let title = display_name(title);
    let mut message = next_message(s, ThreadKey::whole(chat), "")?;
    let topic = TopicId::try_from(message.id.0).map_err(|_| rejected(RejectReason::Other))?;
    message.thread.topic = Some(topic);
    message.body = Body::Service(format!("Someone created the topic «{title}»"));
    let snap = TopicSnapshot {
        topic: Topic {
            id: topic,
            title,
            created: message.date,
            closed: false,
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            freshness: Freshness::Current,
            read_inbox_max: MsgId(0),
            last: None,
            last_date: 0,
            last_id: None,
        },
        visibility: Visibility::Shown,
        top_version: i64::MIN,
    };
    s.topics.entry(chat).or_default().push(snap);
    s.history.entry(chat).or_default().push(message.clone());
    Ok((topic, message))
}

fn delete(s: &mut State, chat: ChatId, topic: TopicId) -> Result<Vec<MsgId>, BackendError> {
    if topic == TopicId::GENERAL {
        return Err(rejected(RejectReason::Other));
    }
    let topics = s
        .topics
        .get_mut(&chat)
        .ok_or(rejected(RejectReason::TopicDeleted))?;
    let before = topics.len();
    topics.retain(|t| t.topic.id != topic);
    if topics.len() == before {
        return Err(rejected(RejectReason::TopicDeleted));
    }
    let history = s.history.entry(chat).or_default();
    let ids = history
        .iter()
        .filter(|m| m.thread.topic == Some(topic))
        .map(|m| m.id)
        .collect();
    history.retain(|m| m.thread.topic != Some(topic));
    Ok(ids)
}

fn set_pinned(s: &mut State, chat: ChatId, topic: TopicId, pin: Pin) -> Result<(), BackendError> {
    let topics = s
        .topics
        .get_mut(&chat)
        .ok_or(rejected(RejectReason::TopicDeleted))?;
    let at = topics
        .iter()
        .position(|t| t.topic.id == topic)
        .ok_or(rejected(RejectReason::TopicDeleted))?;
    let mut snap = topics.remove(at);
    snap.topic.pinned = pin == Pin::Pinned;
    // Pinned topics are served in stored order: the newest pin leads.
    match pin {
        Pin::Pinned => topics.insert(0, snap),
        Pin::Unpinned => topics.push(snap),
    }
    Ok(())
}
