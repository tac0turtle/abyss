//! Chat list, message view and sending (plan sections 7–9): the state the UI
//! keeps and the decisions it makes (what to load, what to mark read, what a
//! row shows), apart from rendering so they can be tested without a window.
//! The shell (`shell::chats`) feeds it events and lends it an [`Admit`] to
//! queue commands with core (`commands.rs`); `shell::chat_view` renders it.

pub(crate) mod announce;
mod commands;
pub(crate) mod copy;
#[cfg(feature = "fake")]
pub(crate) mod demo;
pub(crate) mod discarded;
pub(crate) mod drafts;
pub(crate) mod first_unread;
pub(crate) mod format;
pub(crate) mod inline;
pub(crate) mod list;
pub(crate) mod messages;
pub(crate) mod retry;
pub(crate) mod rich;
pub(crate) mod sends;
pub(crate) mod sidebar_file;
pub(crate) mod topics;
pub(crate) mod viewport;

use std::collections::HashSet;
use std::time::Instant;

use abyss_core::api::{ConnState, Event, HistoryDir, LoadTarget, UserError, WindowState};
use abyss_core::model::{
    ChatId, ChatKind, ChatSummary, Message, MsgId, ThreadKey, TopicGone, TopicId,
};
use abyss_core::store::{ForumStatus, ViewGen};
use gpui::{FollowMode, ListAlignment, ListOffset, ListState, Pixels, SharedString, Task, px};

pub(crate) use self::commands::{Admit, Refused};
use self::discarded::Obsolete;
use self::first_unread::{Divider, OpenRead, Opening, ReadGate};
use self::inline::Inline;
use self::list::ChatList;
use self::messages::{Applied, Batch, MessageWindow, RowOp};
use self::retry::{Backoff, earliest};
use self::sends::{Echo, Echoes};
use self::topics::TopicList;
use self::viewport::Seen;

/// Rows rendered beyond the viewport, so scrolling doesn't pop in.
const OVERDRAW: f32 = 800.;
/// Messages whose spoilers the user revealed, per open chat.
const MAX_REVEALED: usize = 256;

/// One row of the message list: the window's messages, then the open thread's
/// echoes (own sends without a server copy yet).
#[derive(Debug)]
pub(crate) enum Row<'a> {
    Message(&'a Message),
    Echo(&'a Echo),
}

impl Row<'_> {
    pub(crate) fn date(&self) -> i64 {
        match self {
            Row::Message(m) => m.date,
            Row::Echo(e) => e.date,
        }
    }
}

/// What the chat pane shows (plan section 8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Pane {
    #[default]
    Empty,
    /// A chat that is not a forum.
    Thread(ThreadKey),
    /// A forum: its topic list, or one of its topics (`open`).
    Forum { chat: ChatId, open: Option<TopicId> },
}

/// Whether a modal overlay covers the message pane (`ChatShell::set_covered`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Covered {
    Yes,
    No,
}

/// What the open thread's composer area shows (plan section 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Composer {
    /// The message box.
    Open,
    /// The chat takes no messages from the user (Stage 1).
    ReadOnly,
    /// The open topic is closed, and the user may not post in closed
    /// topics.
    TopicClosed,
}

/// Why the pane must change on its own (`ChatShell::stale_pane`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stale {
    /// The chat became a forum, or stopped being one (its summary's kind,
    /// which core sends after `ForumInvalidated`): it is shown the other
    /// way, with `to` its thread afterwards.
    Kind { to: Option<ThreadKey> },
    /// The open topic was deleted or hidden (the reason): back to the
    /// topic list. The cap never cuts the open topic.
    TopicRemoved(TopicGone),
}

impl Pane {
    pub(crate) fn chat(self) -> Option<ChatId> {
        match self {
            Self::Empty => None,
            Self::Thread(t) => Some(t.chat),
            Self::Forum { chat, .. } => Some(chat),
        }
    }

    /// The thread the pane shows messages of: none for a topic list.
    pub(crate) fn thread(self) -> Option<ThreadKey> {
        match self {
            Self::Empty | Self::Forum { open: None, .. } => None,
            Self::Thread(t) => Some(t),
            Self::Forum {
                chat,
                open: Some(topic),
            } => Some(ThreadKey {
                chat,
                topic: Some(topic),
            }),
        }
    }
}

pub(crate) struct ChatShell {
    pub(crate) list: ChatList,
    pane: Pane,
    /// The forum's topic list while the pane shows a forum (`Pane::Forum`).
    topics: Option<TopicList>,
    /// Whether the pane's forum shows its topics under its sidebar row
    /// (plan section 17.3); it goes with the list when the pane leaves it.
    inline: Inline,
    /// Core removed the open topic (`TopicRemoved`): the pane goes back to
    /// the list ([`Self::stale_pane`]).
    open_removed: Option<(TopicId, TopicGone)>,
    /// The kind core last told for the pane's chat, by `KindChanged` or by
    /// a summary, whichever came last: a summary sent before the change
    /// may still be listed (task 11 review).
    shown_kind: Option<(ChatId, ForumStatus)>,
    /// Bringing the pane in line with core after a refusal.
    fix_retry: Backoff,
    window: Option<MessageWindow>,
    echoes: Echoes,
    /// The message list's GPUI state. Always `window.len() + shown_echoes` rows.
    rows: ListState,
    shown_echoes: usize,
    revealed: HashSet<MsgId>,
    /// Highest id core admitted as read for the open thread; it starts at
    /// the read position captured at open (plan section 17.5).
    read_sent: Option<MsgId>,
    /// The open thread's read gate, divider and first positioning.
    first_unread: OpenRead,
    /// The thread core was last told shows its live tail (`ViewingTail`).
    viewing: Option<ThreadKey>,
    /// Composer text of threads not open (task 6k): the box belongs to the
    /// open thread.
    drafts: drafts::Drafts,
    /// Whether core still runs; stopped, nothing is asked of it (task 6l).
    core: CoreState,
    next_local: u64,
    /// Whether the window is active, as the OS last said. Reads and the
    /// live tail follow this at once, whatever core was told.
    active: WindowState,
    /// A modal overlay covers the pane (review A64, N57): the session says
    /// so before each frame check ([`Self::set_covered`]). Nothing under it
    /// is seen, so no read mark and no live tail, exactly as if the window
    /// were inactive.
    covered: Covered,
    /// The window state core last admitted (`Command::Window`).
    focus_sent: WindowState,
    /// Bringing core in line with `active` after a refusal.
    focus_retry: Backoff,
    /// Discarding sends of threads that can no longer be sent to, after a
    /// refusal.
    obsolete_retry: Backoff,
    /// Withdrawing `ViewingTail` while a modal overlay covers the pane
    /// (review A76): its own backoff, apart from the frame retry (loads and
    /// read marks), which may have given up. It never gives up while the
    /// pane stays covered; a cover or an uncover starts it afresh.
    withdraw_retry: Backoff,
    /// Frame-driven commands (loads, read marks, the live tail) core
    /// refused: the next frame retries, and a wake-up makes sure one comes.
    frame_retry: Backoff,
    /// When window activation last restarted retries that gave up
    /// (at most once per [`retry::AUTO_RESTART_GAP`]).
    auto_restart: Option<Instant>,
    /// When a recovery the gap held back may run (the gap's end); the
    /// retry wake-up runs it then (task 10f).
    held_recovery: Option<Instant>,
    /// The one pending wake-up for retries, if one is scheduled.
    retry_timer: Option<RetryTimer>,
    /// Names wake-ups, so a stale one (replaced since) is ignored.
    timer_gen: u64,
    /// Core's connection, as last reported (a reconnection is a fresh
    /// trigger for retries).
    conn: Option<ConnState>,
    countdown: Countdown,
    /// Where the message rows were painted this frame.
    painted: viewport::Painted,
    /// What the live region says of messages arriving in the open thread.
    announcer: announce::Announcer,
}

/// The retry wake-up: when it is due, its name, and its task (dropping the
/// task cancels the wake-up).
struct RetryTimer {
    due: Instant,
    id: u64,
    task: Option<Task<()>>,
}

/// Whether core runs, as the event stream says (task 6l).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoreState {
    Running,
    /// Its event stream closed while it ran.
    Stopped,
}

/// Whether a flood-wait countdown ticker runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Countdown {
    Idle,
    Running,
}

impl Default for ChatShell {
    fn default() -> Self {
        let rows = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        rows.set_follow_mode(FollowMode::Tail);
        Self {
            list: ChatList::default(),
            pane: Pane::Empty,
            topics: None,
            inline: Inline::Expanded,
            open_removed: None,
            shown_kind: None,
            fix_retry: Backoff::default(),
            window: None,
            echoes: Echoes::default(),
            rows,
            shown_echoes: 0,
            revealed: HashSet::new(),
            read_sent: None,
            first_unread: OpenRead::default(),
            viewing: None,
            drafts: drafts::Drafts::default(),
            core: CoreState::Running,
            next_local: 1,
            active: WindowState::Active,
            covered: Covered::No,
            focus_sent: WindowState::Active,
            focus_retry: Backoff::default(),
            obsolete_retry: Backoff::default(),
            withdraw_retry: Backoff::default(),
            frame_retry: Backoff::default(),
            auto_restart: None,
            held_recovery: None,
            retry_timer: None,
            timer_gen: 0,
            conn: None,
            countdown: Countdown::Idle,
            painted: viewport::Painted::default(),
            announcer: announce::Announcer::default(),
        }
    }
}

impl std::fmt::Debug for ChatShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatShell")
            .field("chats", &self.list.len())
            .field("pane", &self.pane)
            .field("rows", &self.rows.item_count())
            .field("viewing", &self.viewing)
            .finish_non_exhaustive()
    }
}

impl ChatShell {
    /// An account's teardown began (review round 15, B): forgets all of its
    /// state (chat list, selection, messages, echoes, spoilers, read and
    /// viewport state, retries, countdowns), so nothing of it reaches the
    /// next account, even one with the same peer ids. Kept: the window's
    /// focus state (the OS's and core's, not the account's) and the local id
    /// counter, so a stale event for an old send never matches a new one.
    pub(crate) fn reset(&mut self) {
        let fresh = Self {
            core: self.core,
            next_local: self.next_local,
            active: self.active,
            focus_sent: self.focus_sent,
            focus_retry: std::mem::take(&mut self.focus_retry),
            ..Self::default()
        };
        *self = fresh;
    }

    /// The account went behind a logout overlay ("Logging out", "Couldn't
    /// start logout"): its tail is no longer on screen. Core forgot it too
    /// (review round 28), so no command is needed; the next frame that
    /// paints the chat again reports it.
    pub(crate) fn hidden(&mut self) {
        self.viewing = None;
    }

    /// Whether a modal overlay covers the pane, as of the frame about to be
    /// checked (review A64, N57). Covered, [`Self::frame_seen`] marks
    /// nothing read and tells core no tail is on screen; the first frame
    /// painted uncovered reports both again.
    pub(crate) fn set_covered(&mut self, covered: Covered) {
        if self.covered != covered {
            // A new cover owes a fresh withdrawal, and an uncover
            // supersedes the pending one (A76).
            self.withdraw_retry.reset();
        }
        self.covered = covered;
    }
}

#[cfg(test)]
mod reset_tests;

fn kind_of(chat: &ChatSummary) -> ForumStatus {
    if is_forum(chat) {
        ForumStatus::Forum
    } else {
        ForumStatus::NotForum
    }
}

/// Whether `chat` is a forum, whose pane shows its topic list (owner
/// decision D4: always, whatever the forum's default view).
pub(crate) fn is_forum(chat: &ChatSummary) -> bool {
    matches!(chat.kind, ChatKind::Supergroup { forum: true, .. })
}

impl ChatShell {
    pub(crate) fn pane(&self) -> Pane {
        self.pane
    }

    /// The thread whose messages the pane shows (and the composer writes
    /// to): none for a topic list.
    pub(crate) fn selected(&self) -> Option<ThreadKey> {
        self.pane.thread()
    }

    pub(crate) fn selected_chat(&self) -> Option<&ChatSummary> {
        self.list.get(self.pane.chat()?)
    }

    /// Switches the sidebar to a tab or the archive.
    pub(crate) fn show_chats(&mut self, shown: list::Shown) {
        self.list.show(shown);
        self.sync_inline();
    }

    /// The strip's next or previous tab (see [`ChatList::step_tab`]).
    pub(crate) fn step_tab(&mut self, step: list::Step) -> bool {
        let stepped = self.list.step_tab(step);
        self.sync_inline();
        stepped
    }

    /// Turns the sidebar's unread filter on or off: the inline topics
    /// follow it.
    pub(crate) fn toggle_unread(&mut self) {
        self.list.toggle_unread();
        self.sync_inline();
    }

    /// The shown forum's topic list.
    pub(crate) fn topics(&self) -> Option<&TopicList> {
        self.topics.as_ref()
    }

    /// The open topic as the retained list holds it (none while the list
    /// is loading or dropped), which every refresh re-reads.
    pub(crate) fn shown_topic(&self) -> Option<&abyss_core::model::Topic> {
        let Pane::Forum {
            open: Some(topic), ..
        } = self.pane
        else {
            return None;
        };
        self.topics.as_ref()?.get(topic)
    }

    /// What the composer area shows for the open thread. A closed topic
    /// blocks it only while the retained list says so: it opens again as
    /// soon as the topic reopens.
    pub(crate) fn composer(&self) -> Composer {
        let can_send = self.selected_chat().is_some_and(|c| c.can_send);
        let closed = self
            .topics
            .as_ref()
            .zip(self.shown_topic())
            .is_some_and(|(list, topic)| list.blocks(topic));
        match (can_send, closed) {
            (false, _) => Composer::ReadOnly,
            (true, true) => Composer::TopicClosed,
            (true, false) => Composer::Open,
        }
    }

    /// How the pane no longer fits what core says, if it does not (see
    /// [`Stale`]).
    pub(crate) fn stale_pane(&self) -> Option<Stale> {
        let chat = self.pane.chat()?;
        match (self.pane, self.forum_status(chat)) {
            (Pane::Thread(_), ForumStatus::Forum) => Some(Stale::Kind { to: None }),
            (Pane::Forum { .. }, ForumStatus::NotForum) => Some(Stale::Kind {
                to: Some(ThreadKey::whole(chat)),
            }),
            (
                Pane::Forum {
                    open: Some(topic), ..
                },
                _,
            ) => match self.open_removed {
                Some((removed, reason)) if removed == topic => Some(Stale::TopicRemoved(reason)),
                _ => None,
            },
            _ => None,
        }
    }

    /// What core last said about `chat`'s kind: the kind it told while the
    /// pane showed the chat, else its summary in the chat list.
    fn forum_status(&self, chat: ChatId) -> ForumStatus {
        match (self.shown_kind, self.list.get(chat)) {
            (Some((c, status)), _) if c == chat => status,
            (_, Some(summary)) => kind_of(summary),
            (_, None) => ForumStatus::Unknown,
        }
    }

    /// A list event that carries the pane's chat tells its kind as of now.
    fn note_summary_kind(&mut self, event: &Event) {
        let Some(chat) = self.pane.chat() else {
            return;
        };
        let summaries: &[ChatSummary] = match event {
            Event::DialogsReset(s) | Event::DialogsAppended(s) => s,
            Event::ChatBumped(s) => std::slice::from_ref(s),
            _ => return,
        };
        if let Some(summary) = summaries.iter().find(|s| s.id == chat) {
            self.shown_kind = Some((chat, kind_of(summary)));
        }
    }

    pub(crate) fn window(&self) -> Option<&MessageWindow> {
        self.window.as_ref()
    }

    pub(crate) fn rows(&self) -> &ListState {
        &self.rows
    }

    /// Where message rows record their painted bounds.
    pub(crate) fn painted(&self) -> &viewport::Painted {
        &self.painted
    }

    /// What the frame just painted showed of the message list.
    pub(crate) fn seen(&self) -> Option<Seen> {
        let messages = self.window.as_ref().map_or(0, MessageWindow::len);
        viewport::seen(&self.painted.take(), self.rows.viewport_bounds(), messages)
    }

    pub(crate) fn row_count(&self) -> usize {
        self.window.as_ref().map_or(0, MessageWindow::len) + self.shown_echoes
    }

    pub(crate) fn row(&self, ix: usize) -> Option<Row<'_>> {
        let w = self.window.as_ref()?;
        match w.get(ix) {
            Some(m) => Some(Row::Message(m)),
            None => self
                .echoes
                .for_thread(w.thread())
                .nth(ix.checked_sub(w.len())?)
                .map(Row::Echo),
        }
    }

    /// Starts the flood-wait countdown ticker unless it runs. Returns
    /// whether the caller should start one.
    pub(crate) fn start_countdown(&mut self) -> bool {
        let start = self.countdown == Countdown::Idle && self.counting_down();
        if start {
            self.countdown = Countdown::Running;
        }
        start
    }

    /// Whether a flood wait counts down: a send's, or the topic list's.
    fn counting_down(&self) -> bool {
        self.echoes.any_waiting() || self.topics.as_ref().is_some_and(TopicList::flooding)
    }

    /// One second passed: redraws the echo rows (the caller redraws the
    /// rest). Returns whether to keep ticking (something still counts down).
    pub(crate) fn countdown_tick(&mut self) -> bool {
        self.remeasure_echoes();
        let more = self.counting_down();
        if !more {
            self.countdown = Countdown::Idle;
        }
        more
    }

    /// Whether the user revealed `id`'s spoilers.
    pub(crate) fn spoilers(&self, id: MsgId) -> rich::Spoilers {
        if self.revealed.contains(&id) {
            rich::Spoilers::Revealed
        } else {
            rich::Spoilers::Hidden
        }
    }

    pub(crate) fn reveal(&mut self, id: MsgId) {
        if self.revealed.len() >= MAX_REVEALED {
            self.revealed.clear();
        }
        self.revealed.insert(id);
        if let Some(ix) = self.row_of(id) {
            self.rows.remeasure_items(ix..ix + 1);
        }
    }

    fn row_of(&self, id: MsgId) -> Option<usize> {
        self.window.as_ref()?.index_of(id)
    }

    /// Applies a chat event from core. Other events are ignored.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per event; the thread key spreads two patterns over more lines"
    )]
    pub(crate) fn apply(&mut self, event: Event) {
        if let Some(why) = Obsolete::of(&event)
            && self.echoes.obsolete(why)
        {
            self.sync_echoes();
        }
        match event {
            list @ (Event::DialogsReset(_)
            | Event::DialogsAppended(_)
            | Event::DialogsPaging { .. }
            | Event::ChatBumped(_)
            | Event::ChatFolders(_)) => {
                self.note_summary_kind(&list);
                self.list.apply(list);
                // The forum's badge decides More under the filter, and a
                // removed folder changes the view.
                self.sync_inline();
            }
            Event::History {
                thread,
                view,
                dir,
                messages,
                removed,
                has_more,
            } => self.on_history(
                thread,
                Batch {
                    view,
                    dir,
                    messages,
                    removed,
                    has_more,
                },
            ),
            Event::Reconciled {
                thread,
                view,
                newest,
            } => self.on_reconciled(thread, view, newest),
            Event::SendsCovered { sends, .. } => {
                if self.echoes.covered(&sends) {
                    self.sync_echoes();
                }
            }
            Event::ThreadInvalidated(thread) => {
                let applied = self.open_window(thread).map(MessageWindow::invalidate);
                if applied.is_some() {
                    // A new view comes: no read until it is painted.
                    self.first_unread.gate = ReadGate::AwaitingHistory;
                }
                self.sync_rows(applied);
            }
            Event::Edited(m) => {
                let applied = self.window_of_chat(m.thread.chat).map(|w| w.edit(m));
                self.sync_rows(applied);
            }
            Event::Deleted { chat, ids } => {
                let applied = self.window_of_chat(chat).map(|w| w.delete(&ids));
                self.sync_rows(applied);
            }
            Event::SendState { local_id, state } => {
                let changed = self.echoes.set_state(local_id, state, Instant::now());
                self.echo_changed(changed);
            }
            Event::SendAdmitted { local_id } => {
                let changed = self.echoes.admitted(local_id);
                self.echo_changed(changed);
            }
            Event::SendRefused { local_id } => {
                let changed = self.echoes.refused(local_id);
                self.echo_changed(changed);
            }
            Event::SendDiscarded { local_id } => {
                let changed = self.echoes.discarded(local_id);
                self.echo_changed(changed);
            }
            Event::DiscardRefused { local_id, state } => {
                let changed = self.echoes.discard_refused(local_id, state, Instant::now());
                self.echo_changed(changed);
            }
            // `LoadEnded` needs the time: see [`Self::load_ended`]. Errors
            // never end a load: core says which load ended.
            Event::Connection(state) => self.on_connection(state),
            topics @ (Event::TopicsReset { .. }
            | Event::TopicsAppended { .. }
            | Event::TopicsPaging { .. }
            | Event::TopicBumped { .. }
            | Event::TopicRemoved { .. }
            | Event::ForumInvalidated(_)) => {
                if let Event::TopicRemoved {
                    chat,
                    topic,
                    reason: reason @ (TopicGone::Deleted | TopicGone::Hidden),
                } = topics
                    && self.pane
                        == (Pane::Forum {
                            chat,
                            open: Some(topic),
                        })
                {
                    self.open_removed = Some((topic, reason));
                }
                if let Some(list) = self.topics.as_mut() {
                    list.apply(topics);
                    self.sync_inline();
                }
            }
            Event::KindChanged { chat, forum } => {
                if self.pane.chat() == Some(chat) {
                    self.shown_kind = Some((chat, forum));
                }
            }
            Event::Auth(_)
            | Event::CredentialSource(_)
            | Event::CredentialItem(_)
            | Event::Error(_)
            | Event::Notify(_)
            | Event::ClearNotifications(_)
            | Event::NotifyDeleted { .. }
            | Event::InboxRead { .. }
            | Event::LoadEnded { .. } => {}
        }
    }

    /// A load the UI asked for ended without its answer
    /// (`Event::LoadEnded`) at `now`.
    pub(crate) fn load_ended(
        &mut self,
        target: LoadTarget,
        error: Option<UserError>,
        now: Instant,
    ) {
        match target {
            LoadTarget::Dialogs => self.list.load_ended(error, now),
            LoadTarget::History { thread, dir } => {
                let Some(w) = self.open_window(thread) else {
                    return;
                };
                w.load_ended(dir, error, now);
                // The anchored open's load failed (best effort): the thread
                // opens at its tail instead.
                if matches!(dir, HistoryDir::Tail | HistoryDir::Around)
                    && self.first_unread.opening == Opening::Anchored
                {
                    self.first_unread.opening = Opening::FallbackDue;
                }
            }
            LoadTarget::Topics(chat) => {
                if let Some(list) = self.topics.as_mut().filter(|l| l.chat() == chat) {
                    list.load_ended(error, now);
                }
            }
        }
    }

    /// When the shell next wants a wake-up to retry something refused or
    /// failed, if anything waits past `now`.
    pub(crate) fn retry_due(&self, now: Instant) -> Option<Instant> {
        let focus = self
            .focus_out_of_sync()
            .then(|| self.focus_retry.due(now))
            .flatten();
        let window = self.window.as_ref().and_then(|w| w.retry_due(now));
        let topics = self.topics.as_ref().and_then(|l| l.retry_due(now));
        let fix = self.stale_pane().and_then(|_| self.fix_retry.due(now));
        // Ready (e.g. just reset by activation or Retry) wakes at once;
        // delayed waits for its backoff; exhausted stays quiet. A stopped
        // core never takes the discard, so it gets no wake-up.
        let obsolete = (self.core != CoreState::Stopped && self.echoes.any_obsolete_to_discard())
            .then(|| {
                self.obsolete_retry
                    .ready(now)
                    .then_some(now)
                    .or_else(|| self.obsolete_retry.due(now))
            })
            .flatten();
        let recovery = self.held_recovery.map(|at| at.max(now));
        [
            self.list.retry_due(now),
            window,
            topics,
            fix,
            obsolete,
            self.frame_retry.due(now),
            recovery,
            self.withdraw_due(now),
        ]
        .into_iter()
        .fold(focus, earliest)
    }

    /// A wake-up for `due` is to be scheduled unless one is already due no
    /// later. Returns the new wake-up's name, for [`Self::arm_retry`] and
    /// [`Self::retry_fired`]. It replaces the previous wake-up, whose task is
    /// dropped (cancelled): at most one is ever outstanding.
    pub(crate) fn schedule_retry(&mut self, due: Instant) -> Option<u64> {
        if self.retry_timer.as_ref().is_some_and(|t| t.due <= due) {
            return None;
        }
        self.timer_gen = self.timer_gen.wrapping_add(1);
        self.retry_timer = Some(RetryTimer {
            due,
            id: self.timer_gen,
            task: None,
        });
        Some(self.timer_gen)
    }

    /// Hands the shell the task of wake-up `id`, so replacing the wake-up
    /// cancels it. A task of a wake-up already replaced is cancelled now.
    pub(crate) fn arm_retry(&mut self, id: u64, task: Task<()>) {
        if let Some(t) = self.retry_timer.as_mut().filter(|t| t.id == id) {
            t.task = Some(task);
        }
    }

    /// Wake-up `id` fired. Returns whether it is the current one; a stale
    /// one (replaced since) must do nothing.
    pub(crate) fn retry_fired(&mut self, id: u64) -> bool {
        if self.retry_timer.as_ref().is_none_or(|t| t.id != id) {
            return false;
        }
        // Its task is the one running this: let it finish on its own.
        if let Some(task) = self.retry_timer.take().and_then(|t| t.task) {
            task.detach();
        }
        true
    }

    /// Core's connection changed. Coming (back) online is a fresh trigger.
    fn on_connection(&mut self, state: ConnState) {
        let was = self.conn.replace(state);
        if state == ConnState::Online && was != Some(ConnState::Online) {
            self.fresh_trigger();
        }
    }

    fn open_window(&mut self, thread: ThreadKey) -> Option<&mut MessageWindow> {
        self.window.as_mut().filter(|w| w.thread() == thread)
    }

    /// The open window when it shows a thread of `chat`: message ids are
    /// per chat, so edits and deletions apply to whichever thread shows them.
    fn window_of_chat(&mut self, chat: ChatId) -> Option<&mut MessageWindow> {
        self.window.as_mut().filter(|w| w.thread().chat == chat)
    }

    /// A history event. A new view (a reopened thread's full `Tail`, a
    /// reload) replaces the rows; echoes are reconciled only by
    /// `Reconciled`, never by a view change.
    ///
    /// A replaced view keeps the reading position when the message at the
    /// top of the viewport survives it (task 10e): a refresh that replaces
    /// the window keeps late live messages, and the user may be reading
    /// one. Following the tail, the list stays at the tail anyway.
    ///
    /// A view's first history moves the read gate to its first paint. The
    /// open's first view is positioned at the divider when it holds it
    /// (plan section 17.5); otherwise the list stays at the tail.
    fn on_history(&mut self, thread: ThreadKey, batch: Batch) {
        let anchor = self.scroll_anchor();
        let late = self.late_divider(thread, &batch);
        let news = self.arrival(thread, &batch);
        let (view, dir) = (batch.view, batch.dir);
        let Some(w) = self.open_window(thread) else {
            return;
        };
        let applied = w.apply(batch);
        let reset = applied == Applied::Reset;
        if let Some(text) = news {
            self.announcer.offer(thread, text, Instant::now());
        }
        self.sync_rows(Some(applied));
        if let Some(after) = late {
            self.first_unread.divider = Divider::At(after);
        }
        if reset {
            self.first_unread.gate = ReadGate::AwaitingPaint { view };
            // Rows painted before this view must not open the gate.
            drop(self.painted.take());
            if self.first_unread.opening == Opening::Positioned {
                if let Some((id, offset_in_item)) = anchor {
                    self.restore_anchor(id, offset_in_item);
                }
            } else {
                self.first_unread.opening = Opening::Positioned;
                self.scroll_to_divider(dir);
            }
        }
        self.after_echo_change();
    }

    /// Scrolls the divider's row to the top of the viewport (following the
    /// tail stops), if the window holds it. An `Around` page proves the
    /// first message above the anchor is the first newer one, even when
    /// nothing at or below the anchor is loaded: that proof is kept for
    /// the divider's row.
    fn scroll_to_divider(&mut self, dir: HistoryDir) {
        if dir == HistoryDir::Around {
            let first_new = self
                .first_unread
                .after()
                .zip(self.window.as_ref())
                .and_then(|(after, w)| w.get(w.first_above(after)?))
                .map(|m| m.id);
            self.first_unread.first_new = first_new;
        }
        if let Some(item_ix) = self.divider_ix() {
            self.rows.scroll_to(ListOffset {
                item_ix,
                offset_in_item: px(0.),
            });
        }
    }

    /// The late divider (plan section 17.5): with nothing captured at open,
    /// an incoming message that arrives at the attached tail while the tail
    /// isn't viewed (the window is inactive or scrolled up) gets the
    /// divider, right after the newest message before it. Once per open.
    fn late_divider(&self, thread: ThreadKey, batch: &Batch) -> Option<MsgId> {
        if self.first_unread.divider != Divider::Unset
            || batch.dir != HistoryDir::Newer
            || (self.active == WindowState::Active && self.viewing == Some(thread))
        {
            return None;
        }
        let w = self
            .window
            .as_ref()
            .filter(|w| w.thread() == thread && w.view() == Some(batch.view) && !w.has_newer())?;
        let newest = w.newest()?;
        batch
            .messages
            .iter()
            .any(|m| !m.outgoing && m.id > newest)
            .then_some(newest)
    }

    /// What the live region says of `batch`, if it brings an incoming message
    /// at the open thread's attached tail: who it is from.
    fn arrival(&self, thread: ThreadKey, batch: &Batch) -> Option<SharedString> {
        let w = self
            .window
            .as_ref()
            .filter(|w| w.thread() == thread && w.view() == Some(batch.view) && !w.has_newer())?;
        if batch.dir != HistoryDir::Newer {
            return None;
        }
        // An attached, loaded tail can be empty (a new chat): then nothing
        // bounds what is new from below (review A42).
        let newest = w.newest();
        let message = batch
            .messages
            .iter()
            .rfind(|m| !m.outgoing && newest.is_none_or(|n| m.id > n))?;
        let title = self.selected_chat().map(|c| c.title.as_str());
        let sender = message.sender.as_ref().map(|s| s.name.as_str()).or(title);
        Some(copy::new_message_from(sender))
    }

    /// The live region's announcement about the open thread.
    pub(crate) fn announcement(&self) -> Option<&announce::Announcement> {
        self.announcer.current(self.selected()?)
    }

    /// Whether message row `ix` starts with the divider.
    pub(crate) fn divider_before(&self, ix: usize) -> bool {
        self.divider_ix() == Some(ix)
    }

    /// The divider's row: where the window holds what comes right before
    /// the first message above the read position, or where an `Around`
    /// page proved that message the first newer one.
    fn divider_ix(&self) -> Option<usize> {
        let after = self.first_unread.after()?;
        let w = self.window.as_ref()?;
        w.divider_row(after).or_else(|| {
            let ix = w.first_above(after)?;
            (Some(w.get(ix)?.id) == self.first_unread.first_new).then_some(ix)
        })
    }

    /// The divider goes (a send): its row is redrawn without it, and none
    /// comes back while the pane shows the thread.
    fn remove_divider(&mut self) {
        let row = self.divider_ix();
        self.first_unread.divider = Divider::Gone;
        if let Some(ix) = row {
            self.rows.remeasure_items(ix..ix + 1);
        }
    }

    /// The message at the top of the viewport and how far into it the
    /// viewport starts, unless the list follows the tail.
    fn scroll_anchor(&self) -> Option<(MsgId, Pixels)> {
        if self.rows.is_following_tail() {
            return None;
        }
        let top = self.rows.logical_scroll_top();
        let id = self.window.as_ref()?.get(top.item_ix)?.id;
        Some((id, top.offset_in_item))
    }

    /// Scrolls back to message `id`, `offset_in_item` into it, if the
    /// window still holds it.
    fn restore_anchor(&self, id: MsgId, offset_in_item: Pixels) {
        if let Some(item_ix) = self.row_of(id) {
            self.rows.scroll_to(ListOffset {
                item_ix,
                offset_in_item,
            });
        }
    }

    /// Core merged the server's tail of `thread` into `view`. It ends no
    /// history load: core answers each one itself (`History` in its
    /// direction or `LoadEnded`), also one asked while the tail loaded.
    fn on_reconciled(&mut self, thread: ThreadKey, view: ViewGen, newest: Option<MsgId>) {
        let on_screen = self
            .window
            .as_ref()
            .filter(|w| w.thread() == thread && w.view() == Some(view));
        let changed = match on_screen {
            Some(w) => {
                let shown = |id: MsgId| w.contains(id);
                self.echoes.reconciled(thread, newest, Some(&shown))
            }
            None => self.echoes.reconciled(thread, newest, None),
        };
        if changed {
            self.sync_echoes();
        }
    }

    /// Takes out the sends of threads that can no longer be sent to whose
    /// text core no longer holds (see [`Echoes::take_obsolete`]), for the
    /// discarded-drafts notice.
    pub(crate) fn take_lost_sends(
        &mut self,
    ) -> Vec<(Obsolete, discarded::Lost, ThreadKey, String)> {
        self.echoes.take_obsolete()
    }

    /// An echo of `changed`'s thread changed (`None`: nothing did).
    fn echo_changed(&mut self, changed: Option<ThreadKey>) {
        if changed.is_some() {
            self.after_echo_change();
        }
    }

    /// Echoes changed: drop the ones whose server copy is shown, then
    /// refresh the echo rows.
    fn after_echo_change(&mut self) {
        if let Some(w) = &self.window {
            self.echoes.resolve(w.thread(), |id| w.contains(id));
        }
        self.sync_echoes();
    }

    /// Mirrors a window change into the list state.
    fn sync_rows(&mut self, applied: Option<Applied>) {
        match applied {
            None | Some(Applied::Ignored) => {}
            Some(Applied::Reset) => {
                self.shown_echoes = self.echo_count();
                self.rows.reset(self.row_count());
            }
            Some(Applied::Changed(ops)) => {
                for op in ops {
                    let next = match op {
                        RowOp::Splice { range, count } => {
                            let next = range.start + count;
                            self.rows.splice(range, count);
                            next
                        }
                        RowOp::Remeasure(ix) => ix,
                    };
                    if next < self.rows.item_count() {
                        self.rows.remeasure_items(next..next + 1);
                    }
                }
                // The first row carries the start-of-history marker.
                if self.rows.item_count() > 0 {
                    self.rows.remeasure_items(0..1);
                }
            }
        }
        debug_assert_eq!(self.rows.item_count(), self.row_count());
    }

    fn echo_count(&self) -> usize {
        self.window
            .as_ref()
            .map_or(0, |w| self.echoes.count(w.thread()))
    }

    /// Replaces the echo rows at the end of the list.
    fn sync_echoes(&mut self) {
        let start = self.window.as_ref().map_or(0, MessageWindow::len);
        let count = self.echo_count();
        self.rows.splice(start..start + self.shown_echoes, count);
        self.shown_echoes = count;
        debug_assert_eq!(self.rows.item_count(), self.row_count());
    }

    /// Echo rows show a countdown that changed: redraw them.
    fn remeasure_echoes(&self) {
        let start = self.window.as_ref().map_or(0, MessageWindow::len);
        if self.shown_echoes > 0 {
            self.rows.remeasure_items(start..start + self.shown_echoes);
        }
    }
}

#[cfg(test)]
mod tests;
