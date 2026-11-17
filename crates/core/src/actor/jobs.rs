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
    BackendError, Client, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, StreamError,
    StreamItem, UpdateStream,
};
use crate::cache::CacheHandle;
use crate::model::{ChatSummary, LocalId, Message, MsgId, ThreadKey};
use crate::store::ViewGen;

/// Messages per history page (server and cache).
pub(super) const PAGE: u32 = 50;
/// Dialogs per page.
pub(super) const DIALOGS_PAGE: u32 = 100;
/// Waits between retries of a failing update stream (plan section 2): after the
/// last one fails too, the instance is rebuilt.
pub(super) const STREAM_RETRY: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Identity of one history load, unique per actor. A bootstrap snapshot is
/// tracked by `(thread, ReqId)`, independent of the view it fills, so it always
/// completes even when its view went stale.
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

/// A backend call waiting for admission.
#[derive(Debug)]
pub(super) enum Job {
    Dialogs {
        cursor: Option<DialogsCursor>,
    },
    History {
        load: Load,
        dir: HistoryDir,
        query: HistoryQuery,
        /// For tails: the newest id the window held when the request was
        /// made. The server's answer is authoritative up to it; anything
        /// newer in the window arrived later (a live update).
        floor: Option<MsgId>,
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
    },
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
    Dialogs {
        step: DialogsStep,
        res: CallResult<DialogsPage>,
    },
    History {
        load: Load,
        dir: HistoryDir,
        query: HistoryQuery,
        floor: Option<MsgId>,
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
    Read(CallResult<()>),
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
    Dialogs,
    /// A history load and its direction: a panic ends only that load
    /// (task 10e), never one deferred behind it.
    History(Load, HistoryDir),
    Send(LocalId),
    Read,
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
        Job::Dialogs { cursor } => {
            let step = if cursor.is_none() {
                DialogsStep::First
            } else {
                DialogsStep::Next
            };
            let call = async move { client.dialogs(cursor, DIALOGS_PAGE).await };
            let fut = admitted.run(call, move |res| Outcome::Dialogs { step, res });
            (TaskKind::Dialogs, Box::pin(fut))
        }
        Job::History {
            load,
            dir,
            query,
            floor,
        } => {
            let call = async move { client.history(load.thread, query, PAGE).await };
            let fut = admitted.run(call, move |res| Outcome::History {
                load,
                dir,
                query,
                floor,
                res,
            });
            (TaskKind::History(load, dir), Box::pin(fut))
        }
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
        Job::Read { thread, max_id } => {
            let call = async move { client.read_history(thread, max_id).await };
            // Read marks are best effort; the next read covers a lost one.
            let fut = admitted.run(call, Outcome::Read);
            (TaskKind::Read, Box::pin(fut))
        }
    }
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
