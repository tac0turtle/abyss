//! Spawned work: backend calls (admitted, with deadlines), cache reads and the
//! update consumer.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;

use crate::api::{ClientGen, HistoryDir};
use crate::backend::{
    AROUND_CONTEXT, BackendError, Client, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery,
    MAX_HISTORY_PAGE, NotifyScope, NotifyTarget, StreamError, StreamItem, TopicsById, TopicsCursor,
    TopicsPage, UpdateStream,
};
use crate::cache::CacheHandle;
use crate::model::{
    ChatFolder, ChatId, ChatSummary, FolderEdit, LocalId, Message, MsgId, PeerFolder, ThreadKey,
    TopicId,
};
use crate::store::{BadgeSeq, ContinuityMark, FetchSeq, PageKind, TOPICS_PAGE, TopicsGen, ViewGen};

/// Messages per history page (server and cache), `Around` pages included.
pub(super) const PAGE: u32 = 50;
// An `Around` page of `PAGE` messages is within its bounds (Stage 2 plan
// section 17.6.4), so the backend never refuses one core asks for.
const _: () = assert!(PAGE > 0 && PAGE <= MAX_HISTORY_PAGE && AROUND_CONTEXT < PAGE);
/// Dialogs per page.
pub(super) const DIALOGS_PAGE: u32 = 100;
/// Waits between retries of a failing update stream (plan section 2): after the
/// last one fails too, the instance is rebuilt.
pub(super) const STREAM_RETRY: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Actor-assigned identity of a dialogs or history request. History snapshots
/// also track their thread, independent of the view they fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReqId(pub(super) u64);

/// One history load (cache read and/or server call): its thread, the view
/// it fills, and its identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Load {
    pub(super) thread: ThreadKey,
    pub(super) view: ViewGen,
    pub(super) req: ReqId,
}

/// Identity of one topics fetch (page or by-id): the list it fills, by
/// forum and generation, and its dispatch seq on that list's clock. A
/// completion applies only while both still match (plan section 6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TopicsCall {
    pub(super) chat: ChatId,
    pub(super) view: TopicsGen,
    pub(super) seq: FetchSeq,
}

/// A dialog snapshot's dispatch stamp (dialogs pages and `peer_dialogs`)
/// on the badge clock, taken at admission; default until then. Dialog row
/// fields and badge dirty stamps are ordered by it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Stamps {
    pub(super) badge: BadgeSeq,
}

/// A backend call waiting for admission.
#[derive(Debug)]
pub(super) enum Job {
    NotifySettings(NotifyScope),
    PeerNotifySettings {
        chat: ChatId,
        stamps: Stamps,
    },
    Dialogs {
        req: ReqId,
        cursor: Option<DialogsCursor>,
        /// Taken at admission (`Badges::dispatch`).
        stamps: Stamps,
    },
    /// One `peer_dialogs` call: kind checks and badge refreshes (plan
    /// sections 6.6, 7). At most `MAX_PEER_DIALOGS` chats.
    PeerDialogs {
        chats: Vec<ChatId>,
        /// The badge stamp is the scheduler's; the folder stamp is taken at
        /// admission.
        stamps: Stamps,
    },
    History {
        load: Load,
        dir: HistoryDir,
        query: HistoryQuery,
        /// For tails: the newest id in the window or awaiting a notice for
        /// this thread when the request was made. The server's tail covers
        /// that id even if it was deleted and the returned top is lower.
        floor: Option<MsgId>,
        /// The continuity clock when the load was requested: a tail may
        /// restore cache continuity only if its window lost none since.
        since: ContinuityMark,
    },
    Send {
        local_id: LocalId,
        thread: ThreadKey,
        text: String,
        random_id: i64,
        /// A flood-waited send's automatic-retry window: past it, the send
        /// is not made (checked right before the call; review round 19).
        expires: Option<tokio::time::Instant>,
    },
    Read {
        thread: ThreadKey,
        max_id: MsgId,
        /// Earlier failed attempts of this read intent.
        retry: u8,
    },
    TopicsPage {
        call: TopicsCall,
        kind: PageKind,
        /// `None` for a first page.
        cursor: Option<TopicsCursor>,
    },
    /// At most `MAX_TOPICS_BY_ID` ids.
    TopicsById {
        call: TopicsCall,
        ids: Vec<TopicId>,
    },
    Folders(FolderJob),
}

/// Archive and chat folder calls (folders plan, parts B and C).
#[derive(Debug)]
pub(super) enum FolderJob {
    /// Load the chat folders.
    Load,
    /// Archive or unarchive a chat.
    SetPeerFolder {
        write: u64,
        chat: ChatId,
        folder: PeerFolder,
    },
    /// Change a chat folder.
    Edit { write: u64, edit: FolderEdit },
}

impl FolderJob {
    /// The id of a folder write (`folder_writes`), `None` for a load.
    pub(super) fn write(&self) -> Option<u64> {
        match self {
            Self::Load => None,
            Self::SetPeerFolder { write, .. } | Self::Edit { write, .. } => Some(*write),
        }
    }
}

impl Job {
    /// The chat whose fence holds this job: a history load or a send
    /// (plan section 6.6).
    pub(super) fn fenced_chat(&self) -> Option<ChatId> {
        match self {
            Job::History { load, .. } => Some(load.thread.chat),
            Job::Send { thread, .. } => Some(thread.chat),
            _ => None,
        }
    }

    /// A topics call, which waits out a topics flood wait.
    pub(super) fn is_topics(&self) -> bool {
        matches!(self, Job::TopicsPage { .. } | Job::TopicsById { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallError {
    Backend(BackendError),
    /// The deadline passed; the outcome of a write is unknown.
    Timeout,
    /// The instance was retired while the call ran; the outcome is unknown.
    Dropped,
}

pub(super) type CallResult<T> = Result<T, CallError>;

/// Whether a dialogs page starts the list (replacing it) or continues it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DialogsStep {
    First,
    Next,
}

/// Why the update consumer stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamEnd {
    /// The stream failed for good: rebuild.
    Failed(StreamError),
    /// The instance is retiring or the actor is gone.
    Cancelled,
}

/// What a finished task reports back to the actor.
#[derive(Debug)]
pub(super) enum Outcome {
    NotifySettings {
        target: NotifyTarget,
        stamps: Stamps,
        res: CallResult<Option<i64>>,
    },
    Dialogs {
        req: ReqId,
        step: DialogsStep,
        stamps: Stamps,
        res: CallResult<DialogsPage>,
    },
    PeerDialogs {
        chats: Vec<ChatId>,
        stamps: Stamps,
        res: CallResult<Vec<ChatSummary>>,
    },
    History {
        load: Load,
        dir: HistoryDir,
        query: HistoryQuery,
        floor: Option<MsgId>,
        since: ContinuityMark,
        res: CallResult<HistoryPage>,
    },
    Send {
        local_id: LocalId,
        res: CallResult<MsgId>,
    },
    /// An automatic send reached its call after its window closed: it was
    /// not made.
    SendExpired {
        local_id: LocalId,
    },
    /// A read mark of `thread` up to `max_id` ended.
    Read {
        thread: ThreadKey,
        max_id: MsgId,
        retry: u8,
        res: CallResult<()>,
    },
    /// A login call ended: no store change, only the error for stall
    /// detection.
    Login(CallResult<()>),
    TopicsPage {
        call: TopicsCall,
        kind: PageKind,
        res: CallResult<TopicsPage>,
    },
    TopicsById {
        call: TopicsCall,
        res: CallResult<TopicsById>,
    },
    ChatFolders(CallResult<Vec<ChatFolder>>),
    /// A folder write ended. `moved`: the archive move it made, if it was
    /// one (an edit, or a write retirement dropped, has none).
    FolderWrite {
        write: u64,
        moved: Option<(ChatId, PeerFolder)>,
        res: CallResult<()>,
    },
    CacheChats(Option<Vec<ChatSummary>>),
    CacheHistory {
        load: Load,
        dir: HistoryDir,
        anchor: Option<MsgId>,
        messages: Option<Vec<Message>>,
    },
    UpdatesEnded(StreamEnd),
}

#[derive(Debug)]
pub(super) struct Done {
    pub(super) cgen: ClientGen,
    /// Present when the call timed out: the permit must be parked.
    pub(super) parked: Option<OwnedSemaphorePermit>,
    pub(super) outcome: Outcome,
}

/// What kind of task a `JoinSet` id belongs to, so a panic can clean up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskKind {
    NotifySettings(NotifyTarget),
    Dialogs(ReqId),
    /// A history load and its direction: a panic ends only that load
    /// (task 10e), never one deferred behind it.
    History(Load, HistoryDir),
    Send(LocalId),
    Read,
    TopicsPage(TopicsCall),
    TopicsById(TopicsCall),
    /// A `peer_dialogs` call, by its badge stamp.
    PeerDialogs(BadgeSeq),
    ChatFolders,
    /// An archive move or a folder change, by write id.
    FolderWrite(u64),
    CacheChats,
    Updates,
    /// A login call (task 6b), for panic handling.
    Auth(super::auth::AuthTag),
}

/// A spawned task reporting back to the actor.
pub(super) type DoneFuture = Pin<Box<dyn Future<Output = Done> + Send>>;

/// What every admitted call carries: its generation, deadline, permit, and the
/// instance's cancellation token.
struct Admitted {
    cgen: ClientGen,
    deadline: Duration,
    permit: OwnedSemaphorePermit,
    cancel: CancellationToken,
}

impl Admitted {
    /// Reports `outcome` without a call; the permit is released.
    fn skip(self, outcome: Outcome) -> Done {
        Done {
            cgen: self.cgen,
            parked: None,
            outcome,
        }
    }

    /// Runs `call` under the deadline and reports `outcome(result)`. On time-out
    /// the permit is handed back for parking; otherwise it is released here.
    async fn run<T>(
        self,
        call: impl Future<Output = Result<T, BackendError>>,
        outcome: impl FnOnce(CallResult<T>) -> Outcome,
    ) -> Done {
        let res = tokio::select! {
            biased;
            () = self.cancel.cancelled() => None,
            res = tokio::time::timeout(self.deadline, call) => Some(res),
        };
        let (res, parked) = match res {
            Some(Ok(res)) => (res.map_err(CallError::Backend), None),
            Some(Err(_)) => (Err(CallError::Timeout), Some(self.permit)),
            // Admission is closed while an instance retires, so releasing this
            // permit cannot over-admit the retiring instance.
            None => (Err(CallError::Dropped), None),
        };
        Done {
            cgen: self.cgen,
            parked,
            outcome: outcome(res),
        }
    }
}

/// Everything `job_future` needs besides the job.
pub(super) struct CallCtx<C> {
    pub(super) client: Arc<C>,
    pub(super) cgen: ClientGen,
    pub(super) deadline: Duration,
    pub(super) cancel: CancellationToken,
}

/// Builds the future for an admitted job, plus its kind.
pub(super) fn job_future<C: Client>(
    ctx: CallCtx<C>,
    job: Job,
    permit: OwnedSemaphorePermit,
) -> (TaskKind, DoneFuture) {
    let CallCtx {
        client,
        cgen,
        deadline,
        cancel,
    } = ctx;
    let admitted = Admitted {
        cgen,
        deadline,
        permit,
        cancel,
    };
    match job {
        Job::NotifySettings(scope) => {
            let target = NotifyTarget::Scope(scope);
            notify_settings(client, admitted, target, Stamps::default())
        }
        Job::PeerNotifySettings { chat, stamps } => {
            notify_settings(client, admitted, NotifyTarget::Peer(chat), stamps)
        }
        Job::Dialogs {
            req,
            cursor,
            stamps,
        } => dialogs(client, admitted, req, cursor, stamps),
        Job::PeerDialogs { chats, stamps } => peer_dialogs(client, admitted, chats, stamps),
        Job::History {
            load,
            dir,
            query,
            floor,
            since,
        } => history(client, admitted, load, dir, query, floor, since),
        Job::Send {
            local_id,
            thread,
            text,
            random_id,
            expires,
        } => {
            let call = async move { client.send_text(thread, text, random_id).await };
            let fut = async move {
                // Admission includes the window: a queued (or preserved)
                // automatic send never reaches the server after it closed.
                if expires.is_some_and(|at| tokio::time::Instant::now() >= at) {
                    return admitted.skip(Outcome::SendExpired { local_id });
                }
                admitted
                    .run(call, move |res| Outcome::Send { local_id, res })
                    .await
            };
            (TaskKind::Send(local_id), Box::pin(fut))
        }
        Job::Read {
            thread,
            max_id,
            retry,
        } => read(client, admitted, thread, max_id, retry),
        Job::TopicsPage { call, kind, cursor } => {
            let topics = async move { client.topics(call.chat, cursor, TOPICS_PAGE).await };
            let fut = admitted.run(topics, move |res| Outcome::TopicsPage { call, kind, res });
            (TaskKind::TopicsPage(call), Box::pin(fut))
        }
        Job::TopicsById { call, ids } => topics_by_id(client, admitted, call, ids),
        Job::Folders(job) => folder_future(client, admitted, job),
    }
}

/// The future for an admitted [`FolderJob`], plus its kind.
fn folder_future<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    job: FolderJob,
) -> (TaskKind, DoneFuture) {
    match job {
        FolderJob::Load => {
            let call = async move { client.chat_folders().await };
            let fut = admitted.run(call, Outcome::ChatFolders);
            (TaskKind::ChatFolders, Box::pin(fut))
        }
        FolderJob::SetPeerFolder {
            write,
            chat,
            folder,
        } => {
            let call = async move { client.set_peer_folder(chat, folder).await };
            let fut = admitted.run(call, move |res| Outcome::FolderWrite {
                write,
                moved: Some((chat, folder)),
                res,
            });
            (TaskKind::FolderWrite(write), Box::pin(fut))
        }
        FolderJob::Edit { write, edit } => {
            let call = async move { client.edit_chat_folder(edit).await };
            let fut = admitted.run(call, move |res| Outcome::FolderWrite {
                write,
                moved: None,
                res,
            });
            (TaskKind::FolderWrite(write), Box::pin(fut))
        }
    }
}

fn history<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    load: Load,
    dir: HistoryDir,
    query: HistoryQuery,
    floor: Option<MsgId>,
    since: ContinuityMark,
) -> (TaskKind, DoneFuture) {
    let call = async move { client.history(load.thread, query, PAGE).await };
    let outcome = move |res| Outcome::History {
        load,
        dir,
        query,
        floor,
        since,
        res,
    };
    (
        TaskKind::History(load, dir),
        Box::pin(admitted.run(call, outcome)),
    )
}

/// The future for an admitted read mark, plus its kind.
fn read<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    thread: ThreadKey,
    max_id: MsgId,
    retry: u8,
) -> (TaskKind, DoneFuture) {
    let call = async move { client.read_history(thread, max_id).await };
    let fut = admitted.run(call, move |res| Outcome::Read {
        thread,
        max_id,
        retry,
        res,
    });
    (TaskKind::Read, Box::pin(fut))
}

fn topics_by_id<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    call: TopicsCall,
    ids: Vec<TopicId>,
) -> (TaskKind, DoneFuture) {
    let topics = async move { client.topics_by_id(call.chat, &ids).await };
    let fut = admitted.run(topics, move |res| Outcome::TopicsById { call, res });
    (TaskKind::TopicsById(call), Box::pin(fut))
}

fn peer_dialogs<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    chats: Vec<ChatId>,
    stamps: Stamps,
) -> (TaskKind, DoneFuture) {
    // At most `MAX_PEER_DIALOGS` ids: the call borrows its own copy, and
    // the outcome carries the list back.
    let asked = chats.clone();
    let call = async move { client.peer_dialogs(&asked).await };
    let fut = admitted.run(call, move |res| Outcome::PeerDialogs { chats, stamps, res });
    (TaskKind::PeerDialogs(stamps.badge), Box::pin(fut))
}

fn notify_settings<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    target: NotifyTarget,
    stamps: Stamps,
) -> (TaskKind, DoneFuture) {
    let call = async move { client.notify_settings(target).await };
    let fut = admitted.run(call, move |res| Outcome::NotifySettings {
        target,
        stamps,
        res,
    });
    (TaskKind::NotifySettings(target), Box::pin(fut))
}

fn dialogs<C: Client>(
    client: Arc<C>,
    admitted: Admitted,
    req: ReqId,
    cursor: Option<DialogsCursor>,
    stamps: Stamps,
) -> (TaskKind, DoneFuture) {
    let step = if cursor.is_none() {
        DialogsStep::First
    } else {
        DialogsStep::Next
    };
    let call = async move { client.dialogs(cursor, DIALOGS_PAGE).await };
    let fut = admitted.run(call, move |res| Outcome::Dialogs {
        req,
        step,
        stamps,
        res,
    });
    (TaskKind::Dialogs(req), Box::pin(fut))
}

/// Reads a page from the cache. `anchor` is the window edge for older/newer reads.
pub(super) async fn cache_history(
    cache: CacheHandle,
    cgen: ClientGen,
    load: Load,
    dir: HistoryDir,
    anchor: Option<MsgId>,
) -> Done {
    let thread = load.thread;
    let res = match (dir, anchor) {
        (HistoryDir::Older, Some(id)) => cache.before(thread, id, PAGE).await,
        (HistoryDir::Newer, Some(id)) => cache.after(thread, id, PAGE).await,
        _ => cache.tail(thread, PAGE).await,
    };
    Done {
        cgen,
        parked: None,
        outcome: Outcome::CacheHistory {
            load,
            dir,
            anchor,
            messages: res.ok(),
        },
    }
}

/// Forwards stream items to the actor. A transient error is retried on the same
/// stream after each [`STREAM_RETRY`] wait; one more failure ends the consumer,
/// and so does a fatal error. Stops promptly when `cancel` fires.
pub(super) async fn consume_updates<U: UpdateStream>(
    mut stream: U,
    cgen: ClientGen,
    tx: mpsc::Sender<(ClientGen, StreamItem)>,
    cancel: CancellationToken,
) -> Done {
    let ended = |end| Done {
        cgen,
        parked: None,
        outcome: Outcome::UpdatesEnded(end),
    };
    let mut failures = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return ended(StreamEnd::Cancelled),
            next = stream.next() => next,
        };
        match next {
            Ok(item) => {
                failures = 0;
                let sent = tokio::select! {
                    biased;
                    () = cancel.cancelled() => false,
                    res = tx.send((cgen, item)) => res.is_ok(),
                };
                if !sent {
                    return ended(StreamEnd::Cancelled);
                }
            }
            Err(StreamError::Transient) if failures < STREAM_RETRY.len() => {
                let wait = STREAM_RETRY.get(failures).copied().unwrap_or_default();
                failures += 1;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return ended(StreamEnd::Cancelled),
                    () = tokio::time::sleep(wait) => {}
                }
            }
            Err(e) => return ended(StreamEnd::Failed(e)),
        }
    }
}
