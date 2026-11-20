//! The chat shell's commands to core. Each goes through an [`Admit`] first,
//! and the UI changes that follow from it (an echo, a cleared composer, a
//! pending load, a read mark) are made only once core admitted it. A full
//! queue therefore changes nothing: the next attempt asks again, after a
//! bounded backoff (`retry.rs`) that the shell wakes itself for.

use std::time::Instant;

use abyss_core::api::{Command, HistoryDir, UserError, WindowState};
use abyss_core::model::{ChatId, LocalId, MsgId, ThreadKey, TopicId};
use gpui::FollowMode;

use super::first_unread::{self, OpenRead, Opening};
use super::inline::Inline;
use super::messages::MessageWindow;
use super::retry::AUTO_RESTART_GAP;
use super::sends::Echo;
use super::topics::{Intent, TopicList};
use super::viewport::Seen;
use super::{ChatShell, Composer, CoreState, Covered, Pane, Stale};

/// Core did not take a command: its queue is full, or it is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Busy,
    Closed,
}

/// Queues a command with core, without waiting.
pub(crate) type Admit<'a> = &'a mut dyn FnMut(Command) -> Result<(), Refused>;

impl ChatShell {
    /// Opens `chat` from the chat list, once core admitted the command
    /// (the Stage 1 `Admit` rule): a forum shows its topic list
    /// (`OpenForum`), any other chat its messages (`OpenThread`, which
    /// closes the previous thread and any forum view in core). Refused,
    /// the pane stays as it was. Selecting a forum whose topic is open
    /// goes back to its list. Selecting a forum expands its sidebar row
    /// (plan section 17.3, decision D7).
    pub(crate) fn select(&mut self, chat: ChatId, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.list.get(chat).is_some_and(super::is_forum) {
            self.show_thread(ThreadKey::whole(chat), admit)?;
            return Ok(());
        }
        match self.pane {
            Pane::Forum {
                chat: c,
                open: None,
            } if c == chat => {}
            Pane::Forum { chat: c, .. } if c == chat => self.back_to_topics(admit)?,
            _ => self.show_forum(chat, admit)?,
        }
        self.set_inline(Inline::Expanded);
        Ok(())
    }

    /// Opens `thread`, a chat that isn't a forum: at its first unread
    /// message when it has some (`OpenThreadAt`, plan section 17.5).
    fn show_thread(&mut self, thread: ThreadKey, admit: Admit<'_>) -> Result<(), Refused> {
        if self.pane == Pane::Thread(thread) {
            return Ok(());
        }
        let after = self.list.get(thread.chat).and_then(first_unread::of_chat);
        // Core stopped (task 6l): local only, so the thread's stranded
        // echoes and draft stay reachable; nothing is asked of core.
        if self.core == CoreState::Running {
            admit(first_unread::open_command(thread, after))?;
        }
        self.topics = None;
        self.set_pane(Pane::Thread(thread));
        self.show_window(Some(thread));
        self.capture(after);
        Ok(())
    }

    /// Shows `chat`'s topic list. The previous thread is closed after the
    /// forum opened: refused, core merely keeps a window nobody shows (its
    /// live tail is cleared by the next frame), so the switch stands.
    fn show_forum(&mut self, chat: ChatId, admit: Admit<'_>) -> Result<(), Refused> {
        let opening = if self.core == CoreState::Running {
            admit(Intent::First.command(chat))?;
            if let Some(prev) = self.selected()
                && let Err(refused) = admit(Command::CloseThread(prev))
            {
                tracing::debug!(?refused, "the previous thread stays open in core");
            }
            Some(Intent::First)
        } else {
            None
        };
        self.topics = Some(TopicList::new(chat, opening));
        self.set_pane(Pane::Forum { chat, open: None });
        self.show_window(None);
        Ok(())
    }

    /// Opens a notification's forum and topic with one queue slot. Both
    /// core views and the pane stay as they are if admission is refused.
    pub(crate) fn show_forum_topic(
        &mut self,
        chat: ChatId,
        topic: TopicId,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if matches!(self.pane, Pane::Forum { chat: c, .. } if c == chat) {
            return self.open_topic(topic, admit);
        }
        let thread = ThreadKey {
            chat,
            topic: Some(topic),
        };
        let running = self.core == CoreState::Running;
        if running {
            admit(Command::OpenForumTopic { chat, topic })?;
        }
        self.topics = Some(TopicList::new(chat, running.then_some(Intent::First)));
        self.set_pane(Pane::Forum {
            chat,
            open: Some(topic),
        });
        self.set_inline(Inline::Expanded);
        self.show_window(Some(thread));
        self.capture(None);
        Ok(())
    }

    /// Opens `topic` of the shown forum (`OpenThread` with the topic, or
    /// `OpenThreadAt` when its row shows an unread mark: plan section
    /// 17.5). Reading it can remove its sidebar row without closing the pane.
    pub(crate) fn open_topic(&mut self, topic: TopicId, admit: Admit<'_>) -> Result<(), Refused> {
        let Pane::Forum { chat, open } = self.pane else {
            return Ok(());
        };
        if open == Some(topic) {
            return Ok(());
        }
        let thread = ThreadKey {
            chat,
            topic: Some(topic),
        };
        let shown = self.topics.as_ref().and_then(|l| l.get(topic));
        let after = shown.and_then(first_unread::of_topic);
        if self.core == CoreState::Running {
            admit(first_unread::open_command(thread, after))?;
        }
        self.set_pane(Pane::Forum {
            chat,
            open: Some(topic),
        });
        self.show_window(Some(thread));
        self.capture(after);
        Ok(())
    }

    /// Back one level (the "‹ Topics" button and Cmd+[): from a topic to
    /// its forum's list, which shows at once from the copy kept here; from
    /// a topic list to no chat (`CloseForum`, which drops a waiting load).
    pub(crate) fn back(&mut self, admit: Admit<'_>) -> Result<(), Refused> {
        match self.pane {
            Pane::Forum { open: Some(_), .. } => self.back_to_topics(admit),
            Pane::Forum { chat, open: None } => {
                if self.core == CoreState::Running {
                    admit(Command::CloseForum(chat))?;
                }
                self.topics = None;
                self.set_pane(Pane::Empty);
                Ok(())
            }
            Pane::Empty | Pane::Thread(_) => Ok(()),
        }
    }

    /// Brings the pane in line with core ([`Self::stale_pane`]): a chat
    /// whose kind changed is shown the other way (`OpenForum`, or
    /// `OpenThread` of the whole chat), a removed open topic goes back to
    /// the list.
    /// Refused, it is tried again after a bounded backoff
    /// ([`Self::fix_due`]).
    pub(crate) fn fix_pane(&mut self, now: Instant, admit: Admit<'_>) -> Result<(), Refused> {
        let Some(chat) = self.pane.chat() else {
            return Ok(());
        };
        let res = match self.stale_pane() {
            None => Ok(()),
            Some(Stale::Kind { to: None }) => self.show_forum(chat, admit),
            Some(Stale::Kind { to: Some(thread) }) => self.show_thread(thread, admit),
            Some(Stale::TopicRemoved(_)) => self.back_to_topics(admit),
        };
        if res.is_err() {
            self.fix_retry.failed(now);
        }
        res
    }

    /// How the pane must change at `now`, unless a refused change waits
    /// out its backoff (or gave up until a fresh trigger).
    pub(crate) fn fix_due(&self, now: Instant) -> Option<Stale> {
        self.stale_pane().filter(|_| self.fix_retry.ready(now))
    }

    /// The pane becomes `pane` (its topic list already set). The kind core
    /// announced for the previous chat (`KindChanged`) and the expansion of
    /// its sidebar row are forgotten with it.
    fn set_pane(&mut self, pane: Pane) {
        if pane.chat() != self.pane.chat() {
            self.shown_kind = None;
            self.inline = Inline::Expanded;
        }
        if pane.thread() != self.pane.thread() {
            self.announcer.clear();
        }
        self.pane = pane;
        self.sync_inline();
    }

    fn back_to_topics(&mut self, admit: Admit<'_>) -> Result<(), Refused> {
        let (Pane::Forum { chat, .. }, Some(thread)) = (self.pane, self.selected()) else {
            return Ok(());
        };
        if self.core == CoreState::Running {
            admit(Command::CloseThread(thread))?;
        }
        self.set_pane(Pane::Forum { chat, open: None });
        self.show_window(None);
        Ok(())
    }

    /// The message list starts afresh for `thread` (none: no messages
    /// shown).
    fn show_window(&mut self, thread: Option<ThreadKey>) {
        self.open_removed = None;
        // The pane changed: a refused reconciliation starts over.
        self.fix_retry.reset();
        self.window = thread.map(MessageWindow::new);
        self.revealed.clear();
        self.read_sent = None;
        // Leaving the thread drops its divider and read gate.
        self.first_unread = OpenRead::default();
        // The user's choice: frame commands for the new thread start afresh.
        self.frame_retry.reset();
        self.shown_echoes = self.echo_count();
        self.rows.reset(self.shown_echoes);
        self.rows.set_follow_mode(FollowMode::Tail);
    }

    /// The thread just shown opened at read position `after` (none: it had
    /// nothing unread). The divider marks it, and frames that show only
    /// what was read send no read mark.
    fn capture(&mut self, after: Option<MsgId>) {
        self.first_unread = OpenRead::opened(after);
        if after.is_some() {
            self.read_sent = after;
        }
    }

    /// Topic list row `last_visible` is on screen. Returns whether a
    /// request is due.
    pub(crate) fn topics_seen(&mut self, last_visible: usize, now: Instant) -> bool {
        let Some(list) = self.topics.as_mut() else {
            return false;
        };
        list.seen(last_visible);
        list.wants(now).is_some()
    }

    /// Asks for what the topic list wants at `now` (see
    /// [`TopicList::wants`]). A refusal is retried after a backoff.
    pub(crate) fn request_topics(&mut self, now: Instant, admit: Admit<'_>) -> Result<(), Refused> {
        if self.core == CoreState::Stopped {
            return Ok(());
        }
        let Some(list) = self.topics.as_mut() else {
            return Ok(());
        };
        let Some(intent) = list.wants(now) else {
            return Ok(());
        };
        match admit(intent.command(list.chat())) {
            Ok(()) => {
                list.requested(intent);
                Ok(())
            }
            Err(refused) => {
                list.refused(intent, now);
                Err(refused)
            }
        }
    }

    /// Where a composer submission would go: `Ok(None)` when nothing is open,
    /// the composer is blocked (a read-only chat, a closed topic) or the
    /// open thread can no longer be sent to (the pane waits to move), `Busy`
    /// when the send table is full.
    pub(crate) fn send_target(&self) -> Result<Option<ThreadKey>, UserError> {
        if self.core == CoreState::Stopped
            || self.composer() != Composer::Open
            || self.stale_pane().is_some()
        {
            return Ok(None);
        }
        let Some(thread) = self.selected() else {
            return Ok(None);
        };
        if self.echoes.is_full() {
            return Err(UserError::Busy);
        }
        Ok(Some(thread))
    }

    /// Sends `text` to `thread` (from [`Self::send_target`]). Only once core
    /// admitted the send does its echo appear; the caller clears the
    /// composer on `Ok(Some(_))`. `Ok(None)`: nothing to send.
    pub(crate) fn submit(
        &mut self,
        thread: ThreadKey,
        text: &str,
        now: i64,
        admit: Admit<'_>,
    ) -> Result<Option<LocalId>, Refused> {
        if text.trim().is_empty() || self.echoes.is_full() {
            return Ok(None);
        }
        // Fail closed rather than reuse an id core may still hold: at one
        // send per microsecond, a u64 lasts over 500,000 years.
        let Some(next) = self.next_local.checked_add(1) else {
            return Ok(None);
        };
        let local_id = LocalId(self.next_local);
        admit(Command::SendText {
            thread,
            text: text.to_owned(),
            local_id,
        })?;
        self.next_local = next;
        let pushed = self
            .echoes
            .push(Echo::new(local_id, thread, text.to_owned(), now));
        debug_assert!(pushed.is_ok(), "checked not full above");
        self.sync_echoes();
        self.rows.set_follow_mode(FollowMode::Tail);
        // A send into the open thread removes its divider.
        if self.selected() == Some(thread) {
            self.remove_divider();
        }
        Ok(Some(local_id))
    }

    /// Retries a failed or unknown send; it shows "Sending…" once core's
    /// queue took the retry. Core answers every retry (the send's state, or
    /// `SendRefused`), so the flip is always reconciled.
    pub(crate) fn retry(&mut self, local_id: LocalId, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.echoes.get(local_id).is_some_and(Echo::can_retry) {
            return Ok(());
        }
        admit(Command::RetrySend(local_id))?;
        self.echoes.retried(local_id);
        self.sync_echoes();
        Ok(())
    }

    /// Drops a failed, unknown or flood-waiting send. The echo shows
    /// "Discarding…" and keeps its slot until core answers (task 6j): it
    /// goes on `SendDiscarded`, and stays with core's state on
    /// `DiscardRefused` (an automatic resend may have gone in flight).
    pub(crate) fn discard(&mut self, local_id: LocalId, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.echoes.get(local_id).is_some_and(Echo::can_discard) {
            return Ok(());
        }
        admit(Command::DiscardSend(local_id))?;
        self.echoes.discarding(local_id);
        self.remeasure_echoes();
        Ok(())
    }

    /// Takes the text of a send core refused (`SendRefused`) or that core
    /// stopped before answering out of its echo, for the composer or to
    /// drop. `None`: no such echo.
    pub(crate) fn take_returned(&mut self, local_id: LocalId) -> Option<String> {
        let echo = self.echoes.take_returned(local_id)?;
        self.sync_echoes();
        Some(echo.text)
    }

    /// The thread and text of a returned echo, if `local_id` is one.
    pub(crate) fn returned(&self, local_id: LocalId) -> Option<(ThreadKey, &str)> {
        self.echoes
            .get(local_id)
            .filter(|e| e.is_returned())
            .map(|e| (e.thread, e.text.as_str()))
    }

    /// The text of a stranded echo (core stopped), for Copy.
    pub(crate) fn stranded_text(&self, local_id: LocalId) -> Option<&str> {
        self.echoes
            .get(local_id)
            .filter(|e| e.is_stranded())
            .map(|e| e.text.as_str())
    }

    /// Core's event stream closed while it ran (task 6k): every echo still
    /// waiting on an answer is stranded, its text kept.
    /// From then on nothing is asked of core: selecting a chat is local
    /// (task 6l) and nothing can be sent.
    pub(crate) fn core_stopped(&mut self) {
        self.core = CoreState::Stopped;
        if self.echoes.strand() {
            self.remeasure_echoes();
        }
    }

    /// Whether core stopped while it ran ([`Self::core_stopped`]).
    pub(crate) fn is_stopped(&self) -> bool {
        self.core == CoreState::Stopped
    }

    /// Stashes the composer's text as `thread`'s draft (an empty one clears
    /// it). Full: `Err` hands the text back; see [`Self::can_switch`].
    pub(crate) fn stash_draft(&mut self, thread: ThreadKey, text: String) -> Result<(), String> {
        self.drafts.stash(thread, text)
    }

    /// Drops the drafts of threads that can no longer be sent to, and
    /// returns them, oldest first.
    pub(crate) fn discard_drafts(
        &mut self,
        obsolete: impl Fn(ThreadKey) -> bool,
    ) -> Vec<(ThreadKey, String)> {
        self.drafts.discard(obsolete)
    }

    /// Whether the open thread can change to `to` (none: a pane without
    /// messages) without losing a draft: the box's `text` needs a slot
    /// unless it is empty or the slots free.
    pub(crate) fn can_switch(&self, to: Option<ThreadKey>, text: &str) -> bool {
        self.selected()
            .is_none_or(|from| self.drafts.can_switch(from, to.unwrap_or(from), text))
    }

    /// Takes `thread`'s stashed draft.
    pub(crate) fn take_draft(&mut self, thread: ThreadKey) -> Option<String> {
        self.drafts.take(thread)
    }

    /// Back to the live tail from far up the history: core reopens the
    /// thread at its tail.
    pub(crate) fn jump_to_latest(&mut self, admit: Admit<'_>) -> Result<(), Refused> {
        let Some(thread) = self.window.as_ref().map(MessageWindow::thread) else {
            return Ok(());
        };
        admit(Command::OpenThread(thread))?;
        self.rows.set_follow_mode(FollowMode::Tail);
        Ok(())
    }

    /// Chat list row `last_visible` is on screen (see
    /// [`super::list::ChatList::seen`]). Returns whether a request is due.
    pub(crate) fn dialogs_seen(&mut self, last_visible: usize, now: Instant) -> bool {
        self.list.seen(last_visible);
        self.list.wants_more(now)
    }

    /// Asks for the next dialogs page if the sidebar wants one and no retry
    /// waits. A refusal is retried after a backoff.
    pub(crate) fn request_dialogs(
        &mut self,
        now: Instant,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if !self.list.wants_more(now) || self.core == CoreState::Stopped {
            return Ok(());
        }
        match admit(Command::LoadDialogs) {
            Ok(()) => {
                self.list.requested();
                Ok(())
            }
            Err(refused) => {
                self.list.refused(now);
                Err(refused)
            }
        }
    }

    /// After a frame was painted: load more history near either end, mark
    /// read up to the newest message on screen while the window is active
    /// and no overlay covers the pane, and tell core whether the live tail
    /// is on screen. `seen` is `None`
    /// when no row was laid out. A refusal is retried on a later frame once
    /// its backoff allows ([`Self::retry_due`] makes sure one comes), and not
    /// at all after [`MAX_FAILURES`](super::retry::MAX_FAILURES) in a row
    /// until a fresh trigger ([`Self::fresh_trigger`]). Focus sync has its
    /// own backoff, so either can be retried without the other.
    pub(crate) fn frame_seen(
        &mut self,
        seen: Option<Seen>,
        now: Instant,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.core == CoreState::Stopped {
            // Nothing to ask of core, so no retry to schedule (task 6l).
            return Ok(());
        }
        let focus = if self.focus_out_of_sync() && self.focus_retry.ready(now) {
            self.sync_focus(now, admit)
        } else {
            Ok(())
        };
        // The withdrawal under a cover has its own backoff: it must not wait
        // for the frame retry, which may have given up (A76).
        let focus = focus.and(self.withdraw_tail(now, admit));
        if !self.frame_retry.ready(now) {
            // Skipped, not succeeded: the backoff stands.
            return focus;
        }
        let mut tried = false;
        let mut counting = |cmd: Command| {
            tried = true;
            admit(cmd)
        };
        let res = self.frame_commands(seen, now, &mut counting);
        match res {
            Err(_) => self.frame_retry.failed(now),
            Ok(()) if tried => self.frame_retry.reset(),
            Ok(()) => {}
        }
        focus.and(res)
    }

    fn frame_commands(
        &mut self,
        seen: Option<Seen>,
        now: Instant,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        // A covered pane shows nothing: neither read nor watched (A64).
        let active = self.active == WindowState::Active && self.covered == Covered::No;
        let Some(w) = self.window.as_mut() else {
            return self.frame_viewing(None, admit);
        };
        let thread = w.thread();
        if self.first_unread.opening == Opening::FallbackDue {
            admit(Command::OpenThread(thread))?;
            self.first_unread.opening = Opening::Unpositioned;
        }
        if let Some(seen) = seen {
            if w.wants_older(seen.first_row, now) {
                admit(Command::LoadOlder(thread))?;
                w.requested(HistoryDir::Older);
            }
            if let Some(last) = seen.last_message
                && w.wants_newer(last, now)
            {
                admit(Command::LoadNewer(thread))?;
                w.requested(HistoryDir::Newer);
            }
        }
        // Only a frame that painted the gate's view opens it (plan
        // section 17.5); the window's rows then show that view.
        let ready = seen.is_some() && self.first_unread.gate.painted(w.view());
        let shown = seen.and_then(|s| s.last_message).and_then(|ix| w.get(ix));
        if let Some(max_id) = shown
            .map(|m| m.id)
            .filter(|id| ready && active && self.read_sent.is_none_or(|r| *id > r))
        {
            admit(Command::MarkRead { thread, max_id })?;
            self.read_sent = Some(max_id);
        }
        let tail = tail_on_screen(w, seen);
        self.frame_viewing((active && tail).then_some(thread), admit)
    }

    /// The frame path's report of the live tail. Under a cover it says
    /// nothing: the withdrawal is [`Self::withdraw_tail`]'s, with its own
    /// backoff, so a refusal never counts against the frame retry (A76).
    fn frame_viewing(
        &mut self,
        thread: Option<ThreadKey>,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.covered == Covered::Yes {
            return Ok(());
        }
        self.set_viewing(thread, admit)
    }

    /// Whether a withdrawal of the watched tail is owed: a modal overlay
    /// covers the pane while core still has the tail as watched.
    fn withdraw_owed(&self) -> bool {
        self.covered == Covered::Yes && self.viewing.is_some() && self.core != CoreState::Stopped
    }

    /// Tells core the tail is no longer watched once a cover owes it.
    /// Independent of paging and read-mark backoff: a refusal waits its
    /// own backoff, which caps at [`MAX_DELAY`](super::retry::MAX_DELAY)
    /// and never gives up while the cover stays, because core counts
    /// incoming messages as viewed until it hears (A76). One non-blocking
    /// attempt per call; the retry wake-up supplies the calls.
    fn withdraw_tail(&mut self, now: Instant, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.withdraw_owed() || !self.withdraw_retry.ready(now) {
            return Ok(());
        }
        let res = self.set_viewing(None, admit);
        match res {
            Ok(()) => self.withdraw_retry.reset(),
            Err(_) => self.withdraw_retry.failed_persistent(now),
        }
        res
    }

    /// When the owed withdrawal is next due, for the retry wake-up.
    pub(super) fn withdraw_due(&self, now: Instant) -> Option<Instant> {
        if !self.withdraw_owed() {
            return None;
        }
        if self.withdraw_retry.ready(now) {
            Some(now)
        } else {
            self.withdraw_retry.due(now)
        }
    }

    /// Discards the sends core keeps for threads that can no longer be sent
    /// to: nobody sees them to Retry or Discard, and a flood-waiting one
    /// must not be resent. Their text reaches the discarded-drafts notice
    /// once core confirms (`take_lost_sends`). Run after every chat event,
    /// whatever the pane shows; a refusal waits out a backoff, and the
    /// retry wake-up tries again.
    pub(crate) fn discard_obsolete_sends(
        &mut self,
        now: Instant,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.core == CoreState::Stopped || !self.obsolete_retry.ready(now) {
            return Ok(());
        }
        for local_id in self.echoes.obsolete_to_discard() {
            if let Err(refused) = admit(Command::DiscardSend(local_id)) {
                self.obsolete_retry.failed(now);
                return Err(refused);
            }
            self.echoes.discarding(local_id);
        }
        self.obsolete_retry.reset();
        Ok(())
    }

    /// Tells core which thread's live tail is on screen, when that changed.
    fn set_viewing(&mut self, thread: Option<ThreadKey>, admit: Admit<'_>) -> Result<(), Refused> {
        if self.viewing != thread {
            admit(Command::ViewingTail(thread))?;
            self.viewing = thread;
        }
        Ok(())
    }

    /// The window gained or lost focus. The shell follows at once (no read
    /// mark and no live tail while inactive); core is told now, or, if it
    /// refuses, after a backoff ([`Self::retry_due`]): a backoff that still
    /// runs keeps its deadline and then sends the latest state. A toggle
    /// never resets the focus backoff either: its failures keep counting
    /// after a deadline passed, and once it gave up only a recovery restarts
    /// it (task 10f). Activation is an automatic recovery
    /// ([`Self::auto_recover`]), not the user's Retry: it never brings a
    /// waiting retry forward.
    pub(crate) fn set_focus(
        &mut self,
        state: WindowState,
        now: Instant,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.active != state {
            self.active = state;
            if state == WindowState::Active {
                self.auto_recover(now);
            }
        }
        if self.focus_out_of_sync() && self.focus_retry.ready(now) {
            self.sync_focus(now, admit)
        } else {
            Ok(())
        }
    }

    /// Automatic recovery (window activation): retries that gave up start
    /// over, at most once per [`AUTO_RESTART_GAP`]; retries that wait keep
    /// their deadline. Rapid activation therefore cannot multiply attempts.
    /// A recovery the gap holds back is not lost: it runs from the retry
    /// wake-up once the gap ends ([`Self::retry_due`], task 10f), so chat B
    /// still recovers after chat A used the allowance.
    fn auto_recover(&mut self, now: Instant) {
        let exhausted = self.focus_retry.gave_up()
            || self.frame_retry.gave_up()
            || self.obsolete_retry.gave_up()
            || self.fix_retry.gave_up()
            || self.list.exhausted()
            || self.topics.as_ref().is_some_and(TopicList::exhausted)
            || self.window.as_ref().is_some_and(MessageWindow::exhausted);
        if !exhausted {
            return;
        }
        if let Some(at) = self.auto_restart
            && now.saturating_duration_since(at) < AUTO_RESTART_GAP
        {
            self.held_recovery = at.checked_add(AUTO_RESTART_GAP);
            return;
        }
        self.auto_restart = Some(now);
        self.held_recovery = None;
        self.focus_retry.restart_if_exhausted();
        self.frame_retry.restart_if_exhausted();
        self.obsolete_retry.restart_if_exhausted();
        self.fix_retry.restart_if_exhausted();
        self.list.recover();
        if let Some(t) = self.topics.as_mut() {
            t.recover();
        }
        if let Some(w) = self.window.as_mut() {
            w.recover();
        }
    }

    /// A genuine transition (reconnection, the user's Retry): every
    /// automatic retry that waits or gave up may try again at once. Never
    /// called from rendering, nor on activation (see [`Self::set_focus`]).
    /// Reconnection is paced by core's own rebuild backoff.
    pub(crate) fn fresh_trigger(&mut self) {
        self.held_recovery = None;
        self.focus_retry.reset();
        self.frame_retry.reset();
        self.obsolete_retry.reset();
        self.fix_retry.reset();
        self.list.retry_now();
        if let Some(t) = self.topics.as_mut() {
            t.retry_now();
        }
        if let Some(w) = self.window.as_mut() {
            w.retry_now();
        }
    }

    /// Whether automatic retries of loads gave up, so the UI offers Retry:
    /// history of the open chat (or the frame commands that ask for it),
    /// and dialogs pages.
    pub(crate) fn history_gave_up(&self) -> bool {
        self.window.as_ref().is_some_and(MessageWindow::gave_up) || self.frame_retry.gave_up()
    }

    /// Whether automatic retries of the topic list's load gave up.
    pub(crate) fn topics_gave_up(&self) -> bool {
        self.topics.as_ref().is_some_and(TopicList::gave_up)
    }

    /// Which Retry pills the UI offers: (history, dialogs, topics). A
    /// change needs a redraw (`shell::chats`).
    pub(crate) fn retry_offered(&self) -> (bool, bool, bool) {
        (
            self.history_gave_up(),
            self.list.gave_up(),
            self.topics_gave_up(),
        )
    }

    /// The user pressed Retry.
    pub(crate) fn retry_loads(&mut self) {
        self.fresh_trigger();
    }

    /// The current retry wake-up fired ([`Self::retry_fired`]): runs a
    /// recovery the gap held back once it is due (while the window is still
    /// active), then resends what is due now, focus if its backoff allows,
    /// a dialogs page, the topic list's load (a flood wait's kept load
    /// once it ended) and discards of obsolete sends; the caller then redraws, so the frame check retries
    /// the rest.
    pub(crate) fn retry_wakeup(&mut self, now: Instant, admit: Admit<'_>) -> Result<(), Refused> {
        if self.held_recovery.is_some_and(|at| at <= now) {
            self.held_recovery = None;
            if self.active == WindowState::Active {
                self.auto_recover(now);
            }
        }
        let focus = if self.focus_out_of_sync() && self.focus_retry.ready(now) {
            self.sync_focus(now, admit)
        } else {
            Ok(())
        };
        let dialogs = self.request_dialogs(now, admit);
        let topics = self.request_topics(now, admit);
        let obsolete = self.discard_obsolete_sends(now, admit);
        let withdrawn = self.withdraw_tail(now, admit);
        focus.and(dialogs).and(topics).and(obsolete).and(withdrawn)
    }

    /// Whether core's idea of the window (its state, the live tail) lags
    /// the actual one.
    pub(crate) fn focus_out_of_sync(&self) -> bool {
        self.focus_sent != self.active
            || (self.active == WindowState::Inactive && self.viewing.is_some())
    }

    /// Tells core what it lacks of the actual window state: `Window`, and
    /// while inactive `ViewingTail(None)`. Both are tried, so either one
    /// admitted already stops core treating the tail as watched.
    pub(crate) fn sync_focus(&mut self, now: Instant, admit: Admit<'_>) -> Result<(), Refused> {
        let mut res = Ok(());
        if self.focus_sent != self.active {
            match admit(Command::Window(self.active)) {
                Ok(()) => self.focus_sent = self.active,
                Err(refused) => res = Err(refused),
            }
        }
        if self.active == WindowState::Inactive {
            res = res.and(self.set_viewing(None, admit));
        }
        match res {
            Ok(()) => self.focus_retry.reset(),
            Err(_) => self.focus_retry.failed(now),
        }
        res
    }

    /// The echo table, for the fake-backed tests.
    #[cfg(all(test, feature = "fake"))]
    pub(crate) fn echoes(&self) -> &super::sends::Echoes {
        &self.echoes
    }
}

/// Whether the open chat's newest message is on screen and the window is
/// attached to the live tail. An empty loaded chat shows its (empty) tail.
fn tail_on_screen(w: &MessageWindow, seen: Option<Seen>) -> bool {
    if !w.is_loaded() || w.has_newer() {
        return false;
    }
    match w.len().checked_sub(1) {
        None => true,
        Some(newest) => seen.and_then(|s| s.last_message) == Some(newest),
    }
}
