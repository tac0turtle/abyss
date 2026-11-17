//! Folder writes (folders plan, part C): archive moves and chat folder
//! edits run one at a time per account.
//!
//! An edit fetches the folders and writes a whole folder back, so two edits
//! in flight together could each start from the same state and the later
//! write would undo the earlier one (or two creates could pick the same
//! id). Core therefore admits one write and keeps the rest waiting, in
//! order; the next starts when the previous one's outcome is known, or
//! known to be unknown (a timeout, a retired instance). Each write is built
//! from its own fetch in the backend, never from a shared copy.
//!
//! Uncertain writes are recovered best-effort. A write whose outcome is
//! unknown (it timed out, failed with a transport error, its task panicked,
//! or its completion was lost with a panicked retirement) may still sit in
//! the client's transport, or be applied by the server later: Telegram may
//! process requests in any order, and `messages.updateDialogFilter` has no
//! version check. Core mitigates this, without being able to rule it out:
//! after one, no write starts until the instance that carried it is
//! retired (core rebuilds at once, through the stall path `Reason::Stall`)
//! or the account is reset. Then it reloads the folders and admits the next
//! write, which fetches its own state.
//!
//! Progress after an uncertain write depends on that retirement completing,
//! as every other client operation does under the single rebuild
//! mechanism: nothing is sent while no client is live, and a retirement
//! that hangs (its aborted tasks are joined without a bound) holds folder
//! writes along with everything else.
//!
//! Residual risk: if the server applies the uncertain write after the next
//! write's fetch, that whole-folder write is built without it, and one of
//! the two changes is lost (folders plan, part C, "Risks").
//!
//! While a logout hides the account (`paused`), no write starts, new ones
//! are refused, and waiting ones are dropped (and the user is told).

use std::collections::VecDeque;

use super::Actor;
use super::apply::user_error;
use super::jobs::{CallError, CallResult, FolderJob, Job};
use super::lifecycle::Reason;
use crate::api::{ClientGen, Event, UserError};
use crate::backend::Backend;
use crate::model::{ChatId, FolderEdit, PeerFolder};

/// Folder writes waiting for the one in flight; more are refused `Busy`.
pub(super) const MAX_WAITING_WRITES: usize = 16;

/// The account's folder writes: at most one admitted (queued for a permit
/// or running), the rest waiting in order.
#[derive(Debug, Default)]
pub(super) struct FolderWrites {
    waiting: VecDeque<FolderJob>,
    /// The admitted write's id.
    active: Option<u64>,
    /// A write with an uncertain outcome went out on this client
    /// generation: no write starts until a later generation's retirement
    /// of it completed (or the account is reset).
    uncertain: Option<ClientGen>,
    next: u64,
}

impl FolderWrites {
    fn id(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.next
    }
}

/// What a folder write does.
pub(super) enum FolderWrite {
    Move { chat: ChatId, folder: PeerFolder },
    Edit(FolderEdit),
}

impl<B: Backend> Actor<B> {
    /// Takes a write from the UI: it waits its turn behind the one in
    /// flight.
    pub(super) async fn folder_write(&mut self, write: FolderWrite) {
        if self.folder_writes.waiting.len() >= MAX_WAITING_WRITES {
            return self.error(UserError::Busy).await;
        }
        let id = self.folder_writes.id();
        let job = match write {
            FolderWrite::Move { chat, folder } => FolderJob::SetPeerFolder {
                write: id,
                chat,
                folder,
            },
            FolderWrite::Edit(edit) => FolderJob::Edit { write: id, edit },
        };
        self.folder_writes.waiting.push_back(job);
        self.next_folder_write().await;
    }

    /// Admits the next waiting write, unless one is admitted already or a
    /// logout hides the account.
    pub(super) async fn next_folder_write(&mut self) {
        if self.folder_writes.active.is_some()
            || self.folder_writes.uncertain.is_some()
            || self.logout_gate.hides_account()
        {
            return;
        }
        while let Some(job) = self.folder_writes.waiting.pop_front() {
            let id = job.write();
            // A full queue refuses it (`Busy`); the next one tries.
            if self.enqueue(Job::Folders(job)).await {
                self.folder_writes.active = id;
                return;
            }
        }
    }

    /// A write ended (or was dropped with its instance). Only the admitted
    /// write's outcome counts: one of a previous account (before a logout)
    /// is ignored. On success an archive move is applied as a live
    /// `updateFolderPeers` would be, and an edit refetches the folders; an
    /// edit whose outcome is unknown refetches them too.
    pub(super) async fn on_folder_write(
        &mut self,
        write: u64,
        cgen: ClientGen,
        moved: Option<(ChatId, PeerFolder)>,
        res: CallResult<()>,
    ) {
        if self.folder_writes.active != Some(write) {
            return;
        }
        self.folder_writes.active = None;
        match (res, moved) {
            (Ok(()), Some(mv)) => self.on_folders(vec![mv]).await,
            (Ok(()), None) => self.load_folders(),
            // The request may still be in the transport, or applied by the
            // server: a timeout, or a transport failure after it was sent.
            (
                Err(
                    e @ (CallError::Timeout
                    | CallError::Backend(crate::backend::BackendError::Transient)),
                ),
                _,
            ) => {
                self.error(user_error(e)).await;
                return self.folder_write_uncertain(cgen).await;
            }
            // A refusal or failure the server answered, or a write its
            // retiring instance dropped (handed back by the retirement,
            // whose runner is gone by then).
            (Err(e), moved) => {
                self.error(user_error(e)).await;
                if e == CallError::Dropped && moved.is_none() {
                    self.load_folders();
                }
            }
        }
        self.next_folder_write().await;
    }

    /// The admitted write's task panicked: its request may have gone out.
    pub(super) async fn folder_write_panicked(&mut self, write: u64) {
        if self.folder_writes.active == Some(write) {
            self.folder_writes.active = None;
            self.error(UserError::Internal).await;
            self.folder_write_uncertain(self.cgen).await;
        }
    }

    /// A write sent on `cgen` may still land: hold the next one until that
    /// instance is retired, and retire it now if it is the live one.
    async fn folder_write_uncertain(&mut self, cgen: ClientGen) {
        let held = self.folder_writes.uncertain.get_or_insert(cgen);
        *held = (*held).max(cgen);
        if cgen == self.cgen {
            self.rebuild(Reason::Stall).await;
        }
    }

    /// A retirement completed: the client of every earlier generation is
    /// gone, so nothing more is sent on it. That does not prove the server
    /// will not still apply an uncertain write it received; the hold ends
    /// here anyway (best-effort), the folders are reloaded and the next
    /// write may start.
    pub(super) async fn folder_transport_retired(&mut self) {
        if self
            .folder_writes
            .uncertain
            .is_some_and(|held| held < self.cgen)
        {
            self.folder_writes.uncertain = None;
            self.load_folders();
            self.next_folder_write().await;
        }
    }

    /// A retirement panicked: completions it would have handed back are
    /// lost. An admitted write that had left the queue may have been sent,
    /// so it counts as uncertain (on the retired generation); the hold then
    /// ends with this retirement, as after any other.
    pub(super) async fn folder_writes_lost_with_retirement(&mut self) {
        let Some(active) = self.folder_writes.active else {
            return;
        };
        let queued = self
            .queue
            .iter()
            .any(|job| matches!(job, Job::Folders(f) if f.write() == Some(active)));
        if queued {
            return;
        }
        self.folder_writes.active = None;
        let retired = ClientGen(self.cgen.0.saturating_sub(1));
        self.folder_write_uncertain(retired).await;
        self.folder_transport_retired().await;
    }

    /// A logout hides the account: waiting writes, and the admitted one if
    /// it is still queued, are dropped. One already handed to the server
    /// ends as usual. Returns whether any was dropped (the user is told,
    /// `FolderWritesCancelled`).
    pub(super) fn drop_unstarted_folder_writes(&mut self) -> bool {
        let waiting = !self.folder_writes.waiting.is_empty();
        self.folder_writes.waiting.clear();
        let before = self.queue.len();
        self.queue
            .retain(|job| !matches!(job, Job::Folders(f) if f.write().is_some()));
        let queued = self.queue.len() != before;
        if queued {
            self.folder_writes.active = None;
        }
        waiting || queued
    }

    /// The old account's writes are gone with it (logout).
    pub(super) fn forget_folder_writes(&mut self) {
        self.folder_writes.waiting.clear();
        self.folder_writes.active = None;
        self.folder_writes.uncertain = None;
    }

    /// Emits that a write was refused while a logout hides the account.
    pub(super) async fn folder_write_refused(&self) {
        self.emit(Event::Error(UserError::Busy)).await;
    }
}
