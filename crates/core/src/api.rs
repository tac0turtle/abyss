//! Commands the UI sends to core and events core sends back.

use crate::model::{
    ChatFolder, ChatId, ChatSummary, FolderEdit, LocalId, Message, MsgId, PeerFolder, ThreadKey,
    Topic, TopicId, TopicOrder,
};
use crate::store::{TopicsGen, ViewGen};

pub use crate::auth_types::{
    ApiCredentials, ApiHash, ApiHashError, ApiId, ApiIdError, AuthState, CodeDelivery,
    CredentialError, CredentialItem, CredentialSource, EnvVar, LocalDeletion, LogoutStatus,
    LogoutTrigger, MAX_CODE_DIGITS, MAX_DELIVERY_TEXT, NewLogin, NextDelivery, PASSWORD_CAPACITY,
    PasswordBuf, PasswordTooLong, RemoteLogout, SaveStatus, SentCodeInfo, StorageError, mask_phone,
};

/// Generation of the client instance. Bumps on every rebuild, login and logout;
/// completions from an older generation are dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientGen(pub u64);

/// `Debug` redacts the phone number, the login code and the message text.
pub enum Command {
    SubmitPhone(String),
    SubmitCode(String),
    /// Consumed: the buffer is wiped when core drops it.
    SubmitPassword(PasswordBuf),
    /// Requests a new login code for the same number (`Code` state).
    ResendCode,
    /// Refetches the 2FA parameters after the password token was consumed
    /// (`PasswordTokenUnavailable`).
    RetryPasswordParams,
    /// Loads the first dialogs page, or the next page once one is loaded.
    LoadDialogs,
    OpenThread(ThreadKey),
    CloseThread(ThreadKey),
    /// Scrolls the open window up (older history).
    LoadOlder(ThreadKey),
    /// Scrolls a detached window back down (newer history).
    LoadNewer(ThreadKey),
    SendText {
        thread: ThreadKey,
        text: String,
        local_id: LocalId,
    },
    /// Retries a failed or unknown send. Answered with the send's state,
    /// or `SendRefused` when core holds no such send.
    RetrySend(LocalId),
    /// Drops a failed, unknown or flood-waiting send from the pending-send
    /// table. Answered `SendDiscarded`, or `DiscardRefused` with the state
    /// of a send core keeps (one in flight cannot be discarded: `Busy`).
    DiscardSend(LocalId),
    /// The main window became active or inactive. Activation triggers a light
    /// refresh; while active, one also runs every 5 minutes (plan section 2).
    Window(WindowState),
    MarkRead {
        thread: ThreadKey,
        max_id: MsgId,
    },
    /// The thread whose live tail is on screen: the open thread, scrolled to
    /// its newest message. `None` when scrolled up or no thread is shown. Together
    /// with `Window(Active)`, this alone suppresses the unread increment for
    /// incoming messages; until the app sends it, every incoming message
    /// counts as unread and is cleared by `MarkRead`.
    ViewingTail(Option<ThreadKey>),
    /// Shows a forum's topic list: the retained copy at once (`TopicsReset`),
    /// then the server's first page. Closes any other forum view. Inside a
    /// topics flood wait, only the retained copy is shown and the load ends
    /// with `LoadEnded { Topics, FloodWait }` at once.
    OpenForum(ChatId),
    /// Leaves a forum's topic list; the list stays retained (LRU).
    CloseForum(ChatId),
    /// The next topics page of the shown forum. Answered by `TopicsPaging`
    /// after the page, or by `LoadEnded { Topics }`.
    LoadTopics(ChatId),
    /// Moves a chat into or out of the archive. On success core applies the
    /// move (`ChatBumped`); a failure is sent as `Error`.
    SetPeerFolder {
        chat: ChatId,
        folder: PeerFolder,
    },
    /// Changes a chat folder. On success core fetches the folders again
    /// (`ChatFolders`); a failure is sent as `Error`. A `Create` title that
    /// [`crate::model::folder_title`] refuses is `Rejected` unsent.
    EditChatFolder(FolderEdit),
    /// Signs out: remote `auth.logOut`, then local deletion of the session
    /// item and the cache (plan sections 5, 16). While signed out, it retries
    /// the local deletion only.
    Logout,
    /// Onboarding (plan section 17.2): use this pair. Persisted only after
    /// the first successful code request. Refused while an env override or
    /// an unreadable Keychain is in control.
    SubmitCredentials {
        api_id: i32,
        api_hash: ApiHash,
    },
    /// Deletes the stored pair (never an env override). Refused while signed
    /// in: changing credentials means logging out first.
    ForgetStoredCredentials,
    /// Re-runs credential resolution: the Retry for an unreadable Keychain,
    /// or "use the saved pair" after Back.
    ReloadCredentials,
    /// Retries saving the entered pair after a Keychain write failed
    /// ([`CredentialItem::SaveFailed`]). Refused unless a code request
    /// succeeded with that pair (its proof).
    RetryCredentialSave,
    /// One login step back (plan section 17.3): code, password and the
    /// official-app screens go to the phone screen; the phone screen goes to
    /// the credential screens. Refused once signed in (only logout goes back),
    /// except on [`AuthState::LogoutNotStarted`] after a user's logout, where
    /// it stays signed in.
    Back,
    /// Reads the logout-pending marker again after startup could not
    /// ([`AuthState::StartupBlocked`]).
    RetryStartup,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubmitPhone(_) => f.write_str("SubmitPhone(<redacted>)"),
            Self::SubmitCode(_) => f.write_str("SubmitCode(<redacted>)"),
            Self::SubmitPassword(_) => f.write_str("SubmitPassword(<redacted>)"),
            Self::ResendCode => f.write_str("ResendCode"),
            Self::RetryPasswordParams => f.write_str("RetryPasswordParams"),
            Self::LoadDialogs => f.write_str("LoadDialogs"),
            Self::OpenThread(t) => f.debug_tuple("OpenThread").field(t).finish(),
            Self::CloseThread(t) => f.debug_tuple("CloseThread").field(t).finish(),
            Self::LoadOlder(t) => f.debug_tuple("LoadOlder").field(t).finish(),
            Self::LoadNewer(t) => f.debug_tuple("LoadNewer").field(t).finish(),
            Self::SendText {
                thread, local_id, ..
            } => f
                .debug_struct("SendText")
                .field("thread", thread)
                .field("local_id", local_id)
                .finish_non_exhaustive(),
            Self::RetrySend(id) => f.debug_tuple("RetrySend").field(id).finish(),
            Self::DiscardSend(id) => f.debug_tuple("DiscardSend").field(id).finish(),
            Self::Window(w) => f.debug_tuple("Window").field(w).finish(),
            Self::MarkRead { thread, max_id } => f
                .debug_struct("MarkRead")
                .field("thread", thread)
                .field("max_id", max_id)
                .finish(),
            Self::ViewingTail(t) => f.debug_tuple("ViewingTail").field(t).finish(),
            Self::OpenForum(c) => f.debug_tuple("OpenForum").field(c).finish(),
            Self::CloseForum(c) => f.debug_tuple("CloseForum").field(c).finish(),
            Self::LoadTopics(c) => f.debug_tuple("LoadTopics").field(c).finish(),
            Self::SetPeerFolder { chat, folder } => f
                .debug_struct("SetPeerFolder")
                .field("chat", chat)
                .field("folder", folder)
                .finish(),
            Self::EditChatFolder(edit) => f.debug_tuple("EditChatFolder").field(edit).finish(),
            Self::Logout => f.write_str("Logout"),
            Self::SubmitCredentials { .. } => f.write_str("SubmitCredentials(<redacted>)"),
            Self::ForgetStoredCredentials => f.write_str("ForgetStoredCredentials"),
            Self::ReloadCredentials => f.write_str("ReloadCredentials"),
            Self::RetryCredentialSave => f.write_str("RetryCredentialSave"),
            Self::Back => f.write_str("Back"),
            Self::RetryStartup => f.write_str("RetryStartup"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowState {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online,
    /// Retiring the client instance and building a new one. `attempt` counts
    /// rebuilds since the last 5 minutes of stable operation, starting at 1.
    Rebuilding {
        attempt: u32,
    },
    /// No usable instance: a build failed and the next one waits for backoff, or
    /// the account needs a new sign-in (repeated `AccountMigrated`).
    Offline,
}

/// Lifecycle of one logical send (plan section 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    Sending,
    Sent {
        id: MsgId,
    },
    /// The server accepted it but no id mapping was observed.
    SentUnconfirmed,
    /// Outcome unknown after retries; same `random_id` is reused on retry.
    Unknown {
        attempts: u8,
    },
    Failed,
    /// The server asked to wait (`FLOOD_WAIT`): not sent. Core resends it with
    /// the same `random_id` once `seconds` (counted from this event) have
    /// passed (at least one second); it can be discarded meanwhile. Automatic
    /// resends are bounded: after repeated flood waits, or past a total retry
    /// window, the send becomes `Failed` (with `UserError::FloodWait`) and
    /// waits for Retry or Discard.
    FloodWait {
        seconds: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    /// A queue or table is full, or a conflicting operation is running.
    Busy,
    Network,
    FloodWait {
        seconds: u32,
    },
    /// The server rejected the request.
    Rejected,
    NotFound,
    ShuttingDown,
    /// The server moved the account to another data center. Core rebuilds once;
    /// if it recurs, the user has to sign in again (plan section 2, risk 6).
    AccountMigrated,
    /// Not available yet in this build.
    Unsupported,
    /// Telegram rejected the phone number (`PHONE_NUMBER_INVALID`).
    PhoneInvalid,
    /// Telegram banned the phone number (`PHONE_NUMBER_BANNED`).
    PhoneBanned,
    /// Too many login attempts for this number (`PHONE_NUMBER_FLOOD`,
    /// `PHONE_PASSWORD_FLOOD`); no countdown is given.
    PhoneFlood,
    /// The entered API credentials could not be saved to the Keychain. They
    /// stay in memory and are retried on the next successful login step.
    CredentialsNotSaved(StorageError),
    /// The stored API credentials could not be deleted.
    CredentialsNotForgotten(StorageError),
    /// Not while signed in: log out first.
    SignOutFirst,
    /// The login code is wrong or expired; ask for a new one.
    InvalidCode,
    /// The 2FA password is wrong.
    InvalidPassword,
    /// The number has no Telegram account; create it in an official app.
    SignUpRequired,
    /// The server no longer accepts this session (signed out elsewhere or
    /// revoked). The local session was removed.
    SessionRevoked,
    /// The API credentials were rejected (plan section 17.2).
    Credentials(CredentialError),
    /// Device-local storage (Keychain, cache) failed.
    Storage(StorageError),
    /// A folder edit was refused because the folder visibly changed since
    /// the UI showed it (deleted, renamed, its rules changed, or replaced by
    /// a different-looking folder): try again. Best-effort: a replacement
    /// identical in id, title, rules and shared flag is not noticed.
    FolderChanged,
    /// A folder edit was refused because it would pass Telegram's folder
    /// limits (abyss's bounds: 64 folders, 512 chats named per folder).
    FolderLimit,
    /// A logout hid the account while archive moves or folder edits waited
    /// to be sent: they were cancelled, and nothing of them reached the
    /// server.
    FolderWritesCancelled,
    /// A bug: a contained panic or broken invariant.
    Internal,
}

/// The load a [`Event::LoadEnded`] is about: one the UI asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadTarget {
    /// `LoadDialogs`.
    Dialogs,
    /// `LoadOlder` (`dir: Older`) or `LoadNewer` (`dir: Newer`) of `thread`.
    History { thread: ThreadKey, dir: HistoryDir },
    /// `OpenForum` or `LoadTopics` of this forum.
    Topics(ChatId),
}

/// Which part of a window a history event extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDir {
    /// The newest messages. An event with a new `view` replaces the window; one
    /// with the current view (cached history shown first) extends it.
    Tail,
    Older,
    /// Newer history, including live messages (`has_more = false`).
    Newer,
}

#[derive(Debug)]
pub enum Event {
    Auth(AuthState),
    /// Which credential source is in control (plan section 17.2a); `None`
    /// while none is.
    CredentialSource(Option<CredentialSource>),
    /// The credential Keychain item's save/delete state (round 11): the UI
    /// offers Retry on a failure.
    CredentialItem(CredentialItem),
    Connection(ConnState),
    /// The whole chat list, replacing the UI's copy. A reloaded first page
    /// (bootstrap, refresh) is merged into the loaded list in core, so later
    /// pages survive it.
    DialogsReset(Vec<ChatSummary>),
    /// Chats a later page added or changed: the UI updates listed chats in
    /// place and appends new ones. A page that reorders the list (after a
    /// refresh, a newly found chat is placed in server order and may evict a
    /// provisional one) is sent as `DialogsReset` instead.
    DialogsAppended(Vec<ChatSummary>),
    /// Whether more dialogs pages can be loaded (`LoadDialogs`), after every
    /// applied dialogs page. A refreshed first page restarts pagination, so
    /// this can turn `true` again after a `false`.
    DialogsPaging {
        has_more: bool,
    },
    /// A chat's summary changed (new message, unread count). A new message
    /// moves an unpinned chat to the top of the unpinned section exactly when
    /// `last_id` grew; read-state changes keep `last_id`.
    ChatBumped(ChatSummary),
    /// The account's chat folders, in server order, replacing the UI's
    /// copy. Sent after each first dialogs page and whenever they change;
    /// empty after a logout.
    ChatFolders(Vec<ChatFolder>),
    History {
        thread: ThreadKey,
        view: ViewGen,
        dir: HistoryDir,
        /// Messages added to the window, ascending.
        messages: Vec<Message>,
        /// Messages that slid out of the window at the opposite end.
        removed: Vec<MsgId>,
        /// More history exists in `dir` (for `Tail`: older history).
        has_more: bool,
    },
    ThreadInvalidated(ThreadKey),
    /// The server's tail of `thread` was applied to `view` (open, bootstrap,
    /// gap or light refresh), after the `History`/`Deleted`/`Edited` events
    /// it caused. Everything in the window up to `newest` is now the server's
    /// truth: echoes of sends at or below `newest` are in it (or were
    /// deleted). It says nothing about `SentUnconfirmed` sends, whose ids
    /// are unknown (see `SendsCovered`).
    Reconciled {
        thread: ThreadKey,
        view: ViewGen,
        newest: Option<MsgId>,
    },
    /// A load the UI asked for ended without its answer (`DialogsPaging`,
    /// or `History` in that direction). `error: None`: there was nothing
    /// to load (every dialogs page is loaded; the thread is not open, or has
    /// no more history that way). `Some`: it failed or was not queued; the
    /// error is also sent as `Error`. A `LoadOlder`/`LoadNewer` that arrives
    /// while another history load of its thread runs is not dropped: it runs
    /// once that load ends, so it is always answered by one or the other.
    LoadEnded {
        target: LoadTarget,
        error: Option<UserError>,
    },
    /// These `SentUnconfirmed` sends of `thread` are covered by server history
    /// requested after the server accepted them, and already applied to the
    /// thread's window: their messages are shown (or were deleted), so their
    /// echoes can go. Sent after the `History` events that showed them.
    /// Core fetches older pages (a few at most) when the tail alone does not
    /// reach back to a send; until this event, the echo stays.
    SendsCovered {
        thread: ThreadKey,
        sends: Vec<LocalId>,
    },
    /// A forum's whole topic list (shown topics, in display order),
    /// replacing the UI's copy and its order. `view` tags the list's
    /// generation: later `TopicsAppended`/`TopicsPaging` of another view
    /// are stale.
    TopicsReset {
        chat: ChatId,
        view: TopicsGen,
        order: TopicOrder,
        topics: Vec<Topic>,
        /// The user may post in closed topics.
        can_post_closed: bool,
    },
    /// Topics a page or by-id result added or changed, in the order core
    /// applied them (see `store::ListChange` for the placement rule).
    TopicsAppended {
        chat: ChatId,
        view: TopicsGen,
        topics: Vec<Topic>,
    },
    /// Whether more topics pages can be loaded (`LoadTopics`), after every
    /// applied page.
    TopicsPaging {
        chat: ChatId,
        view: TopicsGen,
        has_more: bool,
    },
    /// A topic of the shown forum changed live; it moves only as
    /// `store::ListChange::Bumped` says.
    TopicBumped {
        chat: ChatId,
        topic: Topic,
    },
    /// A topic left the shown list: deleted (then its open thread is
    /// invalidated), hidden, or cut by the cap.
    TopicRemoved {
        chat: ChatId,
        topic: TopicId,
    },
    /// The forum's topic list was dropped (reconcile, a forum flag change):
    /// drop the copy and wait for `TopicsReset`.
    ForumInvalidated(ChatId),
    Edited(Message),
    Deleted {
        chat: ChatId,
        ids: Vec<MsgId>,
    },
    SendState {
        local_id: LocalId,
        state: SendState,
    },
    /// Core keeps this `SendText` (task 6j): dispatched, or kept `Failed`
    /// for Retry. Comes before any `SendState` of it.
    SendAdmitted {
        local_id: LocalId,
    },
    /// Core holds nothing for this send: a `SendText` it refused (the
    /// reason, if any, is sent as `Error` first), or a `RetrySend` of a
    /// send it does not know. Its text exists only in the UI now.
    SendRefused {
        local_id: LocalId,
    },
    /// A `DiscardSend` ended: core holds nothing for this send any more.
    SendDiscarded {
        local_id: LocalId,
    },
    /// A `DiscardSend` was refused (after `Error(Busy)`): core keeps the
    /// send, in `state`.
    DiscardRefused {
        local_id: LocalId,
        state: SendState,
    },
    Error(UserError),
}

impl Event {
    /// Approximate heap bytes this event retains, for the event byte budget.
    pub fn retained_bytes(&self) -> usize {
        const BASE: usize = std::mem::size_of::<Event>();
        let chats = |v: &[ChatSummary]| {
            std::mem::size_of_val(v) + v.iter().map(ChatSummary::retained_bytes).sum::<usize>()
        };
        BASE + match self {
            Event::DialogsReset(v) | Event::DialogsAppended(v) => chats(v),
            Event::ChatBumped(c) => c.retained_bytes(),
            Event::ChatFolders(v) => {
                std::mem::size_of_val(v.as_slice())
                    + v.iter().map(ChatFolder::retained_bytes).sum::<usize>()
            }
            Event::History {
                messages, removed, ..
            } => {
                messages.iter().map(Message::retained_bytes).sum::<usize>()
                    + removed.capacity() * std::mem::size_of::<MsgId>()
            }
            Event::Edited(m) => m.retained_bytes(),
            Event::Deleted { ids, .. } => ids.capacity() * std::mem::size_of::<MsgId>(),
            Event::SendsCovered { sends, .. } => sends.capacity() * std::mem::size_of::<LocalId>(),
            Event::TopicsReset { topics, .. } | Event::TopicsAppended { topics, .. } => {
                topics.capacity() * std::mem::size_of::<Topic>()
                    + topics.iter().map(Topic::retained_bytes).sum::<usize>()
            }
            Event::TopicBumped { topic, .. } => topic.retained_bytes(),
            _ => 0,
        }
    }
}
