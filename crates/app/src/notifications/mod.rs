//! App notification policy. Cocoa request and delegate handling live in macos.

use std::collections::{HashMap, HashSet, VecDeque};

use abyss_core::api::Notice;
use abyss_core::model::{
    ChatId, ChatKind, ChatSummary, MsgId, PeerFolder, PeerKind, ReadState, ThreadKey, TopicId,
};

mod handoff;
#[cfg(target_os = "macos")]
pub(crate) mod macos;

#[cfg(any(target_os = "macos", test))]
pub(crate) use handoff::{ClickQueue, channels};
pub(crate) use handoff::{SystemEvents, SystemSender};

pub(crate) const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.Notifications-Settings.extension";
/// Bound on the identifiers we track. Eviction also removes the OS requests.
const MAX_TRACKED: usize = 512;
/// Bound on the chats a banner was delivered for, kept to open them on a
/// click (`Notifications::provisional`). The oldest is forgotten first.
const MAX_TARGETS: usize = 128;
/// Bound on the deleted message ids kept while the first enumeration of
/// delivered notifications is outstanding ([`Notifications::early`]).
const MAX_DELETIONS: usize = 1024;
/// Bound on the chat and topic read floors kept alongside them.
const MAX_FLOORS: usize = 1024;
/// Bound on banners handed to the system whose answer is still outstanding
/// ([`Notifications::submitted`]). Applied at admission (review A51): past it
/// a new banner is not handed over at all, so every request that was handed
/// over keeps its evidence until the system answers.
const MAX_SUBMITTED: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Authorization {
    #[default]
    Unknown,
    NotDetermined,
    Denied,
    Allowed,
}

impl Authorization {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Checking permission…",
            Self::NotDetermined => "Permission not requested",
            Self::Denied => "Permission denied",
            Self::Allowed => "Permission granted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SystemSettings {
    pub(crate) authorization: Authorization,
    pub(crate) badges: bool,
}

/// Only Rust-owned values cross from the system callbacks to GPUI.
pub(crate) enum SystemEvent {
    Settings(SystemSettings),
    AuthorizationFinished,
    Click(Option<Payload>),
    Existing(Vec<Receipt>),
    /// The system's answer to a delivery request.
    Delivered {
        receipt: Receipt,
        outcome: Delivery,
    },
}

/// How the system answered a delivery request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Accepted,
    Failed,
}

pub(crate) type Cleared = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BadgePreference {
    On,
    Off,
}

pub(crate) trait Center {
    fn settings(&self);
    fn authorize(&self);
    fn deliver(&self, banner: Banner);
    fn remove(&self, ids: &[Receipt]);
    fn clear(&self, completion: Cleared);
    fn badge(&self, label: Option<&str>);
    fn save_badge_preference(&self, preference: BadgePreference);
}

/// The wire payload contains exactly three signed integers. Zero is reserved
/// for an absent topic and, together with a zero topic, for a summary's chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Payload {
    pub(crate) epoch: i64,
    pub(crate) chat: i64,
    pub(crate) topic: i64,
}

impl Payload {
    fn target(self, epoch: i64) -> Option<ThreadKey> {
        if self.epoch != epoch {
            return None;
        }
        let chat = ChatId::from_dialog_id(self.chat)?;
        let topic = if self.topic == 0 {
            None
        } else {
            if chat.kind != PeerKind::Channel {
                return None;
            }
            Some(TopicId::try_from(i32::try_from(self.topic).ok()?).ok()?)
        };
        Some(ThreadKey { chat, topic })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) payload: Payload,
    msg: MsgId,
}

impl Receipt {
    pub(crate) fn identifier(self) -> String {
        let Payload { epoch, chat, topic } = self.payload;
        format!("{epoch}:{chat}:{topic}:{}", self.msg.0)
    }

    pub(crate) fn from_identifier(id: &str) -> Option<Self> {
        let mut pieces = id.split(':');
        let mut next = || pieces.next()?.parse::<i64>().ok();
        let payload = Payload {
            epoch: next()?,
            chat: next()?,
            topic: next()?,
        };
        let msg = MsgId(i32::try_from(next()?).ok()?);
        if msg.0 <= 0 || pieces.next().is_some() || payload.target(payload.epoch).is_none() {
            return None;
        }
        Some(Self { payload, msg })
    }

    fn read_by(self, thread: ThreadKey, up_to: MsgId) -> bool {
        self.msg <= up_to
            && self
                .payload
                .target(self.payload.epoch)
                .is_some_and(|target| {
                    target.chat == thread.chat
                        && (thread.topic.is_none() || target.topic == thread.topic)
                })
    }
}

/// What the app delivered a banner for: the evidence that a click may open a
/// chat the dialogs list does not hold yet. Scoped to the account epoch.
struct Target {
    epoch: i64,
    thread: ThreadKey,
    /// The title the banner showed (already clean).
    title: String,
}

pub(crate) struct Banner {
    pub(crate) receipt: Receipt,
    pub(crate) title: String,
    pub(crate) subtitle: Option<String>,
    pub(crate) body: String,
}

/// Evidence that arrived before the launch's first enumeration of delivered
/// notifications: confirmed deletions (review A19) and read pointers (review
/// A36). The enumeration is asynchronous, so a deletion or a read can come
/// first and find nothing to remove, and the lists the app holds do not cover
/// a chat or topic nobody opened. The evidence waits here, bounded, and is
/// applied to what the enumeration reports. Past a bound it is incomplete, so
/// everything enumerated that this launch did not deliver is removed instead:
/// a banner of an earlier launch can only activate the window anyway (it
/// carries no text, D10), and a leftover banner for a message that was read or
/// deleted is worse than a lost one.
#[derive(Default)]
struct Early {
    deleted: HashSet<(Option<ChatId>, MsgId)>,
    /// The highest read pointer seen for each chat or topic.
    floors: HashMap<ThreadKey, MsgId>,
    overflowed: bool,
}

impl Early {
    fn delete(&mut self, chat: Option<ChatId>, ids: &[MsgId]) {
        for &id in ids {
            if self.deleted.len() >= MAX_DELETIONS {
                self.overflowed = true;
                return;
            }
            self.deleted.insert((chat, id));
        }
    }

    fn read(&mut self, thread: ThreadKey, up_to: MsgId) {
        if let Some(floor) = self.floors.get_mut(&thread) {
            *floor = (*floor).max(up_to);
        } else if self.floors.len() >= MAX_FLOORS {
            self.overflowed = true;
        } else {
            self.floors.insert(thread, up_to);
        }
    }

    /// The same rules as live events: a chat's own deletion ids match that
    /// chat, account-wide ids only private chats and basic groups; a chat's
    /// read pointer covers its topics, a topic's only itself.
    fn covers(&self, receipt: Receipt) -> bool {
        let Some(target) = receipt.payload.target(receipt.payload.epoch) else {
            return false;
        };
        let deleted = self.deleted.contains(&(Some(target.chat), receipt.msg))
            || (target.chat.kind.has_account_unique_ids()
                && self.deleted.contains(&(None, receipt.msg)));
        let read = [target, ThreadKey::whole(target.chat)].iter().any(|key| {
            self.floors
                .get(key)
                .is_some_and(|floor| *floor >= receipt.msg)
        });
        deleted || read
    }
}

/// A banner handed to the system whose answer has not arrived. Its evidence
/// (the tracked notification, the click target) is committed only once the
/// system accepted it (review A40): a failed request must not evict the
/// evidence of banners that are still on screen.
struct Submission {
    receipt: Receipt,
    thread: ThreadKey,
    /// The title the banner showed (already clean).
    title: String,
    /// A read or a deletion removed the banner while the request was
    /// outstanding. The native call is still outstanding, so the entry stays
    /// until its answer (review A59), but it commits no evidence and backs no
    /// click.
    covered: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prompt {
    WaitingForLogin,
    Eligible,
    InFlight,
    Finished,
}

pub(crate) struct Notifications {
    pub(crate) center: Box<dyn Center>,
    pub(crate) authorization: Authorization,
    pub(crate) badge_preference: BadgePreference,
    system_badges: bool,
    badge: u8,
    /// Distinguishes account epochs across launches. Absent for removal-only centers.
    epoch_seed: Option<u64>,
    epoch: Option<i64>,
    prompt: Prompt,
    accept_existing: bool,
    /// Deletions and reads seen while `accept_existing` still waits for its
    /// enumeration.
    early: Early,
    receipts: VecDeque<Receipt>,
    /// Message banners the system has not answered yet, the most recent last.
    /// At most [`Self::outstanding`], so at most [`MAX_SUBMITTED`].
    submitted: VecDeque<Submission>,
    /// Native delivery calls (messages and summaries) whose answer has not
    /// arrived, at most [`MAX_SUBMITTED`] (review A59). It drops only when an
    /// answer does, never when a read or a deletion covers the banner, and it
    /// outlives the account: the answers of a finished account still arrive.
    outstanding: usize,
    /// Chats and topics banners were delivered for in this epoch, at most
    /// [`MAX_TARGETS`], the most recently delivered last.
    targets: VecDeque<Target>,
}

impl Notifications {
    pub(crate) fn new(
        center: Box<dyn Center>,
        epoch_seed: Option<u64>,
        badge_preference: BadgePreference,
    ) -> Self {
        Self {
            center,
            authorization: Authorization::Unknown,
            badge_preference,
            system_badges: false,
            badge: 0,
            epoch_seed,
            epoch: None,
            prompt: Prompt::WaitingForLogin,
            accept_existing: true,
            early: Early::default(),
            receipts: VecDeque::new(),
            submitted: VecDeque::new(),
            outstanding: 0,
            targets: VecDeque::new(),
        }
    }

    /// An account is signed in, newly or restored from the last launch: the
    /// prompt becomes eligible once the system reports `NotDetermined`. A
    /// restored session counts: a quit between a sign-in and the system's
    /// settings reply would otherwise leave the person unable to ever be
    /// asked (review A6).
    pub(crate) fn signed_in(&mut self, account_epoch: u64) {
        self.epoch = self.epoch_for(account_epoch);
        if self.prompt == Prompt::WaitingForLogin {
            self.prompt = Prompt::Eligible;
        }
        self.maybe_authorize();
    }

    /// Whether this center can deliver banners and badge the Dock at all: it
    /// has an epoch seed. A removal-only center (the seed failed) can only
    /// remove.
    pub(crate) fn can_deliver(&self) -> bool {
        self.epoch_seed.is_some()
    }

    /// Whether Settings offers "Enable notifications": an account is signed in
    /// with a delivery epoch (the same condition as the automatic prompt, so
    /// Welcome and a removal-only center never ask) and the system has not
    /// been asked yet.
    pub(crate) fn can_request_authorization(&self) -> bool {
        self.epoch.is_some() && self.authorization == Authorization::NotDetermined
    }

    /// The person asked for the system prompt in Settings. Allowed only while
    /// [`Self::can_request_authorization`], so it never repeats a decision.
    pub(crate) fn request_authorization(&mut self) {
        if self.can_request_authorization() {
            self.prompt = Prompt::InFlight;
            self.authorization = Authorization::Unknown;
            self.center.authorize();
        }
    }

    fn epoch_for(&self, account_epoch: u64) -> Option<i64> {
        let seed = self.epoch_seed?;
        Some((seed.wrapping_add(account_epoch) & i64::MAX.cast_unsigned()).cast_signed())
    }

    pub(crate) fn apply_settings(&mut self, settings: SystemSettings) {
        self.authorization = match (self.prompt, settings.authorization) {
            (Prompt::InFlight, Authorization::NotDetermined) => Authorization::Unknown,
            (_, authorization) => authorization,
        };
        self.system_badges = settings.badges;
        self.maybe_authorize();
    }

    fn maybe_authorize(&mut self) {
        if self.epoch.is_some()
            && self.prompt == Prompt::Eligible
            && self.authorization == Authorization::NotDetermined
        {
            self.prompt = Prompt::InFlight;
            self.authorization = Authorization::Unknown;
            self.center.authorize();
        }
    }

    pub(crate) fn authorization_finished(&mut self) {
        self.prompt = Prompt::Finished;
        self.center.settings();
    }

    #[cfg(all(test, feature = "fake"))]
    pub(crate) fn early_is_empty(&self) -> bool {
        self.early.deleted.is_empty() && self.early.floors.is_empty() && !self.early.overflowed
    }

    #[cfg(all(test, feature = "fake"))]
    pub(crate) fn submitted_receipts(&self) -> Vec<Receipt> {
        self.submitted.iter().map(|s| s.receipt).collect()
    }

    /// The system answered a delivery request, for a message banner or a
    /// summary: its slot is free again. For a message banner still waiting,
    /// accepted and not covered since, the evidence is committed: the
    /// notification is tracked for removal and the click target remembered.
    /// Failed, covered, or answered for an epoch that ended meanwhile,
    /// nothing is.
    pub(crate) fn delivered(&mut self, receipt: Receipt, outcome: Delivery) {
        self.outstanding = self.outstanding.saturating_sub(1);
        let Some(at) = self.submitted.iter().position(|s| s.receipt == receipt) else {
            return;
        };
        let Some(sub) = self.submitted.remove(at) else {
            return;
        };
        let epoch = receipt.payload.epoch;
        if outcome == Delivery::Accepted && !sub.covered && self.epoch == Some(epoch) {
            self.track(receipt);
            self.remember(epoch, sub.thread, &sub.title);
        }
    }

    /// Takes a slot for one more native delivery call unless
    /// [`MAX_SUBMITTED`] are already waiting for the system. A refused banner
    /// is dropped (review A51): with that many unanswered the system is not
    /// keeping up, and evicting the oldest request instead would leave it,
    /// once it succeeded, on screen with no tracking and no click target.
    fn admit(&mut self) -> bool {
        if self.outstanding >= MAX_SUBMITTED {
            return false;
        }
        self.outstanding += 1;
        true
    }

    /// Records the message banner just admitted ([`Self::admit`]). A banner
    /// repeating the receipt of an outstanding one replaces its entry; both
    /// calls keep their slots until answered.
    fn submit(&mut self, submission: Submission) {
        self.submitted.retain(|s| s.receipt != submission.receipt);
        self.submitted.push_back(submission);
    }

    pub(crate) fn click_target(&self, payload: Option<Payload>) -> Option<ThreadKey> {
        payload?.target(self.epoch?)
    }

    /// The launch's enumeration of delivered notifications arrived. Deletions
    /// and reads that came before it are applied now (reviews A19, A36); the
    /// caller also reconciles against the lists it holds.
    pub(crate) fn existing(&mut self, receipts: Vec<Receipt>) {
        if !self.accept_existing {
            return;
        }
        self.accept_existing = false;
        let early = std::mem::take(&mut self.early);
        let (gone, kept): (Vec<_>, Vec<_>) = receipts
            .into_iter()
            .filter(|receipt| {
                !self.receipts.contains(receipt)
                    && !self.submitted.iter().any(|s| s.receipt == *receipt)
            })
            .partition(|&receipt| early.overflowed || early.covers(receipt));
        if !gone.is_empty() {
            self.center.remove(&gone);
        }
        for receipt in kept {
            self.track(receipt);
        }
    }

    fn track(&mut self, receipt: Receipt) {
        if self.receipts.contains(&receipt) {
            return;
        }
        if self.receipts.len() == MAX_TRACKED
            && let Some(oldest) = self.receipts.pop_front()
        {
            self.center.remove(&[oldest]);
        }
        self.receipts.push_back(receipt);
    }

    pub(crate) fn deliver(
        &mut self,
        account_epoch: u64,
        notice: Notice,
        visible: Option<ThreadKey>,
        already_read: impl FnOnce(ThreadKey, MsgId) -> bool,
    ) {
        let Some(epoch) = self.epoch.filter(|_| {
            self.authorization == Authorization::Allowed
                && self.epoch == self.epoch_for(account_epoch)
        }) else {
            return;
        };
        let banner = match notice {
            Notice::Message {
                chat,
                topic,
                msg,
                chat_title,
                sender,
                count,
            } => {
                let thread = ThreadKey { chat, topic };
                let Some(chat) = chat.dialog_id() else {
                    return;
                };
                if visible == Some(thread) || already_read(thread, msg) {
                    return;
                }
                let payload = Payload {
                    epoch,
                    chat,
                    topic: topic.map_or(0, |id| i64::from(id.get())),
                };
                let receipt = Receipt { payload, msg };
                if !self.admit() {
                    tracing::debug!("notification dropped: too many requests outstanding");
                    return;
                }
                self.submit(Submission {
                    receipt,
                    thread,
                    title: chat_title.clone(),
                    covered: false,
                });
                Banner {
                    receipt,
                    title: chat_title,
                    subtitle: sender,
                    body: if count > 1 {
                        format!("{count} new messages")
                    } else {
                        "New message".into()
                    },
                }
            }
            Notice::Summary => {
                // No evidence to keep, but a native call like any other.
                if !self.admit() {
                    tracing::debug!("notification dropped: too many requests outstanding");
                    return;
                }
                Banner {
                    receipt: Receipt {
                        payload: Payload {
                            epoch,
                            chat: 0,
                            topic: 0,
                        },
                        msg: MsgId(0),
                    },
                    title: "abyss".into(),
                    subtitle: None,
                    body: "New messages while you were away".into(),
                }
            }
        };
        self.center.deliver(banner);
    }

    fn remember(&mut self, epoch: i64, thread: ThreadKey, title: &str) {
        self.targets.retain(|t| t.thread != thread);
        if self.targets.len() == MAX_TARGETS {
            self.targets.pop_front();
        }
        self.targets.push_back(Target {
            epoch,
            thread,
            title: title.to_owned(),
        });
    }

    /// The evidence that this epoch's banner was delivered for `thread`, as a
    /// stand-in summary to open it with while the dialogs list does not hold
    /// the chat (core notifies about unlisted chats and forums, and a new
    /// message does not list them). `None` for anything the app did not
    /// deliver, or no longer remembers (the evidence is bounded, and a click
    /// whose evidence was forgotten only activates the window).
    ///
    /// Only what the banner showed is known: the title, the peer kind, and a
    /// forum by its topic. The stand-in is never sendable, whatever the kind:
    /// the chat may restrict posting, and only core's own summary knows. It
    /// is read-only until that summary replaces it, and it never replaces a
    /// listed row (review A25).
    pub(crate) fn provisional(&self, thread: ThreadKey, now: i64) -> Option<ChatSummary> {
        let epoch = self.epoch?;
        let title = self.evidence_title(epoch, thread)?;
        let kind = match thread.chat.kind {
            PeerKind::User => ChatKind::Private { contact: false },
            PeerKind::Chat => ChatKind::Group,
            PeerKind::Channel => ChatKind::Supergroup {
                forum: thread.topic.is_some(),
                gigagroup: false,
            },
        };
        Some(ChatSummary {
            id: thread.chat,
            title: title.to_owned(),
            kind,
            pinned: false,
            read: ReadState {
                unread: 0,
                read_inbox_max: MsgId(0),
            },
            unread_mentions: 0,
            last: None,
            last_date: now,
            last_id: None,
            can_send: false,
            muted_until: None,
            scope_muted_until: 0,
            folder: PeerFolder::Main,
            reading: None,
            topic_unread: None,
        })
    }

    /// The title of the banner a click on `thread` can come from. The handoff
    /// serves a click before the answer that confirms its own banner (review
    /// A60, N52), so a request still outstanding for the thread and not
    /// covered by a read or a deletion counts as much as a committed target,
    /// and the newest wins. Reading it commits nothing: unconfirmed requests
    /// never evict committed targets (review A40).
    fn evidence_title(&self, epoch: i64, thread: ThreadKey) -> Option<&str> {
        let waiting = self
            .submitted
            .iter()
            .rev()
            .find(|s| !s.covered && s.receipt.payload.epoch == epoch && s.thread == thread)
            .map(|s| s.title.as_str());
        waiting.or_else(|| {
            self.targets
                .iter()
                .find(|t| t.epoch == epoch && t.thread == thread)
                .map(|t| t.title.as_str())
        })
    }

    fn remove_matching(&mut self, matches: impl Fn(Receipt) -> bool) {
        let mut removed = Vec::new();
        self.receipts.retain(|&receipt| {
            let covered = matches(receipt);
            if covered {
                removed.push(receipt);
            }
            !covered
        });
        // A banner the system has not answered for yet goes too: the request
        // follows the one that added it, and a late answer commits nothing.
        // The entry stays, covered, because the native call is still
        // outstanding (review A59).
        for sub in self.submitted.iter_mut().filter(|s| !s.covered) {
            if matches(sub.receipt) {
                sub.covered = true;
                removed.push(sub.receipt);
            }
        }
        if !removed.is_empty() {
            self.center.remove(&removed);
        }
    }

    /// A dialog pointer covers its whole chat. A topic pointer only covers
    /// that topic, even when other notices share its Notification Center group.
    pub(crate) fn read(&mut self, thread: ThreadKey, max_id: MsgId) {
        if self.accept_existing && max_id.0 > 0 {
            self.early.read(thread, max_id);
        }
        self.remove_matching(|receipt| receipt.read_by(thread, max_id));
    }

    pub(crate) fn deleted(&mut self, chat: Option<ChatId>, ids: &[MsgId]) {
        if self.accept_existing {
            self.early.delete(chat, ids);
        }
        let ids: HashSet<_> = ids.iter().copied().collect();
        self.remove_matching(|receipt| {
            ids.contains(&receipt.msg)
                && ChatId::from_dialog_id(receipt.payload.chat).is_some_and(|target| {
                    chat.map_or_else(
                        || target.kind.has_account_unique_ids(),
                        |chat| target == chat,
                    )
                })
        });
    }

    /// A dialogs or topics snapshot: its read pointers cover the banners they
    /// reach, committed or still submitted (review A50: a snapshot arriving
    /// before the system's answer must not leave a read banner to commit).
    /// While the launch's enumeration is outstanding, every positive pointer
    /// is also kept as an early floor, in the same bounded map as live reads.
    /// A topic's, because the shell holds only the shown forum's topics, so
    /// switching forums before the enumeration would lose it. A chat's too
    /// (review A61): a later reset can drop the chat from the dialogs list
    /// before the enumeration returns its old banner.
    pub(crate) fn read_snapshot(&mut self, reads: impl Iterator<Item = (ThreadKey, MsgId)>) {
        let reads: HashMap<_, _> = reads.collect();
        if self.accept_existing {
            for (&thread, &max) in &reads {
                if max.0 > 0 {
                    self.early.read(thread, max);
                }
            }
        }
        if self.receipts.is_empty() && self.submitted.is_empty() {
            return;
        }
        self.reconcile(|thread, id| {
            reads.get(&thread).is_some_and(|max| *max >= id)
                || (thread.topic.is_some()
                    && reads
                        .get(&ThreadKey::whole(thread.chat))
                        .is_some_and(|max| *max >= id))
        });
    }

    pub(crate) fn reconcile(&mut self, read: impl Fn(ThreadKey, MsgId) -> bool) {
        self.remove_matching(|receipt| {
            receipt
                .payload
                .target(receipt.payload.epoch)
                .is_some_and(|thread| read(thread, receipt.msg))
        });
    }

    pub(crate) fn end_account(&mut self) {
        self.epoch = None;
        if self.prompt == Prompt::Eligible {
            self.prompt = Prompt::WaitingForLogin;
        }
        self.accept_existing = false;
        self.early = Early::default();
        self.receipts.clear();
        self.submitted.clear();
        self.targets.clear();
        self.update_badge(0);
    }

    pub(crate) fn clear(&mut self, completion: Cleared) {
        self.end_account();
        self.center.clear(completion);
    }

    pub(crate) fn toggle_badge(&mut self) {
        if !self.can_deliver() {
            return;
        }
        self.badge_preference = match self.badge_preference {
            BadgePreference::On => BadgePreference::Off,
            BadgePreference::Off => BadgePreference::On,
        };
        self.center.save_badge_preference(self.badge_preference);
    }

    /// Returns the next mute expiry that can change this badge without a
    /// server update. The caller arms one timer for it.
    pub(crate) fn refresh_badge(&mut self, chats: &[ChatSummary], now: i64) -> Option<i64> {
        let mut count = 0_u8;
        let mut expiry = None;
        if self.epoch.is_some()
            && self.badge_preference == BadgePreference::On
            && self.system_badges
        {
            for chat in chats.iter().filter(|chat| chat.has_confirmed_unread()) {
                if chat.muted_at(now) {
                    let until = chat.muted_until.unwrap_or(chat.scope_muted_until);
                    expiry = Some(expiry.map_or(until, |old: i64| old.min(until)));
                } else {
                    count = count.saturating_add(1).min(100);
                }
            }
        }
        self.update_badge(count);
        expiry
    }

    fn update_badge(&mut self, count: u8) {
        if self.badge == count {
            return;
        }
        self.badge = count;
        match count {
            0 => self.center.badge(None),
            100 => self.center.badge(Some("99+")),
            count => self.center.badge(Some(&count.to_string())),
        }
    }
}
