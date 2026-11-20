//! The baseline race of the update stream, over a scripted source.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use grammers_client::InvocationError;
use tokio::sync::{mpsc, watch};

use crate::backend::grammers::stream::{Pull, Source, Updates};
use crate::backend::{StreamError, StreamItem, Update};
use crate::model::{ChatId, MsgId, PeerKind};

type Item = Result<Option<Update>, InvocationError>;

/// Like grammers: the first pull records the baseline (`getState`), then
/// every pull waits for the next scripted item.
struct Script {
    items: mpsc::Receiver<Item>,
    baseline: Option<Arc<watch::Sender<bool>>>,
    pulls: Arc<AtomicUsize>,
}

impl Source for Script {
    type Raw = Option<Update>;

    fn pull(mut self) -> Pull<Self> {
        self.pulls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(flag) = self.baseline.take() {
                flag.send_replace(true);
                // Keep the sender alive, as the session does.
                self.baseline = Some(flag);
            }
            let res = match self.items.recv().await {
                Some(item) => item,
                None => Err(InvocationError::Dropped("script ended")),
            };
            (self, res)
        })
    }

    fn map(raw: Self::Raw) -> Option<StreamItem> {
        raw.map(StreamItem::Update)
    }
}

struct Harness {
    updates: Updates<Script>,
    items: mpsc::Sender<Item>,
    pulls: Arc<AtomicUsize>,
    _flag: Arc<watch::Sender<bool>>,
}

/// Whether the source records a baseline (`getState` succeeded).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Baseline {
    Recorded,
    Missing,
}

fn harness(baseline: Baseline) -> Harness {
    let (tx, rx) = mpsc::channel(8);
    let flag = Arc::new(watch::channel(false).0);
    let pulls = Arc::new(AtomicUsize::new(0));
    let script = Script {
        items: rx,
        baseline: (baseline == Baseline::Recorded).then(|| Arc::clone(&flag)),
        pulls: Arc::clone(&pulls),
    };
    Harness {
        updates: Updates::new(script, flag.subscribe(), crate::backend::Baseline::Fresh),
        items: tx,
        pulls,
        _flag: flag,
    }
}

fn read(still_unread: u32) -> Update {
    Update::ReadInbox {
        chat: ChatId {
            kind: PeerKind::User,
            id: 7,
        },
        topic: None,
        still_unread,
        max_id: MsgId(0),
    }
}

async fn next(h: &mut Harness) -> Result<StreamItem, StreamError> {
    tokio::time::timeout(Duration::from_secs(5), h.updates.next())
        .await
        .expect("next must not hang")
}

fn unread(item: Result<StreamItem, StreamError>) -> u32 {
    match item {
        Ok(StreamItem::Update(Update::ReadInbox { still_unread, .. })) => still_unread,
        other => panic!("expected an update, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn baseline_is_reported_while_the_first_pull_waits() {
    let mut h = harness(Baseline::Recorded);
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline(_))));
    h.items.send(Ok(Some(read(1)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 1);
    assert_eq!(
        h.pulls.load(Ordering::SeqCst),
        1,
        "the pull interrupted by the baseline was resumed, not restarted"
    );
}

#[tokio::test(start_paused = true)]
async fn baseline_comes_before_an_update_that_raced_it() {
    let mut h = harness(Baseline::Recorded);
    h.items.send(Ok(Some(read(2)))).await.unwrap();
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline(_))));
    assert_eq!(unread(next(&mut h).await), 2);
}

#[tokio::test(start_paused = true)]
async fn errors_are_classified_and_the_stream_is_reused() {
    let mut h = harness(Baseline::Recorded);
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline(_))));
    let io = InvocationError::Io(std::io::Error::other("reset"));
    h.items.send(Err(io)).await.unwrap();
    assert_eq!(next(&mut h).await.unwrap_err(), StreamError::Transient);
    h.items.send(Ok(None)).await.unwrap();
    h.items.send(Ok(Some(read(3)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 3, "ignored updates are skipped");
    assert_eq!(h.pulls.load(Ordering::SeqCst), 3);
    h.items
        .send(Err(InvocationError::Dropped("runner gone")))
        .await
        .unwrap();
    assert_eq!(next(&mut h).await.unwrap_err(), StreamError::Fatal);
}

#[tokio::test(start_paused = true)]
async fn without_a_baseline_updates_still_flow() {
    // `getState` failed: core's bootstrap times out and rebuilds; the stream
    // itself keeps delivering.
    let mut h = harness(Baseline::Missing);
    h.items.send(Ok(Some(read(4)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 4);
}

pub(super) mod differences {
    mod round_7;

    use grammers_client::InvocationError;
    use grammers_client::sender::ReadError;
    use grammers_client::tl::{
        self, Serializable as _,
        enums::updates::{ChannelDifference, Difference},
    };
    use grammers_session::Session as _;
    use grammers_session::types::{ChannelState, PeerAuth, PeerInfo, UpdatesState};
    use grammers_session::updates::UpdatesLike;

    use super::super::calls::{Mock, rpc};
    use super::super::{message, notify_settings, peer_channel, peer_user};
    use super::{Arc, Duration, StreamError, StreamItem, Update, mpsc};
    use crate::backend::StaleHistory;
    use crate::backend::grammers::errors::{kind, stream_error};
    use crate::backend::grammers::feed::{FEED_RPC_DEADLINE, Feed};
    use crate::backend::grammers::invoke::Invoke;
    use crate::model::{ChatId, MsgId, PeerKind};
    use crate::secret::MemoryStore;
    use crate::session::{AbyssSession, PersistHandle};

    fn resumed() -> UpdatesState {
        UpdatesState {
            pts: 10,
            date: 1,
            ..UpdatesState::default()
        }
    }

    fn session() -> Arc<AbyssSession> {
        let persist = PersistHandle::spawn(MemoryStore::new()).unwrap();
        Arc::new(AbyssSession::from_snapshot(persist.fresh_session()).unwrap())
    }

    fn raw(id: i32) -> tl::enums::Message {
        message(peer_user(7), id, "private text").into()
    }

    fn state(pts: i32) -> tl::enums::updates::State {
        tl::types::updates::State {
            pts,
            qts: 0,
            date: 1,
            seq: 0,
            unread_count: 0,
        }
        .into()
    }

    #[tokio::test]
    async fn difference_slices_have_one_boundary_before_socket_updates() {
        let first = tl::types::updates::DifferenceSlice {
            new_messages: vec![raw(11)],
            new_encrypted_messages: vec![],
            other_updates: vec![],
            chats: vec![],
            users: vec![],
            intermediate_state: state(11),
        };
        let last = tl::types::updates::Difference {
            new_messages: vec![raw(12)],
            new_encrypted_messages: vec![],
            other_updates: vec![],
            chats: vec![],
            users: vec![],
            state: state(12),
        };
        let mock = Mock::with(vec![
            Ok(Difference::Slice(first).to_bytes()),
            Ok(Difference::Difference(last).to_bytes()),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(Arc::clone(&mock), session(), input, Some(resumed()));
        for id in [11, 12] {
            assert!(matches!(
                feed.next().await.unwrap(),
                StreamItem::CatchUp(Update::NewMessage(new)) if new.message.id.0 == id
            ));
        }
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::Checkpoint(s) if s.0.pts == 12));
        let updates = tl::types::Updates {
            updates: vec![
                tl::types::UpdateNewMessage {
                    message: raw(13),
                    pts: 13,
                    pts_count: 1,
                }
                .into(),
            ],
            users: vec![],
            chats: vec![],
            date: 1,
            seq: 0,
        };
        socket
            .send(grammers_session::updates::UpdatesLike::Updates(
                updates.into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::Update(Update::NewMessage(new)) if new.message.id.0 == 13
        ));
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn difference_too_long_requests_reconcile_without_synthetic_messages() {
        let reply = Difference::TooLong(tl::types::updates::DifferenceTooLong { pts: 17 });
        let mock = Mock::with(vec![Ok(reply.to_bytes())]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session(), input, Some(resumed()));
        assert_stale(&feed.next().await.unwrap(), None);
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::Checkpoint(s) if s.0.pts == 17));
    }

    const CHANNEL: i64 = 5;
    const CHANNEL_CHAT: ChatId = ChatId {
        kind: PeerKind::Channel,
        id: CHANNEL,
    };

    fn resumed_with_channel() -> UpdatesState {
        UpdatesState {
            channels: vec![ChannelState {
                id: CHANNEL,
                pts: 3,
            }],
            ..resumed()
        }
    }

    async fn channel_session() -> Arc<AbyssSession> {
        let session = session();
        session
            .cache_peer(PeerInfo::Channel {
                id: CHANNEL,
                auth: Some(PeerAuth::from_hash(9)),
                kind: None,
            })
            .await
            .unwrap();
        session
    }

    fn empty_account_difference() -> Vec<u8> {
        Difference::Empty(tl::types::updates::DifferenceEmpty { date: 1, seq: 0 }).to_bytes()
    }

    fn account_reply(pts: i32, other_updates: Vec<tl::enums::Update>) -> Vec<u8> {
        Difference::Difference(tl::types::updates::Difference {
            new_messages: vec![],
            new_encrypted_messages: vec![],
            other_updates,
            chats: vec![],
            users: vec![],
            state: state(pts),
        })
        .to_bytes()
    }

    fn common_edit(pts: i32, pts_count: i32) -> tl::enums::Update {
        tl::types::UpdateEditMessage {
            message: raw(11),
            pts,
            pts_count,
        }
        .into()
    }

    async fn keep_common_active(feed: &mut Feed<Arc<Mock>>, socket: &mpsc::Sender<UpdatesLike>) {
        // Keep Common's idle deadline later than the other box under test.
        tokio::time::sleep(Duration::from_secs(1)).await;
        socket
            .send(socket_batch(vec![
                tl::types::UpdateNewMessage {
                    message: raw(11),
                    pts: 11,
                    pts_count: 1,
                }
                .into(),
            ]))
            .await
            .unwrap();
        assert!(matches!(feed.next().await.unwrap(), StreamItem::Update(_)));
        assert_eq!(checkpoint_of(feed).await.pts, 11);
    }

    async fn after_idle(feed: &mut Feed<Arc<Mock>>) -> StreamItem {
        tokio::time::timeout(Duration::from_mins(16), feed.next())
            .await
            .expect("the grammers idle deadline must fire")
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "waits for the pinned grammers 15-minute std::time::Instant deadline"]
    async fn a_channel_idle_deadline_with_an_empty_reply_only_checkpoints() {
        let mock = Mock::with(vec![
            Ok(empty_account_difference()),
            Ok(empty_channel_difference(3)),
            Ok(empty_channel_difference(4)),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            Arc::clone(&mock),
            channel_session().await,
            input,
            Some(resumed_with_channel()),
        );
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
        keep_common_active(&mut feed, &socket).await;
        let next = after_idle(&mut feed).await;
        assert!(matches!(next, StreamItem::CatchUpEnded), "{next:?}");
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 4);
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    #[ignore = "waits for the pinned grammers 15-minute std::time::Instant deadline"]
    async fn a_secondary_idle_deadline_preserves_history_when_a_common_edit_applies() {
        let mock = Mock::with(vec![
            Ok(state(10).to_bytes()),
            Ok(account_reply(11, vec![common_edit(11, 0)])),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session(), input, None);
        checkpoint_of(&mut feed).await;
        keep_common_active(&mut feed, &socket).await;
        let edit = after_idle(&mut feed).await;
        assert!(
            matches!(edit, StreamItem::CatchUp(Update::Edited(_))),
            "{edit:?}"
        );
        let next = feed.next().await.unwrap();
        assert!(matches!(next, StreamItem::CatchUpEnded), "{next:?}");
        assert_eq!(checkpoint_of(&mut feed).await.pts, 11);
    }

    #[tokio::test]
    async fn a_server_scheduled_channel_idle_poll_only_checkpoints() {
        let schedule_poll = ChannelDifference::Empty(tl::types::updates::ChannelDifferenceEmpty {
            r#final: true,
            pts: 3,
            timeout: Some(0),
        });
        let mock = Mock::with(vec![
            Ok(empty_account_difference()),
            Ok(schedule_poll.to_bytes()),
            Ok(empty_account_difference()),
            Ok(empty_channel_difference(4)),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            Arc::clone(&mock),
            channel_session().await,
            input,
            Some(UpdatesState {
                qts: 1,
                ..resumed_with_channel()
            }),
        );
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
        // A Secondary reorder deadline wakes grammers to check the idle timeout
        // the server supplied for the channel. The channel has no gap.
        socket
            .send(socket_batch(vec![
                tl::types::UpdateBotStopped {
                    user_id: 7,
                    date: 1,
                    stopped: true,
                    qts: 3,
                }
                .into(),
            ]))
            .await
            .unwrap();
        assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
        let next = tokio::time::timeout(Duration::from_secs(5), feed.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(next, StreamItem::CatchUpEnded), "{next:?}");
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 4);
        assert_eq!(mock.requests().len(), 4);
    }

    fn channel_hint(pts: i32) -> tl::enums::Update {
        tl::types::UpdateChannelTooLong {
            channel_id: CHANNEL,
            pts: Some(pts),
        }
        .into()
    }

    fn final_channel_difference(updates: Vec<tl::enums::Update>) -> Vec<u8> {
        ChannelDifference::Difference(tl::types::updates::ChannelDifference {
            r#final: true,
            pts: 4,
            timeout: None,
            new_messages: vec![],
            other_updates: updates,
            chats: vec![],
            users: vec![],
        })
        .to_bytes()
    }

    fn channel_delete() -> tl::enums::Update {
        channel_delete_at(6, 4)
    }

    fn channel_delete_at(id: i32, pts: i32) -> tl::enums::Update {
        tl::types::UpdateDeleteChannelMessages {
            channel_id: CHANNEL,
            messages: vec![id],
            pts,
            pts_count: 1,
        }
        .into()
    }

    fn assert_stale(item: &StreamItem, chat: Option<ChatId>) {
        assert!(matches!(item, StreamItem::HistoryStale(StaleHistory(scope)) if *scope == chat));
    }

    #[tokio::test]
    async fn a_final_channel_difference_with_ordered_history_requests_a_reconcile() {
        let mut feed = channel_feed(final_channel_difference(vec![channel_delete()])).await;
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 4);
    }

    async fn assert_incomplete_channel_marks_stale(
        reply: Option<super::super::calls::Reply>,
        expected_pts: Option<i32>,
    ) {
        let account = Ok(empty_account_difference());
        let session = if reply.is_some() {
            channel_session().await
        } else {
            session()
        };
        let mock = Mock::with(std::iter::once(account).chain(reply).collect());
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session, input, Some(resumed_with_channel()));
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        let checkpoint = checkpoint_of(&mut feed).await;
        assert_eq!(checkpoint.pts, 10);
        assert_eq!(
            checkpoint
                .channels
                .iter()
                .find(|c| c.id == CHANNEL)
                .map(|c| c.pts),
            expected_pts,
        );
    }

    #[tokio::test]
    async fn a_private_channel_difference_marks_history_stale() {
        assert_incomplete_channel_marks_stale(Some(rpc(400, "CHANNEL_PRIVATE")), None).await;
    }

    #[tokio::test]
    async fn a_missing_channel_hash_marks_history_stale() {
        assert_incomplete_channel_marks_stale(None, None).await;
    }

    #[tokio::test]
    async fn a_temporary_channel_server_error_marks_history_stale() {
        assert_incomplete_channel_marks_stale(
            Some(rpc(500, "PERSISTENT_TIMESTAMP_OUTDATED")),
            Some(3),
        )
        .await;
    }

    #[tokio::test]
    async fn a_channel_mutation_in_an_account_difference_can_leave_history_stale() {
        let mock = Mock::with(vec![
            Ok(account_reply(11, vec![channel_delete()])),
            Ok(empty_channel_difference(4)),
        ]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            mock,
            channel_session().await,
            input,
            Some(resumed_with_channel()),
        );
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.pts, 11);
    }

    async fn secondary_gap_feed(reply: Vec<u8>) -> (Feed<Arc<Mock>>, mpsc::Sender<UpdatesLike>) {
        let mock = Mock::with(vec![Ok(state(10).to_bytes()), Ok(reply)]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session(), input, None);
        checkpoint_of(&mut feed).await;
        // grammers uses std::time::Instant. Trigger the same Secondary-only
        // recovery via its 500 ms out-of-order deadline, with Common active.
        socket
            .send(socket_batch(vec![
                tl::types::UpdateNewMessage {
                    message: raw(11),
                    pts: 11,
                    pts_count: 1,
                }
                .into(),
                tl::types::UpdateBotStopped {
                    user_id: 7,
                    date: 1,
                    stopped: true,
                    qts: 2,
                }
                .into(),
            ]))
            .await
            .unwrap();
        assert_eq!(checkpoint_of(&mut feed).await.pts, 11);
        (feed, socket)
    }

    #[tokio::test]
    async fn a_secondary_only_gap_marks_untrusted_common_history_before_checkpointing() {
        let delete = tl::types::UpdateDeleteMessages {
            messages: vec![6],
            pts: 12,
            pts_count: 1,
        };
        let (mut feed, _socket) = secondary_gap_feed(account_reply(12, vec![delete.into()])).await;
        assert_stale(&feed.next().await.unwrap(), None);
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.pts, 12);
    }

    #[tokio::test]
    async fn a_secondary_only_difference_preserves_history_for_an_applied_common_edit() {
        let (mut feed, _socket) =
            secondary_gap_feed(account_reply(11, vec![common_edit(11, 0)])).await;
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUp(Update::Edited(_))
        ));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.pts, 11);
    }

    #[tokio::test]
    async fn a_dropped_common_edit_and_read_invalidate_only_their_chats() {
        let read = tl::types::UpdateReadHistoryInbox {
            folder_id: None,
            peer: peer_user(8),
            top_msg_id: None,
            max_id: 15,
            still_unread_count: 0,
            pts: 13,
            pts_count: 1,
        };
        let (mut feed, _socket) =
            secondary_gap_feed(account_reply(13, vec![common_edit(12, 1), read.into()])).await;
        for id in [7, 8] {
            assert_stale(
                &feed.next().await.unwrap(),
                Some(ChatId {
                    kind: PeerKind::User,
                    id,
                }),
            );
        }
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.pts, 13);
    }

    #[tokio::test]
    async fn a_hinted_socket_batch_is_catch_up_before_the_checkpoint() {
        let mock = Mock::with(vec![
            Ok(state(10).to_bytes()),
            Ok(empty_channel_difference(10)),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(Arc::clone(&mock), channel_session().await, input, None);
        checkpoint_of(&mut feed).await;
        socket
            .send(socket_batch(vec![channel_new(CHANNEL, 3)]))
            .await
            .unwrap();
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
        socket
            .send(socket_batch(vec![
                channel_new(CHANNEL, 4),
                channel_hint(10),
            ]))
            .await
            .unwrap();
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUp(Update::NewMessage(_))
        ));
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert_eq!(
            mock.requests().len(),
            2,
            "the reconcile follows the channel reply"
        );
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 10);
    }

    #[tokio::test]
    async fn a_channel_hint_in_an_account_difference_marks_the_chat_stale() {
        let reply = account_reply(11, vec![channel_hint(10)]);
        let mock = Mock::with(vec![Ok(reply), Ok(empty_channel_difference(10))]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, channel_session().await, input, Some(resumed()));
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 10);
    }

    #[tokio::test]
    async fn a_channel_gap_deadline_marks_history_stale_even_after_an_empty_reply() {
        let mock = Mock::with(vec![
            Ok(state(10).to_bytes()),
            Ok(empty_channel_difference(5)),
        ]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, channel_session().await, input, None);
        checkpoint_of(&mut feed).await;
        socket
            .send(socket_batch(vec![channel_new(CHANNEL, 3)]))
            .await
            .unwrap();
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
        socket
            .send(socket_batch(vec![channel_delete_at(6, 5)]))
            .await
            .unwrap();
        // The deletion was buffered behind the missing pts 4.
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 5);
    }

    #[tokio::test]
    async fn a_known_socket_gap_reconciles_even_without_mapped_replay() {
        for (gap, reply, pts) in [
            (
                UpdatesLike::ConnectionClosed,
                empty_account_difference(),
                10,
            ),
            (
                UpdatesLike::Updates(tl::enums::Updates::TooLong),
                empty_account_difference(),
                10,
            ),
            (
                UpdatesLike::ConnectionClosed,
                account_reply(11, vec![tl::enums::Update::Config]),
                11,
            ),
        ] {
            let mock = Mock::with(vec![Ok(state(10).to_bytes()), Ok(reply)]);
            let (socket, input) = mpsc::channel(1);
            let mut feed = Feed::new(mock, session(), input, None);
            checkpoint_of(&mut feed).await;
            socket.send(gap).await.unwrap();
            assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
            assert!(matches!(
                feed.next().await.unwrap(),
                StreamItem::CatchUpEnded
            ));
            assert_eq!(checkpoint_of(&mut feed).await.pts, pts);
        }
    }

    #[tokio::test]
    async fn an_empty_account_difference_without_a_known_gap_skips_reconcile() {
        let mock = Mock::with(vec![Ok(empty_account_difference())]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session(), input, Some(resumed()));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::Checkpoint(s) if s.0.pts == 10));
    }

    #[tokio::test]
    async fn an_explicit_pts_reset_uses_get_state_before_accepting_a_lower_position() {
        let mock = Mock::with(vec![Ok(state(10).to_bytes()), Ok(state(4).to_bytes())]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(Arc::clone(&mock), session(), input, None);
        checkpoint_of(&mut feed).await;
        socket
            .send(UpdatesLike::Updates(
                tl::types::UpdateShort {
                    update: tl::enums::Update::PtsChanged,
                    date: 2,
                }
                .into(),
            ))
            .await
            .unwrap();
        assert_stale(&feed.next().await.unwrap(), None);
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert_eq!(
            mock.requests(),
            vec![tl::functions::updates::GetState {}.to_bytes(); 2]
        );
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert_eq!(checkpoint_of(&mut feed).await.pts, 4);
        socket
            .send(socket_batch(vec![
                tl::types::UpdateNewMessage {
                    message: raw(5),
                    pts: 5,
                    pts_count: 1,
                }
                .into(),
            ]))
            .await
            .unwrap();
        assert!(
            matches!(feed.next().await.unwrap(), StreamItem::Update(Update::NewMessage(new)) if new.message.id == MsgId(5))
        );
        assert_eq!(checkpoint_of(&mut feed).await.pts, 5);
    }

    #[tokio::test]
    async fn an_account_reset_abandons_pending_channels_and_retries_get_state() {
        let mock = Mock::with(vec![
            Ok(account_reply(11, vec![tl::enums::Update::PtsChanged])),
            Err(InvocationError::Io(std::io::Error::other("reset"))),
            Ok(state(4).to_bytes()),
        ]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            Arc::clone(&mock),
            channel_session().await,
            input,
            Some(resumed_with_channel()),
        );
        assert_stale(&feed.next().await.unwrap(), None);
        assert!(matches!(
            feed.next().await.unwrap_err(),
            InvocationError::Io(_)
        ));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        let checkpoint = checkpoint_of(&mut feed).await;
        assert_eq!(checkpoint.pts, 4);
        assert!(checkpoint.channels.is_empty());
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1], tl::functions::updates::GetState {}.to_bytes());
        assert_eq!(requests[1], requests[2]);
    }

    fn empty_channel_difference(pts: i32) -> Vec<u8> {
        ChannelDifference::Empty(tl::types::updates::ChannelDifferenceEmpty {
            r#final: true,
            pts,
            timeout: None,
        })
        .to_bytes()
    }

    fn dialog(pts: Option<i32>) -> tl::enums::Dialog {
        tl::types::Dialog {
            pinned: false,
            unread_mark: false,
            view_forum_as_messages: false,
            peer: peer_channel(CHANNEL),
            top_message: 30,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            unread_count: 0,
            unread_mentions_count: 0,
            unread_reactions_count: 0,
            unread_poll_votes_count: 0,
            notify_settings: notify_settings(),
            pts,
            draft: None,
            folder_id: None,
            ttl_period: None,
        }
        .into()
    }

    fn too_long(pts: Option<i32>, messages: Vec<tl::enums::Message>) -> Vec<u8> {
        ChannelDifference::TooLong(tl::types::updates::ChannelDifferenceTooLong {
            r#final: true,
            timeout: None,
            dialog: dialog(pts),
            messages,
            chats: vec![],
            users: vec![],
        })
        .to_bytes()
    }

    async fn channel_feed(reply: Vec<u8>) -> Feed<Arc<Mock>> {
        let mock = Mock::with(vec![Ok(empty_account_difference()), Ok(reply)]);
        let (socket, input) = mpsc::channel(1);
        std::mem::forget(socket);
        Feed::new(
            mock,
            channel_session().await,
            input,
            Some(resumed_with_channel()),
        )
    }

    #[tokio::test]
    async fn channel_too_long_marks_history_stale_without_replaying_messages() {
        let supplied = vec![
            message(peer_channel(CHANNEL), 30, "newest").into(),
            message(peer_channel(CHANNEL), 29, "older").into(),
        ];
        let mut feed = channel_feed(too_long(Some(20), supplied)).await;
        assert_stale(&feed.next().await.unwrap(), Some(CHANNEL_CHAT));
        assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUpGap));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUpEnded
        ));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::Checkpoint(s) if s.0.channels.iter().any(|c| c.id == CHANNEL && c.pts == 20)
        ));
    }

    #[tokio::test]
    async fn channel_too_long_without_dialog_pts_is_rejected() {
        // grammers would panic on a dialog without a pts.
        let mut feed = channel_feed(too_long(None, vec![])).await;
        assert!(matches!(
            feed.next().await.unwrap_err(),
            InvocationError::Dropped(_)
        ));
    }

    /// An invoker that answers its first call, then never answers.
    struct StallAfter(std::sync::Mutex<Option<Vec<u8>>>);

    impl Invoke for StallAfter {
        fn invoke<R>(
            &self,
            _request: &R,
        ) -> impl std::future::Future<Output = Result<R::Return, InvocationError>> + Send
        where
            R: tl::RemoteCall + Send + Sync,
            R::Return: Send,
        {
            let reply = self.0.lock().unwrap().take();
            async move {
                match reply {
                    Some(bytes) => <R::Return as tl::Deserializable>::from_bytes(&bytes)
                        .map_err(|e| InvocationError::from(ReadError::from(e))),
                    None => std::future::pending().await,
                }
            }
        }
    }

    async fn stalled(first: Option<Vec<u8>>) -> InvocationError {
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            StallAfter(std::sync::Mutex::new(first)),
            channel_session().await,
            input,
            Some(resumed_with_channel()),
        );
        let start = tokio::time::Instant::now();
        let err = loop {
            match feed.next().await {
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        assert_eq!(start.elapsed(), FEED_RPC_DEADLINE);
        err
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_account_difference_retires_the_instance() {
        let err = stalled(None).await;
        assert_eq!(stream_error(kind(&err)), StreamError::Fatal);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_channel_difference_retires_the_instance() {
        let err = stalled(Some(empty_account_difference())).await;
        assert_eq!(stream_error(kind(&err)), StreamError::Fatal);
    }

    fn channel_new(id: i64, pts: i32) -> tl::enums::Update {
        tl::types::UpdateNewChannelMessage {
            message: raw_in(id),
            pts,
            pts_count: 1,
        }
        .into()
    }

    fn socket_batch(updates: Vec<tl::enums::Update>) -> grammers_session::updates::UpdatesLike {
        grammers_session::updates::UpdatesLike::Updates(
            tl::types::Updates {
                updates,
                users: vec![],
                chats: vec![],
                date: 1,
                seq: 0,
            }
            .into(),
        )
    }

    fn raw_in(channel: i64) -> tl::enums::Message {
        message(peer_channel(channel), 1, "x").into()
    }

    /// The newest checkpoint among the items up to the next `Checkpoint`.
    async fn checkpoint_of(feed: &mut Feed<Arc<Mock>>) -> UpdatesState {
        loop {
            if let StreamItem::Checkpoint(state) = feed.next().await.unwrap() {
                return state.0;
            }
        }
    }

    fn channel_slice(
        ids: std::ops::Range<i64>,
        pts: i32,
        extra: Vec<tl::enums::Update>,
    ) -> Vec<u8> {
        Difference::Slice(tl::types::updates::DifferenceSlice {
            new_messages: vec![],
            new_encrypted_messages: vec![],
            other_updates: ids.map(|id| channel_new(id, 1)).chain(extra).collect(),
            chats: vec![],
            users: vec![],
            intermediate_state: state(pts),
        })
        .to_bytes()
    }

    #[tokio::test]
    async fn account_slices_over_the_channel_budget_mark_all_history_stale() {
        let cap = crate::store::MAX_DIALOGS;
        let before = i64::try_from(cap - 100).unwrap();
        let ack = tl::types::UpdateMessageId {
            id: 9,
            random_id: 7,
        };
        let mock = Mock::with(vec![
            Ok(channel_slice(100..before + 100, 11, vec![])),
            Ok(channel_slice(
                before + 100..before + 300,
                12,
                vec![ack.into()],
            )),
            Ok(state(20).to_bytes()),
        ]);
        let (_socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(
            Arc::clone(&mock),
            session(),
            input,
            Some(resumed_with_channel()),
        );
        for _ in 0..before {
            assert!(matches!(feed.next().await.unwrap(), StreamItem::CatchUp(_)));
            assert!(feed.tracked_channels() <= cap);
        }
        assert_eq!(feed.tracked_channels(), cap - 99);
        let stale = feed.next().await.unwrap();
        assert!(matches!(
            &stale,
            StreamItem::HistoryStale(StaleHistory(None))
        ));
        assert!(matches!(
            feed.next().await.unwrap(),
            StreamItem::CatchUp(Update::MessageId {
                id: MsgId(9),
                random_id: 7
            })
        ));
        assert_eq!(feed.tracked_channels(), 0);
        assert_eq!(mock.requests().len(), 2, "invalidation precedes getState");
        let gap = feed.next().await.unwrap();
        assert!(matches!(gap, StreamItem::CatchUpGap));
        let end = feed.next().await.unwrap();
        assert!(matches!(end, StreamItem::CatchUpEnded));
        let checkpoint = feed.next().await.unwrap();
        assert!(
            matches!(&checkpoint, StreamItem::Checkpoint(s) if s.0.pts == 20 && s.0.channels.is_empty())
        );
        assert_eq!(mock.requests().len(), 3, "two slices, then a new baseline");

        #[cfg(feature = "fake")]
        crate::actor::assert_difference_purges_history(CHANNEL_CHAT, [stale, gap, end, checkpoint])
            .await;
    }

    #[tokio::test]
    async fn the_tracked_channel_set_and_every_checkpoint_stay_bounded() {
        let cap = i64::try_from(crate::store::MAX_DIALOGS).unwrap();
        let mock = Mock::with(vec![Ok(tl::enums::updates::State::State(
            tl::types::updates::State {
                pts: 1,
                qts: 0,
                date: 1,
                seq: 0,
                unread_count: 0,
            },
        )
        .to_bytes())]);
        let (socket, input) = mpsc::channel(1);
        let mut feed = Feed::new(mock, session(), input, None);
        // The baseline's own checkpoint comes first.
        checkpoint_of(&mut feed).await;
        // Far more distinct channels than the cap, in batches.
        for start in (1..=cap).step_by(100) {
            socket
                .send(socket_batch(
                    (start..start + 100).map(|id| channel_new(id, 1)).collect(),
                ))
                .await
                .unwrap();
            let state = checkpoint_of(&mut feed).await;
            assert!(state.channels.len() <= crate::store::MAX_DIALOGS);
            assert!(
                state.channels.capacity() <= crate::store::MAX_DIALOGS,
                "a checkpoint holds no spare capacity"
            );
            assert!(feed.tracked_channels() <= crate::store::MAX_DIALOGS);
        }
        socket
            .send(socket_batch(vec![channel_new(1, 2)]))
            .await
            .unwrap();
        let state = checkpoint_of(&mut feed).await;
        assert!(state.channels.iter().any(|c| c.id == 1 && c.pts == 2));
        assert!(state.channels.iter().any(|c| c.id == 2));
        socket
            .send(socket_batch(
                (cap + 1..cap + 11).map(|id| channel_new(id, 1)).collect(),
            ))
            .await
            .unwrap();
        let state = checkpoint_of(&mut feed).await;
        let tracked: Vec<_> = state.channels.iter().map(|c| c.id).collect();
        assert_eq!(tracked.len(), crate::store::MAX_DIALOGS);
        assert!(tracked.contains(&1), "the touched oldest entry is kept");
        assert!(!tracked.contains(&2), "the least recent one is evicted");
        assert!(state.channels.capacity() <= crate::store::MAX_DIALOGS);
        assert!(feed.tracked_channels() <= crate::store::MAX_DIALOGS);
    }
}
