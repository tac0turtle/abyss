//! In-memory, bounded store of chat list, message windows and forum topics.

mod badge;
mod dialogs;
mod fence;
mod limits;
mod route;
mod topics;
mod windows;

pub use badge::{
    BadgeSeq, Badges, CURSOR_SLOTS, KIND_SLOTS, MAX_PEER_DIALOGS, PEER_CALL_INTERVAL,
    PEER_REFRESH_INTERVAL, PeerBatch, PeerScheduler, Reserved, SHOWN_SLOTS,
};
pub(crate) use dialogs::PeerMute;
pub use dialogs::{Appended, Bump, Dialogs, Guarded, Last, MAX_DIALOGS, Rest, Unread, bump_moves};
pub use fence::{Completed, FENCE_FAILURES, Fenced, KindChecks, Settled, Verdict};
pub use limits::{AUTO_REFRESH_COOLDOWN, AutoRefresh, NotBefore, Refresh};
pub use route::{ForumStatus, Route, normalize, route, with_status};
pub use topics::{
    ByIdCall, ByIdSnapshot, Discovered, FetchSeq, ListChange, MAX_DISCOVERY, MAX_DUE_BACKOFF,
    MAX_TOMBSTONES, MAX_TOPIC_LISTS, MAX_TOPICS_BY_ID, MAX_TOPICS_PER_FORUM, PageCall, PageKind,
    PageSnapshot, Paging, PostClosed, TOPICS_CALL_INTERVAL, TOPICS_PAGE, Tail, TopicEdit,
    TopicList, Topics, TopicsGen, UnreadTrigger,
};
pub use windows::{
    Change, ContinuityMark, MAX_MESSAGES_PER_CHAT, MAX_STORE_BYTES, MAX_WINDOWS, Stale, ViewGen,
    Window, Windows,
};
