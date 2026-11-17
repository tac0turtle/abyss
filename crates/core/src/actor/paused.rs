//! The account while a logout hides it (review rounds 25 and 28).
//!
//! From the Logout until the marker is durable ("Logging out"), and while a
//! marker that could not be written keeps "Couldn't start logout" up, the
//! account is unchanged but hidden (`LogoutGate::hides_account`):
//! - Nothing new is sent for it. New sends are kept `Failed` (Retry after
//!   "Stay signed in"), retries are answered with their send's state, send
//!   jobs still queued are taken back (`Sending`, not attempted), and
//!   automatic resends wait: the flood-wait timer is paused with its
//!   deadlines kept, and replays are skipped. A call already handed to the
//!   server cannot be taken back; it ends as usual.
//! - Its live tail counts as unseen: core forgets `ViewingTail`, and a new
//!   one is refused until the UI shows the chat again.
//! - Everything else acts as if it had come before the Logout.
//!
//! "Stay signed in" resumes what waited. A logout that starts tears it all
//! down with the account.

use super::Actor;
use super::jobs::Job;
use super::lifecycle::Phase;
use crate::api::{Command, Event, HistoryDir, LoadTarget, UserError};
use crate::backend::Backend;

impl<B: Backend> Actor<B> {
    /// The account was just hidden: its tail is unseen, and send jobs not
    /// yet handed to the server are taken back.
    pub(super) fn hide_account(&mut self) {
        self.viewing = None;
        let queue = std::mem::take(&mut self.queue);
        let (sends, rest): (Vec<Job>, Vec<Job>) = queue
            .into_iter()
            .partition(|job| matches!(job, Job::Send { .. }));
        self.queue = rest.into();
        for job in sends {
            if let Job::Send { local_id, .. } = job {
                self.sends.unqueue(local_id);
            }
        }
    }

    /// "Stay signed in": resumes the sends that waited (flood waits that
    /// ended, replays).
    pub(super) async fn show_account(&mut self) {
        self.resume_flooded_sends().await;
        if self.life.phase == Phase::Live {
            self.replay_sends().await;
        }
    }

    /// A command `LogoutGate::refuses` while the account is hidden: a new
    /// send is kept `Failed`, a retry is answered with its send's state, and
    /// a tail report is dropped (the UI reports again once it shows the
    /// thread).
    pub(super) async fn refused_behind_logout(&mut self, cmd: Command) {
        match cmd {
            Command::SendText {
                thread,
                text,
                local_id,
            } => self.hold_send(thread, text, local_id).await,
            other => self.acknowledge_refused(other).await,
        }
    }

    /// Answers an account command refused in the UI's epoch, so no UI state
    /// waits on it (review round 25, finding 1): a send ends `Failed`, a
    /// retry goes back to its send's state (`answer_unrun_retry`), a load
    /// ends `Busy`.
    pub(super) async fn acknowledge_refused(&mut self, cmd: Command) {
        match cmd {
            Command::SendText { local_id, .. } => {
                self.emit(Event::SendRefused { local_id }).await;
            }
            Command::RetrySend(local_id) => self.answer_unrun_retry(local_id).await,
            Command::DiscardSend(local_id) => self.answer_unrun_discard(local_id).await,
            Command::LoadDialogs => {
                self.load_ended(LoadTarget::Dialogs, Some(UserError::Busy))
                    .await;
            }
            Command::LoadOlder(thread) => {
                self.history_ended(thread, HistoryDir::Older, Some(UserError::Busy))
                    .await;
            }
            Command::LoadNewer(thread) => {
                self.history_ended(thread, HistoryDir::Newer, Some(UserError::Busy))
                    .await;
            }
            Command::OpenForum(chat) | Command::LoadTopics(chat) => {
                self.load_ended(LoadTarget::Topics(chat), Some(UserError::Busy))
                    .await;
            }
            // Nothing waits on these.
            _ => {}
        }
    }
}
