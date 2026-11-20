//! The call layer over a scripted invoker: retry classes (paused time),
//! `random_id` stability, peer resolution and request shapes.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::sender::{ReadError, RpcError, SenderPool};
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerAuth, PeerInfo};
use grammers_client::tl::{self, Deserializable as _, Serializable as _};

use super::{dialog, message, peer_user, user, users};
use crate::backend::grammers::calls::{Calls, history_offsets};
use crate::backend::grammers::invoke::Invoke;
use crate::backend::grammers::{GrammersControl, connection_params};
use crate::backend::{BackendError, HistoryQuery, InstanceControl, RejectReason, SessionSnapshot};
use crate::model::{ChatId, MsgId, PeerKind, ThreadKey};
use crate::session::{AbyssSession, MAX_PINNED_PEERS};

pub(super) type Reply = Result<Vec<u8>, InvocationError>;

/// Answers each request with the next scripted reply and records the request
/// bytes. An exhausted script answers `Dropped`.
#[derive(Default)]
pub(super) struct Mock {
    script: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Mock {
    pub(super) fn with(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Mock {
            script: Mutex::new(replies.into()),
            requests: Mutex::default(),
        })
    }

    pub(super) fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Invoke for Arc<Mock> {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        self.requests.lock().unwrap().push(request.to_bytes());
        let reply = self.script.lock().unwrap().pop_front();
        async move {
            match reply {
                Some(Ok(bytes)) => R::Return::from_bytes(&bytes)
                    .map_err(|e| InvocationError::from(ReadError::from(e))),
                Some(Err(e)) => Err(e),
                None => Err(InvocationError::Dropped("script exhausted")),
            }
        }
    }
}

fn flood(seconds: u32) -> Reply {
    Err(InvocationError::Rpc(RpcError {
        code: 420,
        name: "FLOOD_WAIT".into(),
        value: Some(seconds),
        caused_by: None,
    }))
}

pub(super) fn rpc(code: i32, name: &str) -> Reply {
    Err(InvocationError::Rpc(RpcError {
        code,
        name: name.into(),
        value: None,
        caused_by: None,
    }))
}

fn io() -> Reply {
    Err(InvocationError::Io(std::io::Error::other("reset")))
}

fn history_reply(ids: &[i32]) -> Vec<u8> {
    tl::enums::messages::Messages::Messages(tl::types::messages::Messages {
        messages: ids
            .iter()
            .map(|&id| tl::enums::Message::Message(message(peer_user(7), id, "m")))
            .collect(),
        topics: vec![],
        chats: vec![],
        users: vec![],
    })
    .to_bytes()
}

fn short_sent(id: i32) -> Vec<u8> {
    tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
        out: true,
        id,
        pts: 1,
        pts_count: 1,
        date: 0,
        media: None,
        entities: None,
        ttl_period: None,
    })
    .to_bytes()
}

pub(super) const ANN: ChatId = ChatId {
    kind: PeerKind::User,
    id: 7,
};
pub(super) const GROUP: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 10,
};

fn session() -> Arc<AbyssSession> {
    Arc::new(AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap())
}

/// A session that knows Ann's and the group's access hashes.
pub(super) async fn setup(mock: &Arc<Mock>) -> Calls<Arc<Mock>> {
    let session = session();
    session
        .cache_peer(PeerInfo::User {
            id: 7,
            auth: Some(PeerAuth::from_hash(49)),
            bot: Some(false),
            is_self: Some(false),
        })
        .await
        .unwrap();
    session
        .cache_peer(PeerInfo::Channel {
            id: 10,
            auth: Some(PeerAuth::from_hash(110)),
            kind: None,
        })
        .await
        .unwrap();
    Calls::new(Arc::clone(mock), session)
}

#[tokio::test(start_paused = true)]
async fn notify_settings_retries_transient_errors_and_short_flood_waits() {
    use crate::backend::{NotifyScope, NotifyTarget};

    for target in [
        NotifyTarget::Scope(NotifyScope::Groups),
        NotifyTarget::Peer(ANN),
    ] {
        let reply = tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
            mute_until: Some(120),
            ..super::notify_settings_raw()
        });
        let mock = Mock::with(vec![io(), flood(3), Ok(reply.to_bytes())]);
        let calls = setup(&mock).await;
        let start = tokio::time::Instant::now();
        assert_eq!(calls.notify_settings(target).await.unwrap(), Some(120));
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }
}

#[tokio::test(start_paused = true)]
async fn read_class_sleeps_short_floods_at_most_twice() {
    let mock = Mock::with(vec![flood(3), flood(3), Ok(history_reply(&[1, 2]))]);
    let calls = setup(&mock).await;
    let start = tokio::time::Instant::now();
    let page = calls
        .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(start.elapsed(), Duration::from_secs(6));

    let mock = Mock::with(vec![flood(2), flood(2), flood(2), Ok(history_reply(&[1]))]);
    let calls = setup(&mock).await;
    let err = calls
        .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 2 });
    assert_eq!(mock.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn read_class_does_not_sleep_long_floods() {
    let mock = Mock::with(vec![flood(6), Ok(history_reply(&[1]))]);
    let calls = setup(&mock).await;
    let start = tokio::time::Instant::now();
    let err = calls
        .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 6 });
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn read_class_retries_io_once() {
    let mock = Mock::with(vec![io(), Ok(history_reply(&[1]))]);
    assert!(
        setup(&mock)
            .await
            .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
            .await
            .is_ok()
    );
    assert_eq!(mock.requests().len(), 2);

    let mock = Mock::with(vec![io(), io(), Ok(history_reply(&[1]))]);
    let err = setup(&mock)
        .await
        .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::Transient);
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn sends_are_never_retried_by_the_adapter_and_keep_their_random_id() {
    let mock = Mock::with(vec![io(), flood(1), Ok(short_sent(99))]);
    let calls = setup(&mock).await;
    let err = calls
        .send_text(ThreadKey::whole(ANN), "hi".into(), 42)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BackendError::Transient,
        "unknown outcome: core retries"
    );
    assert_eq!(mock.requests().len(), 1);
    let err = calls
        .send_text(ThreadKey::whole(ANN), "hi".into(), 42)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 1 });
    assert_eq!(mock.requests().len(), 2);
    // Core's retry of the same logical send.
    assert_eq!(
        calls
            .send_text(ThreadKey::whole(ANN), "hi".into(), 42)
            .await,
        Ok(MsgId(99))
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| *r == requests[0]), "byte-identical");
    let sent = tl::functions::messages::SendMessage::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(sent.random_id, 42);
    assert!(sent.no_webpage);
    assert_eq!(sent.message, "hi");
    assert!(matches!(
        sent.peer,
        tl::enums::InputPeer::User(p) if p.user_id == 7 && p.access_hash == 49
    ));
}

#[tokio::test]
async fn accepted_sends_without_an_id_are_unconfirmed() {
    let no_mapping = Ok(tl::enums::Updates::Updates(tl::types::Updates {
        updates: vec![],
        users: vec![],
        chats: vec![],
        date: 0,
        seq: 0,
    })
    .to_bytes());
    let mock = Mock::with(vec![no_mapping, rpc(400, "RANDOM_ID_DUPLICATE")]);
    let calls = setup(&mock).await;
    for _ in 0..2 {
        let err = calls
            .send_text(ThreadKey::whole(ANN), "hi".into(), 7)
            .await
            .unwrap_err();
        assert_eq!(err, BackendError::SentUnconfirmed);
    }
}

#[tokio::test]
async fn unknown_peers_are_rejected_without_a_request() {
    let mock = Mock::with(vec![]);
    let calls = setup(&mock).await;
    let stranger = ChatId {
        kind: PeerKind::User,
        id: 8,
    };
    assert_eq!(
        calls
            .send_text(ThreadKey::whole(stranger), "x".into(), 1)
            .await,
        Err(BackendError::Rejected(RejectReason::Other))
    );
    assert_eq!(
        calls
            .history(ThreadKey::whole(stranger), HistoryQuery::Tail, 1)
            .await
            .unwrap_err(),
        BackendError::Rejected(RejectReason::Other)
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn read_history_sends_the_displayed_max_id() {
    let affected = Ok(tl::enums::messages::AffectedMessages::Messages(
        tl::types::messages::AffectedMessages {
            pts: 1,
            pts_count: 1,
        },
    )
    .to_bytes());
    let mock = Mock::with(vec![affected, Ok(true.to_bytes())]);
    let calls = setup(&mock).await;
    calls
        .read_history(ThreadKey::whole(ANN), MsgId(41))
        .await
        .unwrap();
    calls
        .read_history(ThreadKey::whole(GROUP), MsgId(42))
        .await
        .unwrap();
    let requests = mock.requests();
    let user = tl::functions::messages::ReadHistory::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(user.max_id, 41);
    let channel = tl::functions::channels::ReadHistory::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!(channel.max_id, 42);
    assert!(matches!(
        channel.channel,
        tl::enums::InputChannel::Channel(c) if c.channel_id == 10 && c.access_hash == 110
    ));
}

#[test]
fn history_offsets_per_query() {
    let offsets =
        |query| history_offsets(query, 50).map(|o| (o.offset_id, o.add_offset, o.limit, o.min_id));
    assert_eq!(offsets(HistoryQuery::Tail), Ok((0, 0, 50, 0)));
    assert_eq!(offsets(HistoryQuery::Before(MsgId(10))), Ok((10, 0, 50, 0)));
    assert_eq!(
        offsets(HistoryQuery::After(MsgId(10))),
        Ok((10, -50, 50, 10))
    );
}

#[tokio::test]
async fn dialogs_teach_the_session_hashes_for_later_calls() {
    let dialogs = tl::enums::messages::Dialogs::Dialogs(tl::types::messages::Dialogs {
        dialogs: vec![tl::enums::Dialog::Dialog(dialog(peer_user(8), 1))],
        messages: vec![tl::enums::Message::Message(message(peer_user(8), 1, "x"))],
        chats: vec![],
        users: users(vec![user(8, "Bob", "")]),
    });
    let mock = Mock::with(vec![Ok(dialogs.to_bytes()), Ok(history_reply(&[]))]);
    let calls = setup(&mock).await;
    let page = calls.dialogs(None, 20).await.unwrap();
    assert_eq!(page.chats.len(), 1);
    assert_eq!(page.next, None);
    let bob = ChatId {
        kind: PeerKind::User,
        id: 8,
    };
    calls
        .history(ThreadKey::whole(bob), HistoryQuery::Tail, 20)
        .await
        .unwrap();
    let requests = mock.requests();
    let first = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!((first.limit, first.offset_id), (20, 0));
    let history = tl::functions::messages::GetHistory::from_boxed_bytes(&requests[1]).unwrap();
    assert!(matches!(
        history.peer,
        tl::enums::InputPeer::User(p) if p.user_id == 8 && p.access_hash == 56
    ));
}

fn uid(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

/// A full dialogs slice listing users `ids` (top message id = user id).
fn dialogs_slice(ids: &[i64]) -> Vec<u8> {
    dialogs_slice_with(ids, std::iter::empty())
}

/// [`dialogs_slice`] whose users also list `extra` (auxiliary peers), after
/// the dialogs' own users.
fn dialogs_slice_with(ids: &[i64], extra: impl IntoIterator<Item = i64>) -> Vec<u8> {
    let listed = ids.iter().copied().chain(extra);
    dialogs_reply(ids, listed.map(|id| user(id, "U", "")).collect())
}

/// A full dialogs slice listing users `ids`, with `users` as its users.
fn dialogs_reply(ids: &[i64], users_list: Vec<tl::types::User>) -> Vec<u8> {
    let top = |id: i64| i32::try_from(id).unwrap();
    tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 1_000,
        dialogs: ids
            .iter()
            .map(|&id| tl::enums::Dialog::Dialog(dialog(peer_user(id), top(id))))
            .collect(),
        messages: ids
            .iter()
            .map(|&id| tl::enums::Message::Message(message(peer_user(id), top(id), "m")))
            .collect(),
        chats: vec![],
        users: users(users_list),
    })
    .to_bytes()
}

fn retire(session: &Arc<AbyssSession>) -> SessionSnapshot {
    let pool = SenderPool::with_configuration(Arc::clone(session), 1, connection_params());
    GrammersControl {
        handle: pool.handle.thin,
        session: Arc::clone(session),
    }
    .retire()
}

/// The peer a rebuilt instance addresses `chat` with, if it can.
async fn rebuilt_peer(snapshot: SessionSnapshot, chat: ChatId) -> Option<i64> {
    let session = Arc::new(AbyssSession::from_snapshot(snapshot).unwrap());
    let mock = Mock::with(vec![Ok(history_reply(&[]))]);
    let calls = Calls::new(Arc::clone(&mock), session);
    calls
        .history(ThreadKey::whole(chat), HistoryQuery::Tail, 5)
        .await
        .ok()?;
    let request = mock.requests().pop()?;
    match tl::functions::messages::GetHistory::from_boxed_bytes(&request)
        .unwrap()
        .peer
    {
        tl::enums::InputPeer::User(p) if p.user_id == chat.id => Some(p.access_hash),
        _ => None,
    }
}

#[tokio::test]
async fn a_refreshed_first_page_keeps_later_pages_and_open_chats_across_a_rebuild() {
    let mock = Mock::with(vec![
        Ok(dialogs_slice(&[21, 22])),
        Ok(dialogs_slice(&[23, 24])),
        Ok(history_reply(&[])),
        // The refresh: page one again, with a newcomer on top.
        Ok(dialogs_slice(&[25, 21])),
    ]);
    let calls = setup(&mock).await;
    let first = calls.dialogs(None, 2).await.unwrap();
    let second = calls.dialogs(first.next, 2).await.unwrap();
    assert_eq!(second.chats.len(), 2);
    // Open a chat from page two, then refresh page one.
    calls
        .history(ThreadKey::whole(uid(24)), HistoryQuery::Tail, 5)
        .await
        .unwrap();
    calls.dialogs(None, 2).await.unwrap();

    // Core's required set: the open chat, then its merged list.
    calls.retain(&[uid(24), uid(25), uid(21), uid(22), uid(23)]);

    let session = Arc::clone(calls.session());
    let snapshot = retire(&session);
    for (id, hash) in [(24, 24 * 7), (23, 23 * 7), (22, 22 * 7), (25, 25 * 7)] {
        assert_eq!(
            rebuilt_peer(snapshot.clone(), uid(id)).await,
            Some(hash),
            "chat {id} lost its access hash"
        );
    }
}

/// The user ids a snapshot carries pinned.
fn carried_users(snapshot: &SessionSnapshot) -> HashSet<i64> {
    snapshot
        .carried
        .iter()
        .filter_map(|info| match info {
            PeerInfo::User { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn uids(ids: impl IntoIterator<Item = i64>) -> Vec<ChatId> {
    ids.into_iter().map(uid).collect()
}

/// Calls over a session that knows users `ids` (hash = 7 × id), unpinned.
async fn knowing(ids: impl IntoIterator<Item = i64>, replies: Vec<Reply>) -> Calls<Arc<Mock>> {
    Calls::new(Mock::with(replies), knowing_session(ids).await)
}

async fn knowing_session(ids: impl IntoIterator<Item = i64>) -> Arc<AbyssSession> {
    let session = session();
    for id in ids {
        session
            .cache_peer(PeerInfo::User {
                id,
                auth: Some(PeerAuth::from_hash(id * 7)),
                bot: Some(false),
                is_self: Some(false),
            })
            .await
            .unwrap();
    }
    session
}

/// An invoker whose requests never complete.
struct Hang;

impl Invoke for Hang {
    fn invoke<R>(
        &self,
        _request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        std::future::pending()
    }
}

#[tokio::test]
async fn the_pinned_set_never_exceeds_the_session_cap() {
    let calls = knowing(1..=2000, vec![Ok(history_reply(&[]))]).await;
    calls.retain(&uids(1..=2000));
    let (selected, unknown) = calls.pin_state();
    assert_eq!((selected.len(), unknown), (MAX_PINNED_PEERS, 0));
    assert_eq!(
        carried_users(&retire(calls.session())).len(),
        MAX_PINNED_PEERS
    );
}

#[tokio::test(start_paused = true)]
async fn in_flight_chats_stay_pinned_until_the_call_ends() {
    let session = knowing_session([7, 8, 9]).await;
    let calls = Calls::new(Hang, Arc::clone(&session));
    calls.retain(&[uid(7)]);
    let mut call = Box::pin(calls.history(ThreadKey::whole(uid(9)), HistoryQuery::Tail, 5));
    let waited = tokio::time::timeout(Duration::from_secs(1), call.as_mut()).await;
    assert!(waited.is_err(), "the call is still in flight");
    // Core has not listed 9 yet; a rebuild now must still carry it.
    assert_eq!(carried_users(&session.snapshot()), HashSet::from([7, 9]));
    drop(call);
    // Cancelled: the extra pin is released; core's set remains.
    assert_eq!(carried_users(&session.snapshot()), HashSet::from([7]));
}

#[tokio::test]
async fn listed_peers_without_a_hash_are_counted_then_pinned_once_learned() {
    let calls = knowing([7], vec![Ok(dialogs_slice(&[8]))]).await;
    calls.retain(&[uid(7), uid(8)]);
    assert_eq!(calls.pin_state().1, 1, "8 is unknown");
    assert_eq!(
        carried_users(&calls.session().snapshot()),
        HashSet::from([7])
    );
    calls.dialogs(None, 20).await.unwrap();
    assert_eq!(calls.pin_state().1, 0);
    assert_eq!(
        carried_users(&calls.session().snapshot()),
        HashSet::from([7, 8])
    );
}

#[tokio::test]
async fn later_dialog_pages_exclude_pinned_chats() {
    let mut pinned = dialog(peer_user(21), 21);
    pinned.pinned = true;
    let page_one = tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 1_000,
        dialogs: vec![
            tl::enums::Dialog::Dialog(pinned),
            tl::enums::Dialog::Dialog(dialog(peer_user(22), 22)),
        ],
        messages: vec![
            tl::enums::Message::Message(message(peer_user(21), 21, "p")),
            tl::enums::Message::Message(message(peer_user(22), 22, "q")),
        ],
        chats: vec![],
        users: users(vec![user(21, "P", ""), user(22, "Q", "")]),
    });
    let mock = Mock::with(vec![Ok(page_one.to_bytes()), Ok(dialogs_slice(&[23, 24]))]);
    let calls = setup(&mock).await;
    let first = calls.dialogs(None, 2).await.unwrap();
    assert!(first.chats[0].pinned);
    let next = first.next.expect("a full page continues");
    let second = calls.dialogs(Some(next), 2).await.unwrap();
    assert!(second.chats.iter().all(|c| !c.pinned && c.id != uid(21)));

    let requests = mock.requests();
    let one = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[0]).unwrap();
    assert!(
        !one.exclude_pinned,
        "the first page carries the pinned chats"
    );
    let two = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[1]).unwrap();
    assert!(two.exclude_pinned, "later pages must not repeat them");
    assert_eq!((two.offset_id, two.offset_date), (22, 1_700_000_022));
    assert!(matches!(
        two.offset_peer,
        tl::enums::InputPeer::User(p) if p.user_id == 22 && p.access_hash == 22 * 7
    ));
}

#[tokio::test]
async fn oversized_responses_are_backend_errors() {
    let many: Vec<i32> = (1..=200).collect();
    let mock = Mock::with(vec![Ok(history_reply(&many))]);
    let calls = setup(&mock).await;
    assert_eq!(
        calls
            .history(ThreadKey::whole(ANN), HistoryQuery::Tail, 10)
            .await
            .unwrap_err(),
        BackendError::Rejected(RejectReason::Other)
    );
}

#[tokio::test]
async fn retire_revokes_before_snapshotting() {
    let session = session();
    let pool = SenderPool::with_configuration(Arc::clone(&session), 1, connection_params());
    let control = GrammersControl {
        handle: pool.handle.thin.clone(),
        session: Arc::clone(&session),
    };
    session.set_home_dc_id(5).await.unwrap();
    let snapshot = control.retire();
    let rev = snapshot.rev().unwrap();
    assert_eq!(rev, 1);
    // A late write by the retired instance (e.g. its runner) is dropped.
    session.set_home_dc_id(4).await.unwrap();
    assert_eq!(snapshot.rev().unwrap(), rev);
    assert_eq!(session.home_dc_id().unwrap(), 5);
}

/// Process-wide TRACE subscriber whose output goes to the calling thread's
/// capture buffer, if any. A thread-local default subscriber is unreliable
/// here: callsites registered by other test threads cache their interest
/// against *their* (absent) default, so this thread could miss events. A
/// global dispatcher makes every callsite interested; the writer then keeps
/// only this thread's output.
pub(super) fn capture_logs(buf: &LogBuf) -> CaptureGuard {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(|| ThreadCapture)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    CAPTURE.with(|c| *c.borrow_mut() = Some(buf.clone()));
    CaptureGuard
}

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<LogBuf>> = const { std::cell::RefCell::new(None) };
}

/// Stops capturing on drop.
pub(super) struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        CAPTURE.with(|c| *c.borrow_mut() = None);
    }
}

/// Writes to the current thread's capture buffer, or discards.
struct ThreadCapture;

impl std::io::Write for ThreadCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|c| {
            if let Some(capture) = c.borrow().as_ref() {
                capture.0.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Collects formatted log output.
#[derive(Clone, Default)]
pub(super) struct LogBuf(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn logs_never_carry_text_phones_or_payloads() {
    let buf = LogBuf::default();
    let _capture = capture_logs(&buf);

    let mut bob = user(8, "Secret Name", "");
    bob.phone = Some("+447700900123".into());
    let dialogs = tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 5,
        dialogs: vec![tl::enums::Dialog::Dialog(dialog(peer_user(8), 1))],
        messages: vec![tl::enums::Message::Message(message(
            peer_user(8),
            1,
            "secret body",
        ))],
        chats: vec![],
        users: users(vec![bob]),
    });
    let mock = Mock::with(vec![
        Err(InvocationError::Io(std::io::Error::other(
            "secret io detail",
        ))),
        Ok(dialogs.to_bytes()),
        rpc(400, "+447700900123 secret"),
        io(),
    ]);
    let calls = setup(&mock).await;
    calls.dialogs(None, 1).await.unwrap();
    let _ = calls
        .send_text(ThreadKey::whole(ANN), "secret outgoing".into(), 1)
        .await;
    let _ = calls
        .send_text(ThreadKey::whole(ANN), "secret outgoing".into(), 1)
        .await;

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("telegram call failed"),
        "events are logged: {out}"
    );
    for needle in ["secret", "Secret", "+44", "7700900123"] {
        assert!(!out.contains(needle), "{needle:?} leaked into: {out}");
    }
}

/// A history page whose users list `ids` (auxiliary peers only).
fn history_naming(ids: impl IntoIterator<Item = i64>) -> Vec<u8> {
    tl::enums::messages::Messages::Messages(tl::types::messages::Messages {
        messages: vec![],
        topics: vec![],
        chats: vec![],
        users: users(ids.into_iter().map(|id| user(id, "U", "")).collect()),
    })
    .to_bytes()
}

/// Two pages, then a 4,096-peer history response, all before core
/// reconciles: both pages (and the cursor peer) are still addressable.
#[tokio::test]
async fn responses_before_reconciliation_do_not_evict_handed_off_pages() {
    let calls = knowing(
        [7],
        vec![
            Ok(dialogs_slice(&(1..=100).collect::<Vec<_>>())),
            Ok(dialogs_slice(&(101..=200).collect::<Vec<_>>())),
            Ok(history_naming(20_000..20_000 + 4_096)),
        ],
    )
    .await;
    let first = calls.dialogs(None, 100).await.unwrap();
    let second = calls.dialogs(first.next, 100).await.unwrap();
    assert_eq!(second.next.and_then(|c| c.offset_peer), Some(uid(200)));
    calls
        .history(ThreadKey::whole(uid(7)), HistoryQuery::Tail, 5)
        .await
        .unwrap();
    calls.retain(&uids(1..=200));
    assert_eq!(calls.pin_state().1, 0);
    assert_eq!(carried_users(&retire(calls.session())).len(), 200);
}

/// A `messages.dialogFilters` reply holding regular folders with these ids.
fn folders_reply(ids: &[i32]) -> Vec<u8> {
    let filter = |id: i32| {
        tl::enums::DialogFilter::Filter(tl::types::DialogFilter {
            contacts: false,
            non_contacts: false,
            groups: false,
            broadcasts: false,
            bots: false,
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            title_noanimate: false,
            id,
            title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
                text: format!("f{id}"),
                entities: vec![],
            }),
            emoticon: None,
            color: None,
            pinned_peers: vec![],
            include_peers: vec![],
            exclude_peers: vec![],
        })
    };
    tl::enums::messages::DialogFilters::Filters(tl::types::messages::DialogFilters {
        tags_enabled: false,
        filters: ids.iter().map(|&id| filter(id)).collect(),
    })
    .to_bytes()
}

/// An edit on an instance that has not fetched the folders yet (a rebuild
/// keeps queued edits) fetches them first: a new folder takes a free id
/// instead of overwriting folder 2.
#[tokio::test(start_paused = true)]
async fn folder_edits_start_from_freshly_fetched_folders() {
    use crate::model::FolderEdit;
    let mock = Mock::with(vec![Ok(folders_reply(&[2, 3])), Ok(true.to_bytes())]);
    let calls = setup(&mock).await;
    let create = FolderEdit::Create {
        title: "New".into(),
        chat: ANN,
    };
    calls.edit_chat_folder(create).await.unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    tl::functions::messages::GetDialogFilters::from_boxed_bytes(&requests[0]).unwrap();
    let update =
        tl::functions::messages::UpdateDialogFilter::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!(update.id, 4, "folders 2 and 3 exist");
}
