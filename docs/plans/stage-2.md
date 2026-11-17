# abyss Stage 2 plan: forum topics

Status: **v5.3** (2026-11-17). Nothing in it is implemented yet. v2 applied review round 1 and recorded the owner's decisions (section 14). v3 applied round 2. v4 applied round 3: every count (the forum badge and each topic's unread) is a server value only, refreshed on triggers, and snapshots are authoritative in dispatch order. v5 applied round 4, v5.1 applied round 5's fixes, v5.2 applied round 6's, and v5.3 applies rounds 7 and 8 (section 16). It builds on [stage-1.md](stage-1.md) and keeps every Stage 1 invariant (section 1). Sections of the Stage 1 plan are cited as "S1 §N".

Pinned upstreams (unchanged from Stage 1):
- **grammers** `e61cf858` (codeberg.org/Lonami/grammers, v0.12.0, TL `LAYER 233`). `G/` = `~/.cargo/git/checkouts/grammers-8937e3b5288aa015/e61cf85/`. The references below were checked against a clone of that exact revision.
- **zed/gpui** `917559da`. `Z/` as in Stage 1.
- `api.tl:N` means `G/grammers-tl-types/tl/api.tl`. `A/` = `G/grammers-session/src/message_box/adaptor.rs`.

## 0. Goal and non-goals

Stage 2 makes forum supergroups usable. Opening a forum chat shows its topic list. Choosing a topic opens that topic's thread, which has the same history, live updates, sending and read marks as a Stage 1 chat. Back returns to the topic list.

In scope:
- the topic list: paging in either server order (last activity or creation date), pinned first, unread counts, last message preview, closed and pinned marks;
- per-topic history, including the General topic (id 1), cached in the encrypted cache;
- live updates routed to the right topic, including messages from topics the client has not seen yet, topic edits, pins, closing and deletion;
- sending text into a topic, and the composer blocked in closed topics;
- read marks per topic, and the forum's chat-list badge (the server's unread-topic count, decision D1);
- everything running against the fake backend first, as in Stage 1.

Out of scope for Stage 2 (owner decisions D2–D5, section 14):
- creating, renaming, closing, reopening, pinning, reordering, hiding or deleting topics (`messages.createForumTopic`, `editForumTopic`, `updatePinnedForumTopic`, `reorderPinnedForumTopics`, `deleteTopicHistory`; `api.tl:2850-2854`). v1 is read and post only (D2);
- topic icons: custom emoji (`icon_emoji_id`) are never fetched, and `icon_color` is not shown; every topic shows a neutral "#" glyph (D3);
- the "view as messages" dialog flag (`dialog.view_forum_as_messages`, `api.tl:225`; `updateChannelViewForumAsMessages`, `api.tl:444`). A forum always opens its topic list (D4);
- caching topic lists on disk (D5, a follow-up in section 14). Topic lists live in memory only; topic *threads* are cached;
- per-topic notification settings, drafts synced from the server (`forumTopic.draft`), unread mentions and reactions navigation (`messages.getUnreadMentions` with `top_msg_id`, `api.tl:2704`);
- topic search (`getForumTopics` `q`);
- monoforums (channel direct messages, `channel.monoforum`, `inputReplyToMonoForum`, `api.tl:108, 1760`) and the forum tabs and sidebar layouts (`forum_tabs`, `forum_sidebar`, `api.tl:108`);
- discussion threads (comments under channel posts). They use the same `getReplies`/`readDiscussion` calls, but channels are out of v1.

Message links stay copy-only, and nothing outside the v1 scope is added (roadmap, "v1 scope").

## 1. Stage 1 invariants this design keeps

Each is restated with how Stage 2 respects it, so a reviewer can check the design against them:

1. **`abyss-core` has no GPUI dependency.** All topic state, routing and caps live in core. The app only renders and sends commands.
2. **Generations.** Every completion carries `ClientGen`. Thread-scoped completions carry `ViewGen` exactly as chat-scoped ones did (windows are now keyed by `ThreadKey`). Topic-list completions carry a new `TopicsGen`, which bumps on open, close, LRU eviction and invalidation. Stale completions are dropped (S1 §1).
   - Within one generation, topic snapshots are ordered by a per-list dispatch counter. A snapshot is never applied over a field group that changed after it was dispatched. That group keeps its live state and is refetched, with a bound (section 6.4). A same-generation response can't overwrite newer state.
3. **One recovery mechanism.** Nothing in Stage 2 adds a recovery path. A topic-list call that fails ends with `LoadEnded { Topics, error }`, and the UI retries with the Stage 1 backoff. Rebuild and reconcile (S1 §2) are extended to reload the open forum's first topics page, the open topic by id, and the open thread's tail.
4. **Reconcile and bootstrap.** Topic snapshots are taken only after the baseline. Updates buffered during bootstrap are stamped on receipt and merged by the same conflict rule as live ones (section 6.4).
5. **Cache continuity.** The disk cache is written live only for a thread whose window is `cache_contiguous` (today `apply.rs:735`). A window loses the flag whenever a live message of its chat bypasses it (fence, `ChatOnly`), and only a thread-scoped tail reconcile restores it (section 6.6). Tail reconciliation decides overlap against the cached tail *of that thread*, and replaces only that thread's rows (today `cache/worker.rs:358-383`, re-keyed in section 10).
6. **Bounded work.** Every new table, queue, log and response has a cap (section 9). By-id topic fetches and targeted dialog refreshes are coalesced, single-flight, and bounded per call.
7. **Single-flight and coalescing.** History loads stay single-flight per window key (now a thread). Topic pages are single-flight per forum with at most one queued. Read marks are coalesced per thread.
8. **Encrypted cache.** Topic threads are cached in the same SQLCipher database with the same validation on write and read (S1 §16). Nothing new is written in plaintext.
9. **Budgets.** Event payloads (`Event::retained_bytes`) count topic titles and previews. Store bytes are unchanged in meaning: windows still count against `MAX_STORE_BYTES`.
10. **Validated server data.** Topic titles go through `sanitize::display_name` and are checked with `is_clean(_, MAX_NAME_BYTES)`. Topic ids are validated by type at the mapping boundary and on deserialization (section 3), never with `debug_assert`.
11. **Pending sends survive rebuilds.** Entries keep their `random_id` and now also their `ThreadKey`, so a replayed send goes back into the same topic.

## 2. Telegram facts this design relies on

Verified in grammers source (`G/`) unless marked *docs* (Telegram's API documentation, not verifiable here) or *live* (needs a real account; listed again in section 13).

**Topics and the forum flag.**
- A forum is a supergroup with `channel.forum` (`api.tl:108`, `flags.30`). The existing mapping already reads it into `ChatKind::Supergroup { forum }` (`crates/core/src/backend/grammers/map/peers.rs`, `channel_entry`).
- `forumTopic` (`api.tl:1672`) has:
  - `id`;
  - `date` (the topic's creation date);
  - `title`, `icon_color`, `icon_emoji_id`;
  - `top_message`, `read_inbox_max_id`, `read_outbox_max_id`;
  - `unread_count`, `unread_mentions_count`;
  - the flags `my`, `closed`, `pinned`, `short`, `hidden`, `title_missing`.
- `forumTopicDeleted { id }` (`api.tl:1671`) is returned for deleted ids.
- A topic's id is the id of the service message that created it (`messageActionTopicCreate`, `api.tl:185`). The existing mapping already uses that (`map/message.rs`, `service`: `topic = Some(m.id)` for `TopicCreate`). Ids are therefore positive. A non-positive id is malformed (section 3).
- The General topic has id 1 (*docs*). Its messages carry no `forum_topic` reply header (*docs*). `forumTopic.hidden` applies only to General (*docs*).

**Listing topics.**
- At this layer topics are listed with `messages.getForumTopics(peer, q?, offset_date, offset_id, offset_topic, limit)` and `messages.getForumTopicsByID(peer, topics)` (`api.tl:2848-2849`). They take an `InputPeer` and moved from the `channels.*` namespace; there is no `channels.getForumTopics` in the pinned schema.
- Both return `messages.forumTopics { order_by_create_date, count, topics, messages, chats, users, pts }` (`api.tl:1674`). `messages` holds each topic's top message, which gives the preview.
- **Two orders.**
  - Without `order_by_create_date`, topics come by last activity. The next page continues from the last topic: `offset_date` = its top message's date, `offset_id` = its `top_message`, `offset_topic` = its id.
  - With the flag, topics come by creation date, and pagination must use `forumTopic.date` as `offset_date` (*docs*). Section 5 defines the cursor per mode.
  - *Live:* the exact `offset_id` to send in creation mode, whether pinned topics lead every first page and never repeat, and whether the flag can differ between pages of one traversal.
- Every `messages.Messages` variant also carries `topics: Vector<ForumTopic>` (`api.tl:298-300`).

**One chat's dialog.**
- `messages.getPeerDialogs(peers: Vector<InputDialogPeer>)` (`api.tl:2678`) returns `messages.peerDialogs { dialogs, messages, chats, users, state }` (`api.tl:872`). `inputDialogPeer { peer }` is at `api.tl:1171`.
- This is the authoritative refresh of one chat's summary, and of its kind through the `chats` entity, wherever the chat sits in the dialog list. It is used for the forum badge (section 7) and for forum-flag changes (section 6.6).

**Message headers.**
- `messageReplyHeader` (`api.tl:1442`) has `forum_topic` (`flags.3`), `reply_to_msg_id` and `reply_to_top_id`.
- The existing `map::message::reply_and_topic` reads them. With `forum_topic`, the topic is `reply_to_top_id` when present (a reply inside the topic), otherwise `reply_to_msg_id` (a plain post in the topic, which "replies" to the topic root and is not shown as a reply). Without `forum_topic`, the topic is `None`, which in a forum means General.
- Today the raw `i32` is accepted unchecked (`map/message.rs:130-136`). Stage 2 validates it (section 3).

**History of one topic.**
- `messages.getReplies(peer, msg_id, offset_id, offset_date, add_offset, limit, max_id, min_id, hash)` (`api.tl:2739`) with `msg_id` = the topic id returns that topic's messages (*docs*). It has the same offset arguments as `getHistory` (`api.tl:2620`), so `history_offsets` (`calls.rs`) carries over unchanged (*live*: the After semantics, as for Stage 1).
- `messages.search` also accepts `top_msg_id` (`api.tl:2621`), a fallback if `getReplies` misbehaves for General.
- **Unknown U1 (live):** how General's history is fetched. Candidates in order: `getReplies(msg_id = 1)`, then `messages.search(top_msg_id = 1, q = "", filter = Empty)`. The backend hides the choice behind `history(thread, ..)`.

**Sending into a topic.**
- `messages.sendMessage.reply_to: InputReplyTo` (`api.tl:2627`). `inputReplyToMessage` (`api.tl:1758`) has `reply_to_msg_id` and `top_msg_id`.
- A plain post into topic `t` sets `reply_to = inputReplyToMessage { reply_to_msg_id: t, top_msg_id: None }`. A post into General has no `reply_to` (*docs*).
- grammers' own reconstruction of a short sent message sets `forum_topic: false` and copies `top_msg_id` into `reply_to_top_id` (`A/:281-297`, flag at `:286`). Our raw send path never goes through it (S1 §9). The echo's thread always comes from the pending-send entry, never from a mapped copy of our own send.
- A send into a closed topic fails with `TOPIC_CLOSED`; into a deleted topic with `TOPIC_DELETED` (*docs*; *live*).

**Read state.**
- `messages.readHistory(peer, max_id)` and `channels.readHistory(channel, max_id)` have no topic argument (`api.tl:2622, 2933`).
- `messages.readDiscussion(peer, msg_id, read_max_id)` (`api.tl:2741`) marks one thread read; for a forum topic, `msg_id` is the topic id (*docs*).
- **Unknown U2 (live):** whether General is read with `readDiscussion(msg_id = 1)` or with `channels.readHistory`, and whether `channels.readHistory` on a forum clears other topics. The design never calls `channels.readHistory` on a forum until U2 is settled.
- Remote reads arrive as one of:
  - `updateReadChannelDiscussionInbox { channel_id, top_msg_id, read_max_id }` (`api.tl:407`), which has no unread count;
  - `updateReadHistoryInbox { peer, top_msg_id?, max_id, still_unread_count }` (`api.tl:349`), which Stage 1 maps only when `top_msg_id` is absent (`map/lists.rs`, `update`);
  - `updateReadChannelInbox` (`api.tl:356`) for the chat as a whole.
- **Risk:** grammers asserts that `updateReadHistoryInbox` never names a channel: `assert!(!matches!(u.peer, tl::enums::Peer::Channel(_)))` at `A/:520-521`, on the update path. If Telegram ever sends a per-topic read that way for a forum, the update consumer panics. That is contained (S1 §1), but costs a rebuild each time (risk 2).
- **Unknown U3 (live):** that `dialog.unread_count` (`api.tl:225`), and the `still_unread_count` of a chat-level read update, count unread *topics* for a forum (D1 assumes so; section 7).

**Topic updates.**
- `updateEditForumTopic { peer, topic_id, title?, icon_emoji_id?, closed?, hidden? }` (`api.tl:499`).
- `updatePinnedForumTopic { pinned, peer, topic_id }` and `updatePinnedForumTopics { peer, order? }` (`api.tl:474-475`).
- `updateChannel { channel_id }` (`api.tl:354`): something about the channel changed, possibly the forum flag.
- None of these has a `pts`. grammers' message box passes updates without one straight through (`G/grammers-session/src/message_box/mod.rs:485-491`; `PtsInfo::from_update` ends in `_ => None`, `A/:758`), and our consumer sees them raw through `UpdateStream::next_raw` (`G/grammers-client/src/client/updates.rs:36-38`).
- Because they have no `pts`, **nothing recovers a lost one**: no difference fetch replays it. The design never assumes a later update will arrive. Only a later snapshot corrects state (section 6.4).
- There is no "topic created" or "topic deleted" update. Creation shows up as the `messageActionTopicCreate` service message. Deletion shows up as the deletion of the topic's messages, its root included (*docs*; *live*).

**grammers has no forum API.** The only topic fields grammers-client touches are:
- the typing action's `topic_id` (`G/grammers-client/src/peer/action.rs:25-67`);
- a hardcoded `top_msg_id: None` in its own requests (`G/grammers-client/src/client/messages.rs:135, 368, 969, 1297`; `client/dialogs.rs:258`).

So every Stage 2 call is a raw `invoke`, like the Stage 1 read and send adapters.

## 3. Domain model

```rust
/// A forum topic's id: the id of the service message that created it. Always ≥ 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i32", into = "i32")]
pub struct TopicId(i32);                          // private: only validated values exist

impl TopicId {
    pub const GENERAL: TopicId = TopicId(1);
    pub const fn get(self) -> i32 { self.0 }
}
impl TryFrom<i32> for TopicId {                  // rejects id <= 0
    type Error = InvalidTopicId;
    fn try_from(id: i32) -> Result<Self, InvalidTopicId> { .. }
}

/// A conversation: a whole chat (`topic: None`) or one topic of a forum.
pub struct ThreadKey { pub chat: ChatId, pub topic: Option<TopicId> }   // was Option<i32>

/// How the server ordered a forum's topic list (`messages.forumTopics.order_by_create_date`).
pub enum TopicOrder { LastActivity, Created }

pub struct Topic {
    pub id: TopicId,
    pub title: String,            // display_name, ≤ MAX_NAME_BYTES
    pub created: i64,             // forumTopic.date, Unix seconds
    pub closed: bool,
    pub pinned: bool,
    pub unread: u32,              // the server's count only; never changed locally (section 6.4)
    pub unread_mentions: u32,
    pub freshness: Freshness,     // Current | NewSince | ReadSince | Stale: a display hint only
    pub read_inbox_max: MsgId,
    pub last: Option<Preview>,
    pub last_date: i64,
    pub last_id: Option<MsgId>,   // the top message
}
```

**`TopicId` validation (C-VALIDATE, C-STRUCT-PRIVATE).**
- The field is private, and the only constructors are `TryFrom<i32>` and the `GENERAL` const. Deserialization goes through `try_from`, so a cached row or any serialized value with a non-positive id fails to decode.
- **Network.** `reply_and_topic` converts `reply_to_top_id`/`reply_to_msg_id` with `TryFrom`. A message whose forum header names a non-positive topic is **dropped by the mapping** (`map::message` returns `None`, counted at debug), the way messages with out-of-range peers are dropped today. Routing such a message to General would misfile it.
- `forumTopic`, `forumTopicDeleted`, `updateEditForumTopic`, `updatePinnedForumTopic`, and read updates with a non-positive id are skipped item by item.
- **Commands** carry `TopicId`, so the UI can't express an invalid id. Core still treats an id it doesn't know as unknown, not as an error (section 6.3).
- **Cache.** `TopicId(0)` cannot exist, so the cache's `topic = 0` sentinel for `None` can't collide (section 10). A row with `topic < 0`, or one whose column disagrees with the decoded `thread`, is a bad row: it is deleted and skipped.
- **Tests** exercise these paths with real inputs: malformed TL headers and topic objects, serialized values with 0 and −1, and tampered cache rows. They don't rely on `debug_assert`.

**`Topic`.**
- `Topic::is_valid` checks the title with `is_clean(_, MAX_NAME_BYTES)` and the preview with `Preview::is_valid`. `Topic::retained_bytes` mirrors `ChatSummary::retained_bytes`.
- `title_missing` topics and empty sanitized titles get a fixed fallback: "General" for id 1, "Topic" otherwise (app copy, in `chat/copy.rs`).
- `icon_color` and `icon_emoji_id` are not in the model (D3).
- Visibility (`hidden`, General only) is the public `Visibility` enum below, carried only by the backend's `TopicSnapshot`. Deletion suspicion is a core-internal enum (section 6.4). Hidden topics are never emitted, so the public `Topic` (what events carry to the UI) doesn't carry either.
- **`TopicSnapshot`** is what the backend returns: `pub struct TopicSnapshot { pub topic: Topic, pub visibility: Visibility, pub top_version: i64 }`, with `pub enum Visibility { Shown, Hidden }` mapped from `forumTopic.hidden`. `top_version` is the top message's `Message::version()` (`i64::MIN` when unedited or absent). It is the snapshot's `last_version`, so a stale live edit of the top message can't overwrite a newer snapshot preview (task 2 review). Core applies `visibility` under the merge rules of section 6.4 and emits only `Topic`. This is the only path by which a cold load, or a refresh after a missed hide/unhide, learns visibility.

**Thread normalization: one function, at core's ingress.** The backend reports the topic as the server stated it: `Message.thread.topic` from `reply_and_topic`, `None` when there was no forum header. Core turns that into a canonical key in exactly one place:

```rust
/// `forum`: what core knows about the chat. `None` = unknown or being re-checked (fenced).
fn route(chat: ChatId, raw: Option<TopicId>, forum: Option<bool>) -> Route {
    match forum {
        Some(true)  => Route::Thread(ThreadKey { chat, topic: Some(raw.unwrap_or(TopicId::GENERAL)) }),
        Some(false) if raw.is_some() => Route::ForumSuspected(chat),  // the chat may have become a forum
        Some(false) => Route::Thread(ThreadKey { chat, topic: None }),
        None        => Route::ChatOnly(chat),
    }
}
```

- `Route::Thread`: the message's `thread` is rewritten to the key. It can then enter windows, the cache (subject to continuity, section 6.2) and events.
- `Route::ChatOnly`: core doesn't know whether the chat is a forum, or the chat is fenced while its kind is re-checked (section 6.6). Only the chat-list bump applies. The message is not cached and not put in any window. Nothing shows it yet, and opening the thread fetches it from the server.
- `Route::ForumSuspected`: runs the forum-change path (section 6.6), and is otherwise `ChatOnly`.
- Forum knowledge comes from `ChatSummary.kind` (dialogs pages and targeted refreshes) and from the open views. A forum view or thread window remembers its chat's kind.
- **Invariant (enforced, tested):** every message in a window, in the cache and in an event has `thread` equal to its window's key. In a forum, `topic` is always `Some`. Outside a forum, it is always `None`. Windows and the cache reject a mismatching message at runtime with an error path. It is not a `debug_assert`.

`ChatSummary` is unchanged in shape. For a forum, `unread` is the server's unread-topic count (D1, section 7).

## 4. Commands and events

The Stage 1 commands and events that name a chat's *messages* now name a `ThreadKey`. Chat-level ones keep `ChatId`. For a non-forum chat the key is `{ chat, topic: None }`, so task 1 is a pure re-key with no behaviour change.

```rust
// Re-keyed (task 1)
OpenChat(ThreadKey)            // renamed OpenThread; a forum key without a topic is refused (Rejected)
CloseChat(ThreadKey)           // CloseThread
LoadOlder(ThreadKey), LoadNewer(ThreadKey)
SendText { thread: ThreadKey, text, local_id }
MarkRead { thread: ThreadKey, max_id }
ViewingTail(Option<ThreadKey>)

// New (task 6)
OpenForum(ChatId)              // shows the topic list; closes any other forum view
CloseForum(ChatId)
LoadTopics(ChatId)             // the next topics page
```

```rust
// Re-keyed
History { thread, view, dir, messages, removed, has_more }
ThreadInvalidated(ThreadKey)   // was ChatInvalidated(ChatId)
Reconciled { thread, view, newest }
SendsCovered { thread, sends }
LoadTarget::History { thread, dir }
// Unchanged: Deleted { chat, ids } (ids are per chat; the UI drops them from whatever thread shows them),
//            Edited(Message) (carries its normalized thread), ChatBumped, Dialogs*.

// New
TopicsReset    { chat, view: TopicsGen, order: TopicOrder, topics: Vec<Topic>, can_post_closed: bool }
TopicsAppended { chat, view: TopicsGen, topics: Vec<Topic> }   // added or changed, in list order
TopicsPaging   { chat, view: TopicsGen, has_more: bool }
TopicBumped    { chat, topic: Topic }        // changed; moves only as section 6.1's order rule says
TopicRemoved   { chat, topic: TopicId, reason: TopicGone }  // Deleted (fresh by-id) | Hidden (General) | Evicted (cap); a deleted open thread is invalidated
                                             // Deleted/Hidden are always told, also for a topic the list no longer holds and before a reset that omits it; absence from a reset implies nothing (task 10 review)
ForumInvalidated(ChatId)                     // forum flag changed or reconcile: drop the copy, await TopicsReset
KindChanged { chat, forum: ForumStatus }     // forum flag changed (Forum | NotForum), listed or not; precedes its own ForumInvalidated, summaries may arrive independently (task 10 review)
LoadTarget::Topics(ChatId)
UserError::TopicClosed, UserError::TopicDeleted
```

- `can_post_closed` is `creator || admin_rights.manage_topics` from the channel entity in the topics response. Admins may post in closed topics (*docs*).
- `account_scoped` (actor) gains the three new commands, so the account-epoch check (S1 events, round 15) covers them.
- The `Debug` impl for `Command` keeps redacting text. Topic titles are never logged (S1 §11: titles are private).

## 5. Backend trait additions

```rust
pub struct TopicsCursor { pub(crate) order: TopicOrder, pub(crate) offset_date: i32,
                          pub(crate) offset_id: i32, pub(crate) offset_topic: i32,
                          pub(crate) index: u32 /* fake */ }
pub struct TopicsPage { pub order: TopicOrder, pub topics: Vec<TopicSnapshot>,
                        pub next: Option<TopicsCursor>, pub can_post_closed: bool }
pub struct TopicsById { pub found: Vec<TopicSnapshot>, pub deleted: Vec<TopicId>, pub can_post_closed: bool }

trait Client {
    // re-keyed (task 1, then given topic behaviour in task 3)
    fn history(&self, thread: ThreadKey, query: HistoryQuery, limit: u32) -> ..HistoryPage..;
    fn send_text(&self, thread: ThreadKey, text: String, random_id: i64) -> ..MsgId..;
    fn read_history(&self, thread: ThreadKey, max_id: MsgId) -> ..()..;
    // new
    fn topics(&self, chat: ChatId, cursor: Option<TopicsCursor>, limit: u32) -> ..TopicsPage..;
    fn topics_by_id(&self, chat: ChatId, ids: &[TopicId]) -> ..TopicsById..;     // ≤ MAX_TOPICS_BY_ID
    fn peer_dialogs(&self, chats: &[ChatId]) -> ..Vec<ChatSummary>..;           // ≤ MAX_PEER_DIALOGS
}

enum Update {
    // existing variants unchanged except:
    ReadInbox { chat: ChatId, topic: Option<TopicId>, still_unread: u32 },
    // new
    ReadTopicMax { chat: ChatId, topic: TopicId, max_id: MsgId },      // no count (updateReadChannelDiscussionInbox)
    TopicEdited { chat: ChatId, topic: TopicId, title: Option<String>, closed: Option<bool>, hidden: Option<bool> },
    TopicsPinned { chat: ChatId },     // updatePinnedForumTopic(s): refetch the first page
    ChatChanged { chat: ChatId },      // updateChannel: re-check the chat's kind
}
```

**Cursor per order.** The cursor is built from the last topic of a page, by the order *that page* reported:
- `LastActivity`: `offset_date` = the top message's date, `offset_id` = `top_message`, `offset_topic` = the id.
- `Created`: `offset_date` = `topic.created` (`forumTopic.date`), `offset_id` = `top_message`, `offset_topic` = the id (*live*: confirm `offset_id`).
- If a later page reports a different order than the cursor was built for, core discards it and restarts from the first page (section 6.1). Mixing orders would skip or repeat topics.

**Grammers (task 4).** Raw calls, retry class `Read` for everything except `send_text` (`Never`, S1 §9).
- **`topics`:** `messages.GetForumTopics { q: None, .. }`.
  - The response is size-checked before mapping, like `dialogs_parts` (`map/lists.rs`): `topics ≤ limit + RESPONSE_SLACK`, `messages ≤ 2 ×` that, peers `≤ MAX_RESPONSE_PEERS`. Otherwise the call fails as `Oversized`.
  - Each topic is mapped with its top message from `messages` (preview, date, and `top_version` from its `edit_date`; section 3). `order` comes from `order_by_create_date`. `forumTopicDeleted` entries are skipped.
  - `next` is `None` when the page was empty, or when the topics listed so far reach `count`. A short page is not the end: `getForumTopics` may return fewer topics than requested while more remain (task 4 review).
- **`topics_by_id`:** `messages.GetForumTopicsByID`, at most `MAX_TOPICS_BY_ID` ids, same size checks. `forumTopicDeleted { id }` goes to `deleted`.
- **`peer_dialogs`:** `messages.GetPeerDialogs`, at most `MAX_PEER_DIALOGS` peers, size-checked like a dialogs page. The dialogs are mapped with the existing dialog mapping (kind from `chats`, unread, top message). Broadcasts are excluded as in Stage 1.
- **`history`:**
  - `topic: None` keeps `GetHistory`. `Some(t)` uses `GetReplies { msg_id: t, .. }` with `history_offsets`. General uses U1's answer, behind a single private function.
  - Mapped messages whose (normalized) thread is not the requested one are dropped, the way the cache drops rows that don't match their key.
- **`send_text`:**
  - `send_request` gains a `reply_to: Option<InputReplyTo>`. `Some(t)` with `t != GENERAL` gives `inputReplyToMessage { reply_to_msg_id: t, top_msg_id: None, .. }`. General and non-forum chats give `None`.
  - `TOPIC_CLOSED` and `TOPIC_DELETED` map to `BackendError::Rejected` with a typed reason (a new `RejectReason`). Core turns that into `UserError::TopicClosed`/`TopicDeleted`.
- **`read_history`:** `topic: None` is Stage 1. `Some(t)` uses `messages.ReadDiscussion { msg_id: t, read_max_id }`. General follows U2. Until U2 is settled live, General also uses `readDiscussion(1)`, never `channels.readHistory`.
- **`update` mapping** (`map/lists.rs`). Every topic id goes through `TopicId::try_from`, and invalid ones drop the item. Everything not listed is unchanged.
  - `ReadHistoryInbox` with `top_msg_id` becomes `ReadInbox { topic: Some(..) }` instead of being dropped (non-channel peers only; grammers asserts on channel peers, section 2).
  - `ReadChannelDiscussionInbox` becomes `ReadTopicMax`, unless it names a broadcast channel (`broadcast_id` set, or the channel is a broadcast in the directory).
  - `EditForumTopic` becomes `TopicEdited`. The title is sanitized, and `icon_emoji_id` is ignored.
  - `PinnedForumTopic` and `PinnedForumTopics` become `TopicsPinned`.
  - `Channel` becomes `ChatChanged`, unless the directory says it's a broadcast.

**Fake (task 3).**
- `FakeScript` gains `topics: HashMap<ChatId, Vec<TopicSnapshot>>` (so it can hold an initially hidden General; a scripted hide/unhide updates the stored `visibility` even when its emitted update is dropped, which is how the missed-update test works) and `topic_order: HashMap<ChatId, TopicOrder>`. Messages in `history` carry raw topics the way the server sends them: `None` for General and for non-forum chats, `Some(t)` otherwise. So the fake exercises `route`.
- `FakeClient` implements the six calls over that state:
  - it filters by normalized thread;
  - it pages topics by index in the chat's configured order;
  - it refuses sends into closed topics (unless `can_post_closed`) with `TopicClosed`;
  - `peer_dialogs` returns the current summaries, including the kind;
  - it records `read_calls` with their thread.
- New ops `Topics`, `TopicsById` and `PeerDialogs` take the existing faults (`Error`, `Hang`, `Panic`, `Delay`, `CompleteThenHang`).
  - `Delay` is the tool for the crossing-response tests in section 11. A delayed response is a snapshot of the state *at call time*, so later fake mutations show up as live updates crossing it.
- `FakeHandle` gains these controls:
  - `push_update(Update)` (it exists, and carries the new variants);
  - `sent()`/`read_calls()` report the thread;
  - `create_topic`, `edit_topic`, `close_topic`, `delete_topic` (removes the topic's messages and emits the deletions), `pin_topic`;
  - `set_topic_order(chat, order)`;
  - `set_forum(chat, bool)` (emits `ChatChanged`);
  - `drop_next_update()`, which models a lost non-PTS update.
- The `cargo dev` data (`crates/app/src/chat/demo.rs`, whose `kind_of` already marks `n % 10 == 2` as forums) gives every demo forum a General topic plus 60 topics through `FakeScript::add_demo_forum` (`fake::demo_forum`). One is closed, two are pinned, and one forum (chat 12) uses creation order. Core's `FakeScript::demo` is unchanged and has no forums; tests opt in with `add_demo_forum`.
- The fake allocates message ids (and so topic ids) above a per-chat high-water mark that deletions never lower, so a deleted topic's id is never reused.
- The contract suite (`backend/contract.rs`) gains topic and peer-dialog cases that `GrammersBackend` must pass under `#[ignore]` once credentials exist.

## 6. Core

### 6.1 Topic lists (`store/topics.rs`)

```rust
pub struct TopicList {
    view: TopicsGen, order: TopicOrder,
    topics: Vec<TopicEntry> /* listed order */, next: Option<TopicsCursor>, complete: bool,
    can_post_closed: bool,
    seq: FetchSeq,                         // the list's clock: ++ on every dispatch (section 6.4)
    in_flight: Vec<InFlightFetch>,         // ≤ 2: one page, one by-id; each {seq, kind, ≤ 20 ids}
    discovery: BTreeSet<TopicId>,          // unknown ids, disposable, ≤ MAX_DISCOVERY
    due: BTreeMap<TopicId, Due>,           // required refetches, keyed by id (≤ 501); Due { next_try, step }
    cursor: Option<TopicId>,               // last id served by the drain (stable id order)
    tombstones: Vec<(TopicId, FetchSeq)>,  // ≤ MAX_TOMBSTONES
    pinned_open: Option<TopicId>,          // the open topic: never dropped by caps
}
struct TopicEntry {
    topic: Topic, visibility: Visibility, deletion: Deletion, last_version: i64,
    meta: Stamp, unread: Stamp, activity: Stamp,   // Stamp { seen: FetchSeq, changed: FetchSeq }
}
pub struct Topics { by_chat: HashMap<ChatId, TopicList>, lru: Vec<ChatId>, shown: Option<ChatId>, next_gen: u64 }
```

- **Caps.** At most `MAX_TOPIC_LISTS = 6` lists (LRU, like `Windows`; 6, not 8, keeps the section 9 budget's margin). The shown forum is never evicted. A list holds at most `MAX_TOPICS_PER_FORUM = 500` topics, plus the open topic if it would otherwise be cut. Past the cap, paging stops and the UI shows "topic limit for this view" (the Stage 1 history-limit pattern).
- **Order.** Pinned topics come first, in server order. The rest depend on the list's `order`:
  - **`LastActivity`:** server order on load. A live message moves its topic to the top of the unpinned part exactly when `last_id` grows (Stage 1's `bump_moves`). An applied snapshot that lowers `last_id` (the top message was deleted) moves the topic down to where its new `last_id` ranks. A topic learned by id goes where its `last_id` ranks among the loaded unpinned topics. When the list is incomplete and it ranks below the last loaded topic, it goes at the end, and a later page that contains it updates it in place (ids are deduplicated).
  - **`Created`:** server order on load (newest created first). Live messages update `last`, `last_date`, `last_id` and unread, but **never move** a topic. A topic learned by id is inserted by `created` (descending, ties by id descending). A new topic therefore appears at the top of the unpinned part.
  - **Order change.** A first page whose order differs from the list's resets the list (`TopicsReset` with the new order). A later page whose order differs from its cursor's is discarded, and the first page is refetched once.
  - Hidden topics (`Visibility::Hidden`) are kept for routing but never emitted. `Deletion::Suspected` topics are emitted normally until a fresh by-id result settles them.
- **Opening** a forum (`OpenForum`):
  - bumps `TopicsGen`;
  - emits `TopicsReset` from the retained list if there is one, which makes Back instant;
  - fetches the first page.
  
  The first page is merged like `Dialogs::merge_first_page`, so later pages survive a refresh. `LoadTopics` fetches the next page. Both are single-flight per forum, with at most one queued.
- **Events.** A `TopicsReset` replaces the UI's copy. Later pages are `TopicsAppended`. `TopicsPaging` follows every applied page.
- **Closing** (`CloseForum`, or opening another chat) bumps the generation, so in-flight pages are dropped. The list stays retained until LRU eviction.
- A topics page (or by-id result) also teaches core that the chat is a forum, which feeds `route`.

### 6.2 Threads, windows and cache continuity

- `Windows` is keyed by `ThreadKey` (task 1).
  - `MAX_WINDOWS = 8` now counts threads, so topics of one forum compete with other chats.
  - `shown` becomes a `ThreadKey`, and the never-evict rule applies to the open thread.
  - The account-unique message-id index is untouched, because forums are channels.
- **Continuity is per thread.** `cache_contiguous` lives on the thread's window (as today) and is set only after that thread's server tail was reconciled into the cache. Every live write-through (`apply.rs:735` for new messages, `:693` for edits, `:558` for range reconciles, `mod.rs:757`) is gated on `is_cache_contiguous(thread)`.
  - A message routed to a thread with no window, or a window that is not contiguous, is **not** written to the cache.
  - A window loses `cache_contiguous` whenever a live message of its chat is routed `ChatOnly` (a fence, or unknown forum status). Only a thread-scoped `ReconcileTail` sets it again (section 6.6).
  - So a thread closed while messages arrive leaves a gap in neither direction. The cache holds only runs the server confirmed.
- **Deletes** are safe to apply to the cache without continuity. They remove rows and never extend a run. `Windows::delete(chat, ids)` removes the ids from **every** window of that chat, whatever its topic. The cache deletion stays per chat.
- **Edits.** `replace`/`upsert` for an edit find the message by `(chat, id)` across that chat's windows. The stored copy's thread wins, and a mismatch is logged at debug.
- History loads, deferred loads, `owed`, `covers` (S1 task 10f/10g) and `history_busy` are keyed by `ThreadKey`. Their logic is unchanged.
- **Opening a topic** (`OpenChat({ chat, Some(t) })`) works exactly like opening a Stage 1 chat: cached tail first, then the server tail through `ReconcileTail` for that thread (section 10), then `Reconciled`.
  - It sets the list's `pinned_open = Some(t)` and keeps the forum's list retained, so Back is instant.
  - If `t` is not in the retained list, it is fetched by id (section 6.3).

### 6.3 Live routing

For every `NewMessage` and `Edited`, core runs `route` first (section 3). New messages and edits then go through **separate reducers**, as Stage 1 keeps `on_new_message` and `on_edited` apart (`apply.rs:688-747`).

**Principle (v4).** Counts are **server values only**: the forum badge (section 7) and every topic's unread count. Live events never add to or subtract from them. A live event that may change a count is a *trigger*: it marks the count stale and makes the topic *due* for a by-id refetch (section 6.4). Nothing can be counted twice, because nothing is counted locally.

**New message** (`Route::Thread(key)`):
1. **Chat list.** The forum's `ChatSummary` is bumped as in Stage 1 (last message, order). Its `unread` is never adjusted, and a badge refresh is scheduled (section 7).
2. **Window and cache.** The message goes to `key`'s window if that window is at the live tail. It is written to the cache only under section 6.2's continuity gate.
3. **Topic list.** If the forum's list is retained:
   - **Known topic.**
     - **Activity:** if `id > last_id`, set `last`, `last_date`, `last_id`, `last_version` (a live activity change, section 6.4) and emit `TopicBumped`. The topic moves only in `LastActivity` order. A live message only ever moves activity forward.
     - **Unread:** if the message is incoming and its thread's tail is not visible (`ViewingTail(Some(thread))` while the window is active), it is an unread trigger. `freshness = NewSince` and the topic is due. The count itself is untouched.
   - **Unknown topic** (a new topic, or one beyond the loaded pages): the id goes to the list's **discovery set** (section 6.4), a disposable queue separate from the required refetches. The `TopicCreate` service message takes this path too, because its `thread.topic` is its own id.
4. **General.** A forum message without a topic header routes to `TopicId::GENERAL` by `route`. No separate path exists.
5. **No list retained:** steps 1–2. The next `OpenForum` fetches fresh topic state.

**Edit** (`Route::Thread(key)`):
- **Window and cache.** The Stage 1 replacement-only path, re-keyed. The message replaces its copy only if it is in a window and is not older than the stored copy (`has_newer_copy`, the `Message::version` check). It is written to the cache under the continuity gate. An edit never inserts a message.
- **Topic list.** An edit never touches unread or order, and never touches the chat summary. It updates the topic's preview only when the edited message is the topic's current top message (`id == last_id`) and its version is newer than `last_version` (a live activity change). An edit of any older message leaves the topic alone, except while the topic's activity is unsettled (a live change, such as its top message's deletion, waits for a refetch): the client doesn't know the real top message then, so an edit of another message is a live activity change too (the preview is untouched), and a snapshot dispatched before it conflicts instead of settling a stale preview (task 8 review).

**Deletions.**
- `Deleted { chat, ids }` removes the ids from every window and the cache (section 6.2).
- **Deletions invalidate counts.** Deleting an unread message changes the server's counts, and no read update may follow. So for a forum:
  - a badge refresh of the chat is always scheduled (section 7);
  - each deleted id that a window held (its thread is known) is an unread trigger for that topic (`freshness = Stale`, due);
  - ids no window held can't be assigned to a topic. If the list is retained, **every** listed topic, including those with `unread == 0`, gets an unread trigger: its unread group's `changed` is set to the current `seq` and it becomes due. The count stored locally says nothing about what an in-flight snapshot captured, so the trigger can't be filtered by it. This is bounded by the list (≤ 501 entries) and served by the normal drain. An in-flight snapshot dispatched before the deletion is then a conflict for every topic and can't settle any of them;
  - the top-message and root rules below still apply.
- **The current top message.** If an id equals a listed topic's `last_id`, its preview is now wrong, and only the server knows the new top message (199, or none). Core marks the activity group changed and the topic due. The by-id refetch, dispatched afterwards, sets activity to whatever the server says, including a lower `last_id` or none. Snapshots are authoritative for activity in dispatch order; there is no monotone rule that could reject the lower value (section 6.4).
- **The root message.** If an id equals a listed topic's *id*, the root is gone. That is a **suspicion**, not a deletion: `deletion = Suspected`, and the topic is due. It stays listed and usable.
- **Removal.** A topic is removed only by a **fresh by-id `deleted`**: a by-id result whose dispatch is newer than every live change to the topic's meta group and every snapshot applied to it (section 6.4). This holds with or without a prior suspicion, so a topic deleted while the client was offline (no root-deletion update ever arrives) is removed by the by-id refetch of bootstrap or the light refresh. Removal:
  - emits `TopicRemoved`;
  - invalidates that topic's window (`ThreadInvalidated`);
  - deletes its cached rows;
  - records a tombstone.
  
  The UI shows "This topic was deleted" and goes back to the list.
- A fresh by-id result that **finds** the topic clears the suspicion. Its visibility is taken from that result, so a hidden General found again stays hidden (section 6.4). General's id 1, for instance, is not a deletable message (*live*: U4).
- A page never removes a topic. A topic missing from a page where its order says it should appear only becomes due for a by-id check.

**Topic updates.**
- `TopicEdited` patches title, closed and visibility (a live meta change). It emits `TopicBumped` without a move, or `TopicRemoved` from the emitted list when the topic becomes hidden. A `closed` change on the open topic blocks or unblocks the composer (section 8).
- `TopicsPinned` refetches the first page (coalesced with any queued page).
- For a forum with no retained list, these updates are dropped. Opening reloads anyway.

### 6.4 Snapshot merge and refetch scheduling

One rule covers every topic snapshot: first pages, later pages, by-id results, and the snapshots taken in bootstrap, reconcile and the light refresh. There is no replay and no local arithmetic.

**Field groups.** A topic's state is split in three:
- **Meta:** `title`, `closed`, `pinned`, `visibility`, `deletion` (below).
- **Unread:** the server's `unread_count`, `unread_mentions_count`, `read_inbox_max_id`, plus the local `freshness` marker (below).
- **Activity:** `last`, `last_date`, `last_id`, `last_version`.

**Stamps.**
- Each list has one logical clock, `seq: FetchSeq`. Every topics fetch (page or by-id) takes `++seq` when it is **dispatched**, so results are ordered by dispatch, whatever order they complete in.
- Per topic and per group, core keeps a `Stamp { seen, changed }`:
  - `seen`: the dispatch seq of the snapshot that last set the group;
  - `changed`: the value of `seq` at the group's last live change or trigger.
  
  Live changes are: a new top message (activity), an edit of the top message (activity), deletion of the top message (activity), unread triggers (unread), `TopicEdited` and root-deletion suspicion (meta).

**Merge rule.** For a result with seq `S`, and for each topic and group in it:
- **Apply** the group iff `S > max(seen, changed)`. The snapshot was dispatched after everything the client knows about this group, so it is authoritative, even when it moves `last_id` backwards or lowers a count. Then `seen = S`. Applying the unread group sets `freshness = Current`.
- **Stale** (`S <= seen`): skip it.
- **Conflict** (`seen < S <= changed`): skip it and keep the current state. The topic stays (or becomes) due, so a later-dispatched refetch settles it.
- A topic new to the list is applied as is. Live events for topics not in the list aren't stored, but each one adds the topic to discovery.
- **Unlisted changes (task 2 review).** Each list keeps one stamp, `unlisted_change`: the clock's value at the last live event that touched a topic the list doesn't hold (a message, an edit, a read, a topic edit), and at every deletion (any id may belong to such a topic). A topic that a result dispatched at or before that stamp adds to the list starts with every group `changed = unlisted_change`, and it is due. This is conservative: it costs a refetch, never a value. It protects eventual correction, because otherwise a page in flight across a dropped event (for example a root deletion, or an edit of the top message) could insert stale state that nothing refetches.
- **Settling a due topic (task 2 review).** A by-id result clears a topic's due state only when the topic is fully settled afterwards: every group's last applied snapshot postdates its last live change (`seen > changed`), and no deletion is suspected. Otherwise the topic stays due and backs off, even when the result was merely stale. Pages never settle due state.
- Every live change of a group (activity growth from any message, an edit of the top message, a topic edit, a trigger) makes its topic due. The posting permission (`can_post_closed`) has its own dispatch stamp and follows the same "newer dispatch wins" rule; a discarded page changes nothing.

**Example (finding 1).** A by-id snapshot for topic T is dispatched at seq 5, and the server already has message 101. The result applies `unread = 1`. Then the delayed live 101 arrives. It is a trigger, not an increment: `freshness = NewSince`, `changed = 5`, T is due. The next refetch is dispatched at seq 6 or later and gives the server's count again (1). No count ever reads 2.

**Unread freshness** (a display hint, never a count):

```rust
pub enum Freshness { Current, NewSince, ReadSince, Stale }
```

- The last trigger wins: an incoming message sets `NewSince`, a local read completing (section 6.8) sets `ReadSince`, and a remote read sets `Stale`.
- An applied unread snapshot sets `Current`.
- How it is shown is in section 8.

**Meta: visibility and deletion are separate** (C-CUSTOM-TYPE; fixes finding 7):

```rust
// `Visibility` is the public enum from section 3, set from TopicSnapshot.visibility and TopicEdited.
enum Deletion { None, Suspected }           // core-internal; a local suspicion only, removal is not a state
```

- A fresh by-id result that finds the topic sets both from the result: `visibility` from its `TopicSnapshot.visibility`, and `deletion = None`. Page results set `visibility` the same way, under the meta group's stamp.
- So hidden → suspected → found-hidden ends `Hidden`, not exposed.

**Tombstones.**
- A fresh by-id `deleted` (one that passes the merge rule for the meta group) with seq `S` removes the topic and records `(id, S)`. A tombstone blocks re-insertion by any result whose seq is below `S`, and is dropped once no in-flight fetch is older than it.
- **Cap:** `MAX_TOMBSTONES = 64` per list. Before one is discarded for room, every in-flight fetch older than it is invalidated (its result is dropped on arrival) and re-queued.

**Refetch scheduling (required vs. disposable; fixes finding 3).**
- **Required work lives on the entries themselves** (the implementation keeps the due record inline on each entry, with no separate map). A topic is *due* when any group has a live change newer than its `seen`, when it is `Suspected`, or when a page missed it. Each due entry carries `next_try: Instant` and a backoff step: 0 s, then 1 s, 2 s, 4 s, … up to 30 s after each conflict. Due state can't overflow, because it costs no extra storage beyond the ≤ 501 entries.
- **Draining** is fair and independent of display order. Due topics live in `due`, a map keyed by `TopicId`, not in the display-ordered vector. Live bumps, pins and snapshots reorder the display, but never the drain.
  - Each by-id call reserves **one slot for the open topic** when it is due.
  - The other **19** slots (20 when the open topic isn't due) take eligible due ids in ascending id order, starting after `cursor` and wrapping around. `cursor` becomes the last id taken.
  - There are at most 500 non-open entries, so any eligible due topic is served within **`ceil(500 / 19) = 27` calls**. A continuously due open topic takes its slot every call without slowing the others below 19 per call.
- **Discovery is disposable.** Unknown topic ids go to `discovery`, at most `MAX_DISCOVERY = 64` per list, and fill any room left in a by-id call. On overflow, discovery is cleared and the list is marked `needs_first_page`. Losing discovery ids loses nothing required: an unknown topic has no entry to keep correct, and it appears in pages as the user scrolls.
- **Automatic first-page refreshes are rate-limited** (overflow, `TopicsPinned`, a page order change). They go through one account-wide limiter: at most one automatic first-page topics fetch per `AUTO_REFRESH_COOLDOWN = 30 s`.
  - A request inside the cooldown sets one coalesced *trailing* request, run when the cooldown ends, and only for the forum shown at that time.
  - A flood wait extends the cooldown to the server's wait.
  - Lists that aren't shown only keep `needs_first_page`, which their next `OpenForum` satisfies (an open is a user request and isn't limited).
  - So repeated overflow costs at most one page fetch per 30 s, however many lists overflow.
- **Rate.**
  - Only the **shown** forum's list issues by-id calls: one in flight, at least `TOPICS_CALL_INTERVAL = 1 s` apart.
  - **Flood waits.** `Read`-class calls return a flood wait longer than 5 s to the caller instead of sleeping (`backend/grammers/invoke.rs:67-73`). So a `FLOOD_WAIT` from any topics call (`getForumTopics` or `getForumTopicsByID`) sets an account-level `topics_not_before = now + wait`, which only ever extends.
    - No automatic topics call (drain, discovery, automatic first page, trailing request) is dispatched before it.
    - A user's `OpenForum` or `LoadTopics` inside it shows the retained list, if any, and is answered `LoadEnded { Topics, Some(FloodWait { seconds }) }` at once. Section 8 says what the UI does with it.
    - The deadline is actor state, not list or instance state. Forum switches, list eviction, backoff resets (light refresh, reconcile) and rebuilds all preserve it.
    - The `peer_dialogs` scheduler keeps its own `peer_not_before` by the same rule (section 7).
  - Due entries of other retained lists wait. Opening such a forum refetches its first page, and the drain continues from there.
  - A call's ids are bounded (20), so the worst case is one 20-id call per second for the one forum on screen.
- The light refresh and reconcile reset every backoff to 0, so nothing waits longer than the 5-minute refresh plus one drain cycle (27 calls).

**What this does not promise.** Topic edits, pins and topic reads have no `pts` (section 2). A lost one is corrected only by the next snapshot that applies: the drain, the light refresh (on activation and every 5 minutes, always including the open topic) or a reconcile. This is the Stage 1 silent-gap residual risk (S1 risk 7), extended to topic metadata (risk 3).

### 6.5 Bootstrap, reconcile, light refresh

- **Bootstrap step 5 (S1 §2)** gains, after the dialogs page and the open thread's tail:
  - the shown forum's first topics page;
  - **if a topic is open and that page doesn't contain it, `topics_by_id([open])`**.
- **Fenced chats in bootstrap** (no deadlock with section 6.6):
  - kind checks are dispatched as soon as the baseline is established, as priority jobs like the bootstrap snapshots, not after `Online`;
  - **a fenced open thread's tail is not part of bootstrap**. Its chat is left out of the boot's awaited tails (today `boot.rs:150`, `snapshots_done` waits for every tail), so bootstrap completes without it;
  - when the fence lifts, by any outcome including abandonment, the tail is reloaded by section 6.6's lift rule, as a normal load after `Online`, or as a priority job if bootstrap is still running.
  
  Bootstrap therefore never waits on a fence, and a fence never waits on bootstrap.
- **Bootstrap merge.** Reconcile has invalidated the lists, so the bootstrap snapshots start from no state. Updates buffered during bootstrap are stamped when they are *received*:
  - a buffered change received before a snapshot's dispatch is older than that snapshot and is discarded for the topics it contains;
  - one received after it is a live change (the topic becomes due).
  
  Messages still go to windows and the cache by the Stage 1 bootstrap rules.
- **Reconcile:**
  - every retained topic list is invalidated (`ForumInvalidated`, generation bump);
  - the shown forum's first page and, when needed, the open topic by id are refetched, along with `DialogsReset`;
  - the open thread's tail is refetched;
  - every window is invalidated as before (`ThreadInvalidated`).
- **Light refresh** (window activation, every 5 minutes):
  - the first dialogs page;
  - the open thread's tail;
  - the shown forum's first topics page;
  - the open topic by id when the page doesn't contain it;
  - resetting every due entry's backoff;
  - a `peer_dialogs` refresh of the shown forum when it is not on the first dialogs page.
  
  Single-flight as before.
- **The open topic is refreshed on every one of these paths**, wherever it sits in the list, and is first in every drain. A stale `closed = true` lasts at most until the next drain call or refresh.
- **The open topic deleted while offline:** no root-deletion update arrives, so there is no suspicion. The by-id fetch of bootstrap (or of the light refresh) is fresh, because the reconcile left no newer state. Its `deleted` therefore removes the topic by section 6.3's rule: `TopicRemoved`, the thread is invalidated, and the UI goes back to the list.

### 6.6 Forum flag changes

- **Triggers:**
  - `ChatChanged { chat }`;
  - a `Route::ForumSuspected`;
  - an authoritative summary (dialogs page or `peer_dialogs`) whose `kind` changes the forum flag of a known chat;
  - a topics call answered with a "not a forum" rejection for a chat core thought was one.
- Only **known chats** are fenced: a chat in the loaded dialogs or with a window. A trigger for any other chat is ignored. It has no windows to protect, and `route` already answers `ChatOnly` for it.
- **Targeted check.** Unless the trigger already carried an authoritative summary, core asks `peer_dialogs([chat])`. That works wherever the chat sits in the list. A first-page refresh would not reach a chat beyond page one, because `merge_first_page` keeps such chats unchanged (`store/dialogs.rs:124-158`).
- **The kind-check queue** (replaces v3's slots and reconcile fallback; fixes finding 5):
  - **One FIFO of fenced chats, deduplicated, with no reconcile fallback.**
  - **Bound by the required-peer set.** A chat may be fenced only while it is in the set core already maintains for `Client::retain_peers` (`backend/mod.rs:310-317`): pending sends, open windows and retained dialogs, deduplicated and at most `session::MAX_PINNED_PEERS` (1,264). Deferred commands are bounded by `loads::MAX_DEFERRED`; a chat with a deferred command has a window or a dialog entry, so it is in that set. The FIFO, the in-flight batch and every per-chat mark (`recheck`, retry count, `suspect`) are therefore bounded by `MAX_PINNED_PEERS`, whatever dialogs paging, refreshes, rebuilds or flood waits do.
  - **Leaving the set retires the chat's kind-check state.** This is the same coalesced pass that recomputes `retain_peers` (dialogs reset, first-page merge, append, window eviction, a send leaving the table). For each chat that left:
    - it is removed from the FIFO;
    - if its check is in flight, its slot in that batch is marked retired, and its result is ignored when it arrives;
    - its `recheck`, retry count and `suspect` marks are dropped;
    - its fence is released. It has no windows (otherwise it would still be in the set), so there is no continuity to restore and no tail to reload;
    - any deferred command left for it is answered (`LoadEnded { error: None }`).
    
    A chat that re-enters the set later starts with no state, and the next trigger fences it again.
  - **Dispatch:** kind checks take up to `KIND_SLOTS = 10` of the 20 chats in every `peer_dialogs` call, from the head of the FIFO, plus any slots the badges leave unused (section 7's slot split). One call is in flight, and calls are at least `PEER_CALL_INTERVAL` apart (section 7). So each queued check receives an attempt within `ceil(n / 10)` calls (at most 127 for the 1,264-chat bound), excluding rebuild re-dispatch. This bounds first service, not fence lifetime. Each fence has two budgets, reset only when a new fence starts: a **recheck budget of 1** and a **cumulative failure budget of 3** (the third failure abandons). Attempts discarded by a rebuild (stale `ClientGen`) consume neither budget and are not counted. So a fence ends at completion, abandonment or retirement after at most **4 completed attempts** (for example fail, fail, succeed with a recheck pending, then the recheck either succeeds or is the third failure). After a rebuild they are dispatched once the baseline is established (section 6.5).
  - **Repeated triggers:** a chat already queued stays in place. A chat whose check is in flight gets a `recheck` mark, so it is queued once more after the result, because the result may predate the new trigger. This uses the fence's recheck budget: at most one recheck per fence; later triggers during that recheck are covered by the next light refresh's `suspect`/re-check path.
  - **Completion:** the kind is applied (below), and the chat leaves the queue.
  - **Failure:** it counts against the fence's failure budget. The first two failures send the chat back to the tail of the FIFO; the third **abandons** it: it keeps its old kind, is marked `suspect` (the next light refresh re-queues it, and `route` answers `ChatOnly` for its topic-headed messages until then), and leaves the queue as for "unchanged".
  - **Rebuild:** the in-flight call is dropped (stale `ClientGen`). Its chats return to the head of the FIFO. Reconcile reloads the first dialogs page, which may settle some of them; settled ones leave the queue. A reconcile never empties or re-admits the queue, so it can't loop back into itself.
- **Fence.** From the trigger until the chat leaves the queue, the chat is *fenced*:
  - `route` answers `ChatOnly` for it;
  - **on entry, every window of the chat loses `cache_contiguous`**, and its `ViewGen` is bumped so in-flight history completions are stale;
  - no history load, cache read or cache write for any of its threads is started.
- **Deferred commands** for fenced chats:
  - `OpenChat`, `LoadOlder` and `LoadNewer` wait in the existing deferred-load list (`loads::MAX_DEFERRED`). A command beyond that cap is answered at once with `LoadEnded { error: Some(Busy) }`.
  - Waiting commands are answered when their chat leaves the queue, whatever the outcome.
  - `SendText` is admitted to the pending-send table (256 entries, as in Stage 1) but not dispatched until the fence lifts.
- **Lifting the fence.** Whatever the outcome:
  - **Changed kind:** invalidate every window and the topic list of the chat, delete its cached messages (their thread keys changed meaning), and emit `KindChanged { chat, forum }`, `ForumInvalidated(chat)` and, for a listed chat, `ChatBumped`. The UI shows the topic list or the plain chat; `KindChanged` reaches it also for a chat it has open that the dialogs list no longer holds. It precedes its own `ForumInvalidated`; summaries carrying the kind may arrive independently, before or after it, so the UI reconciles from either (task 10 review).
  - **Unchanged kind, or abandoned:** continuity is still **not** restored. Every window of the chat is invalidated. The open thread, if it belongs to the chat, reloads its tail, and only that thread-scoped `ReconcileTail` (section 10) can set `cache_contiguous` again.
  - Deferred commands then run.
- **General rule:** whenever a live message for a chat is routed `ChatOnly` while that chat has windows, all of its windows lose `cache_contiguous`. A window keeps the flag only while every live message of its thread has reached it.

### 6.7 Sending

- Pending-send entries hold a `ThreadKey` instead of a `ChatId`. `random_id` rules, attempts, replay and `SentUnconfirmed` covering are unchanged (S1 §9). A cover now comes from server history *of that thread*.
- **Closed topics.**
  - `SendText` into a topic that core knows is closed (from a retained list), when `!can_post_closed`, is refused before admission with `Error(TopicClosed)` then `SendRefused`.
  - The server's `TOPIC_CLOSED` (the list was stale) gives `Failed` with `UserError::TopicClosed`. The topic becomes due, so a by-id refetch brings the server's meta.
  - `TOPIC_DELETED` gives `Failed` and a deletion suspicion for the topic (section 6.3), settled by a fresh by-id result.
- An echo is keyed by its entry's thread. A server message replaces it by id, as in Stage 1.

### 6.8 Read marks

- `reads: HashMap<ThreadKey, MsgId>`, coalesced for 500 ms, at most `MAX_DIALOGS + MAX_TOPICS_PER_FORUM` keys. A new key beyond the cap is refused with `Busy`, as in Stage 1.
- **The flush** (today `flush_reads`, `actor/mod.rs:805`) spawns one `read_history(thread, max_id)` per key. Each completion carries its `ThreadKey` and `max_id` back to the actor.
- **Admission changes no count** and schedules nothing.
- **When a topic read completes successfully** (`ReadDone { thread, max_id }`, under the current `ClientGen`):
  - the topic gets an unread trigger (`freshness = ReadSince`, due);
  - a badge refresh of the chat is scheduled (section 7).
  
  Both are scheduled only now, so neither refetch can be answered from state the server had before the read.
- **A failed read** schedules nothing. Its key returns to `reads` for the next flush, as in Stage 1.
- **Remote reads:**
  - `ReadInbox { topic: Some(t), .. }` and `ReadTopicMax` are unread triggers for `t` (`freshness = Stale`, due) and schedule a badge refresh;
  - `ReadInbox { topic: None }` sets the chat's badge (authoritative, section 7).

## 7. Chat list behaviour for forums

**Decision D1:** a forum's row shows the server's unread-*topic* count. The row otherwise looks like any group row, with the "forum" kind label it already has (`copy::kind_label`). Opening a forum row shows the topic list and marks nothing read.

**The badge is only ever set from the server.** It is never adjusted locally, because the retained topics are a subset and local deltas can't be proven consistent with the server's baseline.

**Authoritative sources.**
- The forum's `ChatSummary.unread` is *set* from:
  - a dialogs page that contains the chat;
  - a `peer_dialogs` result;
  - a chat-level `ReadInbox { topic: None }`.
- Sources are ordered by one actor-wide clock, `badge_seq`, advanced at every dialogs or `peer_dialogs` dispatch and at every chat-level read update applied. A source is applied only if its stamp is greater than the stamp of the value currently shown.

**Refresh triggers.** Each of these schedules a `peer_dialogs` refresh of the chat, and changes nothing else:
- a new incoming message routed to the forum;
- a local topic read that **completed successfully** (section 6.8). It is never scheduled at admission, so the refresh can't be answered before the read lands;
- a remote topic read (`ReadInbox { topic: Some }`, `ReadTopicMax`);
- a message deletion in the forum (`Deleted { chat, .. }`, section 6.3).

**Dirty bits, not a droppable queue.** A trigger sets the chat's **badge-dirty stamp** to the current `badge_seq`. The stamp is a field on core's per-chat state for retained dialogs, so it can't overflow: at most `MAX_DIALOGS` (1,000).
- **Clearing:** only an authoritative result for that chat whose *dispatch* stamp is greater than the dirty stamp clears it. That is a `peer_dialogs` result or a dialogs page that contains the chat. A trigger that crosses an in-flight call sets a newer stamp, so the result doesn't clear it, and the chat is refreshed again (this replaces v4's follow-up rule).
- **Draining:** fair and stable. Dirty chats other than the shown forum are taken in ascending `ChatId` order from a cursor, wrapping around, into the badge-cursor slots of each call (below). The shown forum has its own slot. A chat isn't taken again within `PEER_REFRESH_INTERVAL = 3 s` of its last dispatch. Kind checks and the shown forum can never take the cursor's reserved slots, so any dirty chat is reached within `ceil(999 / 9) = 111` calls, whatever else is queued.
- **Leaving the dialogs list** drops the chat's state, and with it the bit. There is no badge to keep correct for a chat that isn't listed.
- Nothing is dropped and no dialogs-page refresh is substituted, so a chat beyond page one can't be left stale by overflow.

**The `peer_dialogs` scheduler** (one per account, shared by kind checks and badge refreshes).
- **Rate:** at most one call in flight, and calls at least **`PEER_CALL_INTERVAL = 3 s`** apart, a conservative default. Nothing establishes a faster rate as safe; live verification checks it (section 13). A flood wait sets `peer_not_before = now + wait`, which only extends and is kept across navigation and rebuilds. No call is dispatched before it.
- **Batch:** at most `MAX_PEER_DIALOGS = 20` chats per call, split into reserved slots:
  - `KIND_SLOTS = 10` for kind checks (section 6.6);
  - `SHOWN_SLOTS = 1` for the shown forum's badge;
  - `CURSOR_SLOTS = 9` for the badge cursor.
  
  Unused slots spill over, kind checks first, then the cursor. Reserved slots guarantee progress for all three, even with a FIFO that never empties and a shown forum that is always dirty.
- **Latency, at the nominal rate** (one call per 3 s; slow responses, flood waits and deadlines stretch it). The shown forum's badge trails a change by about one interval plus a round trip. Any other dirty forum is reached within 111 calls (about 333 s) with all 1,000 dirty, and within `ceil(n / 9)` calls with n dirty, however busy the kind checks are. With few kind checks, spill-over makes it faster (up to 19 per call). The badge never shows a number the server did not send (risk 4).

**Non-forum chats** keep Stage 1's message-count rules unchanged.

**Topic rows** show each topic's own server unread count, with the freshness hint of section 8. These are per-topic numbers and are not summed into the badge.

## 8. UI

**Navigation.** `ChatShell.selected: Option<ChatId>` becomes a `Pane`:

```rust
enum Pane { Empty, Thread(ThreadKey), Forum { chat: ChatId, open: Option<TopicId> } }
```

- Selecting a non-forum chat gives `Thread({chat, None})`, which is Stage 1.
- Selecting a forum gives `Forum { open: None }` and sends `OpenForum`. The pane shows the topic list.
- Selecting a topic sends `OpenChat({chat, Some(t)})` and gives `Forum { open: Some(t) }`. The pane shows the thread.
- **Back** (a "‹ Topics" button in the thread header, and Cmd+[) sends `CloseChat(thread)` and returns to `Forum { open: None }`. The retained list renders immediately. Escape is not bound, because it cancels IME composition in the composer.
- Selecting the same forum row again while a topic is open also goes back to the list.
- Selecting another chat sends `CloseChat` and `CloseForum` as needed.
- The UI changes the pane only after the command is admitted (the Stage 1 `Admit` rule, task 6j).
- `ForumInvalidated` drops the UI's list. If the chat is still a forum, the pane waits for `TopicsReset` (with a "Loading topics…" placeholder). If not, it switches to `Thread({chat, None})`. The kind comes from the chat's summary, or from `KindChanged` for a shown chat the list doesn't hold; a chat that became a forum switches to `Forum { open: None }` the same way. A refused switch retries with the Stage 1 backoff. Drafts of threads an event makes unsendable are discarded when the event arrives, whatever the pane shows or whether a command is admitted: `TopicRemoved` with `reason: Deleted` drops that topic's key (a hidden General can still be posted to, and an evicted topic still exists: their drafts stay; an open hidden topic's text is stashed, or, with no slot free, goes to the notice), `KindChanged` every topic key of a chat that stopped being a forum or the whole-chat key of one that became a forum; the composer's text goes with them when it belongs to one. Nothing unsendable is stashed, so draft slots stay at 64 with no exception. One notice holds every discarded text under its label ("a topic of “X”", "“X”"), with one line per batch and **Copy text** (all entries, newest first). It keeps at most 64 texts, dropping the oldest: that and Dismiss are the only ways a discarded text is lost. It is cleared at account teardown, and no text reaches logs (task 10 review).
- `TopicRemoved` for the open topic (`Deleted` or `Hidden`; the cap never evicts it) goes back to the list with a notice. `Evicted` only drops the row.

**Topic list.**
- A `uniform_list` of fixed-height rows: a neutral "#" glyph (D3), the title, a preview (the Stage 1 100-character rule), the time, the unread mark, a pin mark for pinned topics, and a "closed" mark.
- **The unread mark** shows the server's count, adjusted only for display by `freshness`, in the theme's ink (no colour, per the brand):
  - `Current` or `Stale`: the count, when it is above 0;
  - `NewSince`: the count when above 0, otherwise a small dot meaning "new since last count";
  - `ReadSince`: nothing, because the user has just read the topic and the refetch will confirm it.
- The header shows the chat title and kind label. The `FORUM_TOPICS` placeholder copy ("Topics arrive in a later version.") is removed.
- Scrolling within 10 rows of the end sends `LoadTopics` when `has_more` is set and nothing is loading. A failed page shows the Stage 1 retry button and backoff.
- **Flood wait on topics.** `LoadEnded { Topics, Some(FloodWait { seconds }) }` is not counted as a failure by the Stage 1 `Backoff` (`chat/retry.rs`, `MAX_FAILURES = 5`).
  - The UI **keeps the load intent** (first page or next page) until the deadline and shows a muted countdown: "Telegram asked to wait. Loading again in N s." It replaces the loading placeholder on a cold open or an invalidated list, and sits as a footer under a retained list.
  - **At expiry** it sends the kept intent once. If that attempt fails, the normal Stage 1 retry button and backoff take over, with manual Retry after they give up.
  - Navigation is never blocked. Leaving the forum drops the intent and the countdown. Coming back during the wait shows the countdown again from core's deadline (core answers `FloodWait` with the remaining seconds).

**Topic thread.**
- The Stage 1 message view, header "‹ Topics", then the chat title and topic title, with the "closed" mark when closed.
- Rows don't show "reply" quotes for the topic root (already guaranteed by `reply_and_topic`).
- **Composer:** when the open topic is closed and the chat's `can_post_closed` is false, the composer is replaced by a muted line, "This topic is closed." Existing echoes still show and can be discarded. The composer is also disabled when the chat's `can_send` is false (Stage 1). The closed state comes from the retained open topic, which every refresh path re-reads (section 6.5).
- **Drafts, echoes, revealed spoilers, `read_sent`, `viewing`** are keyed by `ThreadKey`. Draft and echo caps are unchanged and count across threads.
- `MarkRead` and `ViewingTail` carry the thread. A topic's tail visible counts only for that topic.

## 9. Budgets and caps

| What | Cap | On overflow |
|---|---|---|
| Retained topic lists | `MAX_TOPIC_LISTS = 6` (LRU; the shown one is never evicted) | evict the least recently used |
| Topics per forum | `MAX_TOPICS_PER_FORUM = 500` (+ the open topic) | stop paging, show "topic limit for this view" |
| Topics page | `TOPICS_PAGE = 50` (`limit`) | — |
| Topics / peer-dialogs response | `limit + RESPONSE_SLACK` items, `2 ×` that messages, `MAX_RESPONSE_PEERS` peers | call fails `Oversized` |
| By-id request | `MAX_TOPICS_BY_ID = 20` ids | split into later calls |
| Due topics (required refetches) | `due` map keyed by id, ≤ 501; drained in id order, 1 reserved slot for the open topic + 19, backoff 0 s → 30 s; any due topic within 27 calls | none: never dropped |
| Discovery (unknown ids) | `MAX_DISCOVERY = 64` per list, fills leftover room in calls | clear, mark `needs_first_page` |
| Automatic first-page topics refreshes | `AUTO_REFRESH_COOLDOWN = 30 s`, account-wide; one trailing request; flood waits extend it | coalesce |
| By-id calls | shown forum only, 1 in flight, `TOPICS_CALL_INTERVAL = 1 s` apart | wait |
| In-flight topic fetches | 2 per list (one page, one by-id), each record `{seq, kind, ≤ 20 ids}` | queue (one page queued at most) |
| Tombstones | `MAX_TOMBSTONES = 64` per list | invalidate every in-flight fetch older than the tombstone, re-queue it, then discard the tombstone |
| `peer_dialogs` calls | 1 in flight, `PEER_CALL_INTERVAL = 3 s` apart per account, ≤ 20 chats, kind checks first | wait; flood waits pause the scheduler |
| Badge dirty stamps | one per retained dialog (≤ `MAX_DIALOGS`), drained in `ChatId` order with a cursor, 3 s per chat | none: never dropped; cleared only by a later-dispatched authoritative result |
| Flood-wait deadlines | `topics_not_before`, `peer_not_before`, account-level, only extended | wait; kept across navigation, backoff resets and rebuilds |
| Kind checks (fenced chats) | FIFO, deduplicated, only chats in the required-peer set: ≤ `MAX_PINNED_PEERS` (1,264), with every per-chat mark; one recheck per fence, abandoned on the third cumulative failure, at most 4 completed attempts (rebuild-discarded attempts excluded) | none: never dropped; retired when the chat leaves the set |
| Deferred loads of fenced chats | `loads::MAX_DEFERRED` | `LoadEnded { error: Some(Busy) }` |
| Topic title | `MAX_NAME_BYTES` (256), sanitized | `clean` truncates at a char boundary |
| Topic preview | `MAX_PREVIEW_BYTES` | as chats |
| Read marks | `MAX_DIALOGS + MAX_TOPICS_PER_FORUM` keys | `Busy` |
| Windows | `MAX_WINDOWS = 8` threads, `MAX_STORE_BYTES` | Stage 1 LRU |
| Cache | per-chat `MAX_CACHED_PER_CHAT` and the global budget, counted across all topics of a forum | Stage 1 eviction |
| Event bytes | `Topic::retained_bytes` counted in `Event::retained_bytes` | sender waits (S1 §6) |

Topic lists are not counted in `MAX_STORE_BYTES`. The budget is **retained payload capacity**: every container's allocated capacity (not its length), B-tree storage at its worst-case node count, 32 bytes of allocator overhead per heap allocation, and the outer `Topics` map and LRU. Allocator fragmentation and transient peaks inside one operation are not counted. A unit test asserts the bound from the constants: 6 lists × (501 entries × (inline entry with stamps and inline due state + title ≤ 256 B + preview ≤ 512 B + two allocations) + 64 tombstones + 64 discovery ids + 20 in-flight ids), plus the outer containers, the badge dirty stamps (≤ 1,000) and the kind-check FIFO with its marks (≤ 1,264 chats), comes to about 3.48 MB, at least 10% under 4 MiB. A second test drives full lists through paging, refreshes, discovery and open-topic churn, and checks their measured capacities against that bound. Capacity growth is released after each change (the entries vector is shrunk back to 501, strings to their length).

## 10. Encrypted cache

**Schema version 4.**
- `messages` gains `topic INTEGER NOT NULL`: 0 for `topic: None`, otherwise `TopicId::get()`, which is ≥ 1, so 0 can't collide.
- The primary key stays `(kind, chat, id)`, since ids are unique per chat. A new index `messages_thread ON messages (kind, chat, topic, id)` serves thread pages.
- **Migration 3 → 4:** delete all messages and `cached_chats` rows, then recreate. Version 3 rows have unnormalized topics (`route` didn't exist), and the cache is disposable (S1 §16). Chats survive. (Migrations 4 → 5 and 5 → 6, from [folders.md](folders.md) parts A and B, then drop the chat list, so a version 3 cache ends at the current version with neither.)

**Thread-scoped operations.** Everything that reads, extends or replaces a run of history takes a `ThreadKey` and filters on `topic`:
- `tail`, `before`, `after` (pages of one thread).
- `put_messages(thread, msgs)`: messages of other threads are skipped, as other chats' are today.
- `reconcile_range(thread, lo, hi, page)`: deletes only *that thread's* ids in `[lo, hi]` that the page lacks. Other topics' messages interleave by id and must survive.
- **`CacheCmd::ReconcileTail { thread, messages, from }`** and its worker function (`worker.rs:358-383` today) are re-keyed as a whole:
  - The **overlap query** is `cache.tail(thread, 1)`: the newest cached message *of that thread*, not of the chat.
  - **Overlap** (the server tail's first id is at or below that newest id, or the tail reaches the thread's start) reconciles `[first, MAX]` for that thread only.
  - **No overlap** (a possible gap) **replaces that thread's rows only**: `reconcile_range(thread, MIN, MAX, page)`. Sibling topics' rows are untouched.
- `CacheCmd::Reconcile`, `PutMessages`, `Tail`, `Before` and `After` carry `ThreadKey`.
- `Delete(chat, ids)`, `DeleteUnscoped`, `Touch(chat)` and LRU eviction stay per chat. Deleting and evicting never make a run look longer than it is.

**Validation.**
- `decode_message` checks the row's `(chat, topic)` against the decoded `thread`, in addition to `id` and `is_valid()`. A decode failure (including a `TopicId` ≤ 0 inside the body) or a mismatch is a bad row: deleted and skipped.
- Writes happen only for `Route::Thread` messages, and live writes only for contiguous threads (section 6.2). So neither an unnormalized key nor an unconfirmed run reaches disk.

**Example the design rules out.**
- Topic A's cache ends at 100. The user leaves A. Messages 101–199 arrive while A has no window: none is cached (no contiguous window). Message 200 arrives: also not cached.
- On reopen, the server tail is, say, 150–250. It is checked against A's cached tail (100). There is no overlap, so A's rows are replaced by 150–250, and topic B's rows are kept.
- Nothing can make 100 and 200 look adjacent.

Topic lists are not cached in Stage 2 (D5).

## 11. Tests

**Unit and proptest (core):**
- **`TopicId`.** `TryFrom` rejects 0, −1 and `i32::MIN`. Serde decoding of 0 and −1 fails. `GENERAL.get() == 1`. A malformed forum header (`reply_to_top_id = 0`, `-5`) drops the message in `map::message`. A `forumTopic` with id 0 is skipped. These are runtime checks, not `debug_assert`s.
- **`route`.** For every combination of forum knowledge and raw topic, the result follows section 3. A forum key always has `Some(topic)`. Normalizing a normalized message is the identity (proptest).
- **`reply_and_topic`**, extended: General (no header), a topic post, a reply inside a topic, a reply to another chat, a `TopicCreate` service message, and malformed ids.
- **`TopicList` ordering** (proptest, both `TopicOrder` modes, random sequences of reset, append, bump, edit, pin refetch, remove, by-id results):
  - no duplicate ids;
  - pinned topics form a prefix;
  - in `LastActivity`, the unpinned part only reorders on `last_id` growth;
  - in `Created`, it never reorders on messages, and by-id inserts land by `created`;
  - `len ≤ MAX_TOPICS_PER_FORUM + 1`;
  - every title `is_clean`;
  - hidden topics are never emitted;
  - an order change on a later page triggers a reset.
- **Merge policy** (proptest, pure `TopicList`). Random interleavings of dispatches, completions in any order, live changes and triggers. Checks:
  - a group is applied iff the result's seq exceeds both its `seen` and `changed`, whether the server state the snapshot saw includes **none, a prefix, or all** of the crossing changes;
  - an applied snapshot is authoritative even when it lowers `last_id` or a count;
  - a topic's unread count only ever holds a value some snapshot sent (no increments anywhere);
  - every trigger leaves the topic due, and a due topic is served within 27 drain calls once its backoff expires, **even while the display order changes continuously** (bumps, pins, snapshots) and **while the open topic is continuously due** (it takes the reserved slot each call; the rest progress at 19 per call);
  - a tombstoned topic is never re-inserted by an older result, also after tombstone overflow (the older fetches are invalidated first);
  - removal happens only on a fresh by-id `deleted`, with or without a prior suspicion;
  - visibility after a fresh find comes from that result (hidden → suspected → found-hidden stays hidden).
- **Automatic-refresh limiter** (pure): repeated overflow and pin triggers across several lists and many completed calls produce at most one first-page fetch per cooldown, plus one trailing; a flood wait extends the cooldown.
- **Badge dirty bits and slot split** (pure): a bit is cleared only by a result dispatched after it was set. With the 10/1/9 split and spill-over, the drain reaches every dirty non-shown chat within `ceil(n / 9)` calls, and every kind check within `ceil(m / 10)` calls, whatever the arrival pattern. Flood-wait deadlines only ever extend.
- **Reducers:**
  - an edit of an old message changes neither unread nor preview nor order;
  - an edit of the top message updates the preview only when its version is newer;
  - an edit never inserts a message;
  - a new message with `id <= last_id` doesn't move activity;
  - no reducer changes an unread count.
- **Forum badge** (proptest over messages, local reads, remote topic reads, chat-level reads, dialogs pages, `peer_dialogs` results, in random order and with crossing completions):
  - the badge only ever holds a value some authoritative source sent;
  - a source dispatched before the shown value's source never replaces it;
  - every trigger is followed by a refresh (or the overflow fallback);
  - after the last refresh, the badge equals the server's value.
- **`Windows` keyed by thread** (proptest): every message in a window has `thread == key`, and a mismatching insert is rejected at runtime. A chat-level delete removes the ids from every thread window of the chat and from no other chat.
- **Cache:**
  - thread pages return only their thread;
  - `reconcile_range` on one thread never deletes another's rows (proptest over interleaved topics);
  - `ReconcileTail` with overlap only touches its thread;
  - `ReconcileTail` without overlap (the section 10 example) replaces only its thread and keeps the sibling topic's rows;
  - migration 3→4 drops messages and keeps chats;
  - rows whose stored topic mismatches the body, or whose body has `TopicId(0)`, are deleted.
- **Budgets:** the topic-list memory bound from the constants (entries, tombstones, discovery ids, in-flight records, the badge queue, the kind-check FIFO). `Event::retained_bytes` grows with topic titles.

**Actor against the fake (`actor/tests/topics*.rs`):**
- **Opening a forum:**
  - first page, then paging to the cap;
  - a stale page after close is dropped;
  - LRU eviction of lists;
  - instant `TopicsReset` on reopen;
  - both orders, including a forum that switches order between pages.
- **A topic thread:** tail from cache then server, older and newer paging, `Reconciled`, invalidation on rebuild.
- **Cache continuity:**
  - a closed-thread gap: cache to 100; close; live 101–200; reopen with a server tail that overlaps 200 but not 100. The cache must not bridge the gap, and no message between 100 and the tail's start is served from cache.
  - Reconciling topic A, with and without overlap, preserves topic B's cached rows.
  - Live messages into a thread with no window are not cached.
  - **Fence gap:** a thread's cache holds up to 100 and its window is contiguous. The chat is fenced; live 101–199 are routed `ChatOnly`; the check returns the *unchanged* kind; live 200 arrives after the fence lifts. 200 must not be written as contiguous. The window lost the flag on fence entry, the open thread reloaded its tail, and only that tail's `ReconcileTail` (no overlap with 100, so a replace) restored continuity.
- **Live routing:**
  - into a known topic: bumped, `NewSince`, due, the count unchanged until the refetch;
  - into the open topic's visible tail (no trigger);
  - into General without a header;
  - into an unknown topic (discovery, a by-id fetch, then `TopicsAppended`, placed by order mode);
  - 100 unknown topics at once (discovery overflow, then a first-page refetch; no required work lost);
  - a `TopicCreate` service message;
  - a message for a chat of unknown forum status (chat bump only, nothing cached).
- **Crossing responses** (fake `Delay` on `Topics`/`TopicsById`, with live updates during the delay; the delayed snapshot sees none, a prefix, or all of them):
  - a by-id snapshot crossing a close then a reopen (kept live, refetched, settles reopened);
  - crossing a hide;
  - crossing a root deletion (suspected, not removed until a fresh by-id result says deleted);
  - **a snapshot that already includes message 101 completes, then the live 101 arrives** (finding 1: the count stays 1, `NewSince`, and the refetch returns 1; never 2);
  - then a later snapshot with an absolute read count (applied as is);
  - crossing a local read then message 101;
  - a page dispatched before a by-id fetch but completing after it (doesn't regress the by-id state);
  - 65 confirmed deletions while an old page is in flight (tombstone overflow invalidates the page first);
  - a dropped non-PTS update corrected by the next drain or light refresh.
- **Top-message deletion** (with an older snapshot in flight each time):
  - delete the latest message 200: the topic becomes due; the old snapshot (still showing 200) is a conflict and is skipped; the refetch sets 199;
  - delete the last remaining message: the refetch sets the server's state (no top message, or the root), and a lower `last_id` is accepted.
- **Draining:** 80 due topics (suspected, conflicted or triggered) beyond page one of the shown forum, while live messages keep reordering the list and the open topic is due on every call. Each is served within 27 calls after its backoff, calls are never closer than `TOPICS_CALL_INTERVAL`, and discovery overflow during the drain loses no due topic.
- **Repeated overflow:** discovery overflows in the shown forum and in two retained lists, repeatedly across several completed calls. At most one automatic topics page is fetched per cooldown, plus one trailing. A flood wait extends the cooldown.
- **By-id flood wait** (round 5): a 60 s `FLOOD_WAIT` on a by-id call while topics are due, followed by a switch to another forum and back, a light refresh and a rebuild. No automatic topics call is dispatched before the deadline. A user `OpenForum` inside it is answered with `FloodWait` at once. The drain resumes after expiry.
- **Deletion counts:**
  - deleting a non-top unread message held in a window: the topic becomes due, a badge refresh is scheduled, and both settle to the server's values;
  - deleting the last unread message of a topic with no read update ever delivered: the refetch gives unread 0, and the badge refresh drops the forum's badge;
  - deleting ids no window holds: every listed topic becomes due, zero counts included, and the badge refreshes;
  - **the crossing deletion** (round 5): topic T stores 0; incoming messages make T due; a by-id snapshot is dispatched and captures 2; a non-top message held by no window is deleted before the response arrives. The response is a conflict for T and doesn't apply. T stays due, and the next refetch gives the server's 1.
- **Edits:** an edit of an old message in a topic (no unread change, no preview change, no move); an edit of the top message (preview updated); a stale edit (ignored by the version check).
- **Forum badge:**
  - messages into loaded topics, unloaded topics, and with no list (no local change, one coalesced `peer_dialogs` per chat per interval);
  - a topic snapshot that already contains message 101 crossing the live 101 (the badge comes from the refresh);
  - **a delayed read** (finding 4): the read is slow, a badge refresh scheduled by something else returns the old badge first, and no read update is ever delivered. The read's completion schedules a new badge refresh, which shows the new value; admission alone schedules nothing;
  - a failed read (no refresh scheduled, the key retried);
  - a chat-level read before a topic-level read, and the reverse (same final badge);
  - a `peer_dialogs` result dispatched before a chat-level read update (ignored);
  - **badge overflow beyond page one** (round 5): 50 forums beyond the first dialogs page get a remote read each, then nothing else happens: no later updates and no navigation. Every one is refreshed by the dirty-bit drain within `ceil(50 / 9)` calls (fewer with spill-over). None is dropped, and no dialogs page is substituted;
  - a trigger crossing an in-flight `peer_dialogs` for the same chat (the bit survives the result, and the chat is refreshed again);
  - calls never closer than `PEER_CALL_INTERVAL`, and a flood wait pauses the scheduler.
- **Forum flag:**
  - on and off while a thread is open;
  - **a chat beyond the first dialogs page becoming a forum** (a `peer_dialogs` check, the fence holds history and cache work, then `ForumInvalidated`);
  - a failed check (retries, abandonment, `suspect`, continuity still revoked, re-check on the next refresh);
  - 40 dirty badges with 3 fenced chats: the 3 kind checks complete, every fenced chat's deferred commands are answered, and every badge is refreshed;
  - **sustained kind checks** (round 6): new fences keep the kind-check FIFO non-empty on every call, every check succeeds, and the shown forum is dirty on every call. An unrelated dirty forum beyond page one is still refreshed within `ceil(n / 9)` calls through the cursor slots;
  - **40 chats beyond page one fenced at once** (finding 5), with repeated triggers for some and a rebuild in the middle. Every chat receives its first attempt within `ceil(40 / 10)` calls under badge contention (10 guaranteed kind slots), or `ceil(40 / 20)` with full spill-over when no badge is dirty; rechecks and the post-rebuild re-dispatch are counted separately and asserted as such. Re-triggered in-flight chats are checked exactly once more. No reconcile is triggered, and every deferred command is answered (a deferred load beyond `MAX_DEFERRED` gets `Busy` at once);
  - a rebuild while a kind check is in flight (its chats re-dispatched first once the baseline is established);
  - **fence budgets** (round 9): the traces `fail → fail → success with a recheck pending → success` and `… → third failure (abandoned)`. Assert at most 4 completed attempts, that a trigger during the recheck gets no second recheck, and that a rebuild mid-trace neither resets nor consumes either budget;
  - **a rebuild with the open chat fenced beyond page one:** bootstrap completes without the fenced tail; the kind check runs right after the baseline; on completion the tail reloads. The same with the check failing three times: abandonment releases the fence, windows are invalidated, deferred commands run, and the tail reload restores continuity. Bootstrap never waits on the fence;
  - **membership churn while dispatch is paused** (a flood wait): dialogs refreshes and paging remove and admit fenced chats, and windows are evicted. The FIFO and every per-chat mark stay within the required-peer set. Chats that left have no state left, and their deferred commands are answered. An in-flight result for a retired chat is ignored. A chat that re-enters starts fresh.
- **Open topic beyond page one:** it is fetched by id when opened. A missed close then reopen across a rebuild is corrected by bootstrap's by-id fetch and the composer unblocks. A light refresh corrects a stale `closed`.
- **Deleted while offline** (finding 6): the open topic is deleted while the app is offline, and no root-deletion update arrives. Bootstrap's by-id fetch returns `deleted`, which is fresh, so the topic is removed, the thread invalidated, and the UI returns to the list.
- **Topic edits:** title and closed while open, close then reopen ordering across bootstrap, hidden General, and hidden → suspected → found-hidden (finding 7: stays hidden).
- **Snapshot visibility** (round 7): the grammers mapping and the fake map `forumTopic.hidden` into `TopicSnapshot.visibility`; a cold load with General initially hidden never emits it; a missed hide and a missed unhide are both corrected by the next page or by-id snapshot.
- **Deletion** of a topic root (suspicion, then by-id confirmation, then `TopicRemoved`; the window is invalidated, cached rows are gone), a root deletion the by-id result contradicts (stays listed), and of an ordinary message in a topic.
- **Sending:**
  - into a topic, with the correct `reply_to` thread recorded by the fake;
  - into General;
  - into a closed topic (refused locally, and refused by the server with a stale list, followed by a by-id refetch);
  - a send surviving a rebuild into the same topic with the same `random_id`;
  - a send to a fenced chat waiting for the fence.
- **Read marks:** coalesced per thread; completion carries the thread and schedules the topic refetch (`ReadSince`) and the badge refresh; `ReadInbox{topic}` and `ReadTopicMax` are triggers.
- **Faults** on `Topics`, `TopicsById` and `PeerDialogs`: errors give `LoadEnded` and retry; a hang is bounded by the call deadline; a panic is contained.
- **Bootstrap:** a topic edit and a new topic message arriving before, during and after the topics snapshot and the open topic's by-id snapshot.

**Grammers mapping (hand-built TL, like `backend/grammers/tests/map.rs`):**
- `forumTopic` to `Topic`, including the title sanitizing, `title_missing`, the General fallback, `created`, a deleted topic, invalid ids, and an oversized response.
- `order_by_create_date` to `TopicOrder`, and the cursor in both modes.
- `getReplies` request fields for a topic, and General per U1.
- `sendMessage.reply_to` for a topic, General and a non-forum chat.
- `readDiscussion` fields.
- `getPeerDialogs` request and mapping, including a forum flag change.
- Each new update mapping, including a broadcast `ReadChannelDiscussionInbox` being dropped and non-positive topic ids being skipped.

**App (GPUI test context, like `chat/tests.rs`):**
- Pane transitions: forum → list → topic → Back → list, and selecting another chat while a topic is open.
- Commands are sent only after admission. Drafts and echoes stay with their thread across navigation.
- The composer is replaced in a closed topic and returns on reopen.
- `ForumInvalidated` in both directions. `TopicRemoved` for the open topic.
- Paging triggers near the end and backs off after failure.
- **Flood wait on topics** (round 6), for a cold open (no retained list), an open of a retained list, and a list invalidated during the wait. In each:
  - the loading state clears, and the countdown is visible;
  - selecting other chats and coming back works, and the countdown reflects core's remaining time;
  - at expiry exactly one request for the right page (first, or next) is sent;
  - a failure of that request shows the Stage 1 retry button;
  - the flood wait never consumes `Backoff` failures.

## 12. Ordered tasks and acceptance criteria

Each task is one implementation and review cycle on `agent/stage-2`, recorded in the review log. Every task ends with the checks in the roadmap's "How work is done": fmt, clippy with and without `fake`, tests with and without `ABYSS_TEST_STORE_DELAY_MS=50`, and no gpui in `cargo tree -p abyss-core`. On a Linux host, source `~/.local/opt/abyss-env.sh` first and run clippy as `cargo clippy --workspace --all-targets --features fake -- -D warnings -A dead_code -A clippy::unnecessary_wraps`; the macOS-only checks run on macOS before pushing.

1. **Re-key by `ThreadKey`** (no behaviour change).
   - Add the validated `TopicId` (private field, `TryFrom<i32>`, serde `try_from`, `GENERAL`). Change `ThreadKey.topic` to `Option<TopicId>`, and make `reply_and_topic` drop messages with malformed ids.
   - Re-key the commands, events, `Windows` (including `cache_contiguous` per thread), the actor's history and read maps, pending sends, covers, the cache API and `CacheCmd`s (`ThreadKey` arguments, still ignoring topic in SQL), and the app's selection, drafts and echoes.
   - Topic is always `None` in **keys**: commands, events, windows, the actor's maps, pending sends and cache arguments use `ThreadKey { chat, topic: None }`. Message **payloads** keep the raw topic the server stated (`Message.thread.topic` may be `Some` for forum messages); core files every message under its whole chat (`apply::shown_in`) without rewriting it.
   - So the section 3/6.2 invariant (a message's `thread` equals its window's key) does **not** hold yet. Normalization through `route` and the window/event key-equality enforcement are deferred to task 7; the cache's row-key and `TopicId` enforcement to task 5.
   
   *Done when:*
   - every existing test passes unchanged apart from the renames;
   - the `TopicId` validation tests pass;
   - the fake demo behaves as before;
   - the diff contains no other new behaviour (the reviewer checks this explicitly).
2. **Topic model, ordering and merge policy** (pure code). `Topic`, `TopicOrder`, `Freshness`, `Visibility`, `Deletion`, `route`, and `TopicList` with ordering per mode, caps, the clock, per-group stamps and the merge rule, due state with backoff and the round-robin drain, disposable discovery, tombstones with overflow invalidation, and the new-message and edit reducers. Also the pure badge stamp ordering of section 7. *Done when:* the proptests for `route`, `TopicList` ordering, the merge policy, the reducers and the forum badge pass, including the round-3 finding 1, 2 and 7 counterexamples as fixed cases, the drain under continuous reordering and a continuously due open topic (≤ 27 calls), the automatic-refresh limiter, the badge dirty bits and the round-5 crossing deletion as a fixed case, and the memory-bound test passes.
3. **Backend trait and fake.** Add `topics`, `topics_by_id`, `peer_dialogs` and the new `Update` variants. Give the re-keyed `history`/`send_text`/`read_history` topic behaviour in the fake. Add the fake forum data in both orders, the controls (including `drop_next_update` and `set_topic_order`), the ops and faults, demo forums, and contract cases. *Done when:* the contract suite passes against the fake, and `cargo dev` data includes forums with paging-sized topic lists (not yet shown).
4. **Grammers implementation.** Raw `getForumTopics` (both orders, cursor per mode), `getForumTopicsByID`, `getPeerDialogs`, `getReplies`, `readDiscussion`, `sendMessage.reply_to`, the response size checks, id validation, and the new update mappings, with U1/U2 behind one function each. *Done when:* the mapping and request tests in section 11 pass, it builds in the production configuration, and the log tests show no title leaking.
5. **Cache schema v4.** The topic column, index and migration; thread-filtered queries; thread-scoped `reconcile_range`; **`ReconcileTail` re-keyed with its overlap query and replace scope per thread**; the row key and `TopicId` check. *Done when:* the cache tests in section 11 pass (including the section 10 example as a test, sibling-topic preservation, and the proptests), and the existing cache tests pass. This task takes over the cache-side enforcement task 1 deferred: rows checked against the decoded `thread` and its `TopicId`.
   - **Whole-chat keys until task 7.** A row's `topic` column comes from the message's own (decoded) `thread`, so a forum message the actor still files under `ThreadKey::whole(chat)` with a raw `Some(topic)` is stored under that topic. A topic key addresses only its topic's rows. A **whole-chat key addresses every row of its chat, topics included**: pages, `put_messages`, `reconcile_range`, and `ReconcileTail`'s overlap query and replace. That is exactly today's behaviour for the forum stream `apply::shown_in` builds (its cached tail, gaps and reconciles stay chat-wide), and for a non-forum chat, whose rows all have topic 0, it equals `topic = 0`. Filtering a whole-chat key on `topic = 0` instead would hide a forum's cached topic rows from its whole-chat window and make its tail reconcile skip them, a behaviour change before task 7.
6. **Core topic lists.**
   - `OpenForum`/`CloseForum`/`LoadTopics`, `TopicsGen`, the paging events, LRU.
   - Applying page and by-id results through the merge policy; the by-id drain (shown forum only, open topic first, rate-limited); fresh by-id deletions.
   - Bootstrap, reconcile and light-refresh additions, including the open topic by id.
   
   *Done when:* the "opening a forum", "crossing responses", "top-message deletion", "draining" (with reordering and a continuously due open topic), "repeated overflow", "by-id flood wait", "open topic beyond page one", "deleted while offline" and bootstrap actor tests pass, and a stale page is provably dropped. Paging also stops when a non-empty page adds no new topic to the list (duplicates only), so an inflated server `count` can't drive redundant requests (task 4 review); test it with the fake returning a repeated page.
7. **Core topic threads.** Opening a topic thread, history, the cache per thread under the continuity gate, `Reconciled`, invalidation, and normalization of everything entering windows and the cache through `route`. *Done when:* the "topic thread" and "cache continuity" actor tests pass, and the thread-equals-key invariant holds under the existing history proptests run with topics. This task replaces task 1's temporary `apply::shown_in` (whole-chat keys, raw message topics) with `route` normalization, and enforces key equality for windows and events. It also narrows the cache's whole-chat scope (task 5: a whole-chat key spans its chat's topics; `db::addresses` and `db::Rows`) to `topic = 0`, so a whole-chat key addresses exactly the thread `{ chat, topic: None }` and the cache rejects a mismatching message as section 3 requires. After normalization no forum is addressed by a whole-chat key, and a kind change deletes the chat's rows (section 6.6), so only the scope rule and its tests (`a_whole_chat_key_spans_the_chats_topics`, the whole-key case of `reconcile_range_never_touches_another_thread`) change.
8. **Core live routing, forum badge, forum flag.**
   - Routing and topic bumps; unknown-topic by-id fetches with caps; General; topic edits and pins; root deletion.
   - The badge queue and section 7's set-only badge.
   - The `peer_dialogs` scheduler (`PEER_CALL_INTERVAL`, `peer_not_before`) and the badge dirty-bit drain; the kind-check FIFO (required-peer set only, retirement when a chat leaves it, priority in batches, recheck marks, abandonment, dispatch after the baseline) and the fence, including continuity revocation, deferred commands, exclusion from bootstrap's awaited tails and the reload on lift.
   - Deletions as count invalidations.
   
   *Done when:* the live-routing, edits, deletion, deletion-counts, forum-badge, fence-gap and forum-flag actor tests pass, including the chat beyond page one, badge overflow beyond page one with no later updates or navigation, sustained kind checks with an unrelated dirty forum, the crossing deletion, 40 dirty badges with 3 fences, the 40-chat fence test with rebuilds, the rebuild with the open chat fenced beyond page one (with and without abandonment), and membership churn while dispatch is paused.
9. **Core sending and read marks per topic.** The thread on pending sends, closed-topic refusal, `TopicClosed`/`TopicDeleted`, per-thread read coalescing, read completions carrying the thread (topic trigger and badge refresh only on success), remote reads as triggers. *Done when:* the sending and read actor tests pass, including the rebuild-survival, fenced-send and delayed-read tests.
10. **App: forum navigation and the topic list.** `Pane`, the topic list view, paging, the flood-wait countdown with its kept intent, Back (button and Cmd+[), and removing `FORUM_TOPICS`. *Done when:*
    - the pane-transition and flood-wait app tests pass;
    - in `cargo dev`, a demo forum lists 61 topics with the pinned ones first and pages smoothly;
    - the creation-ordered demo forum keeps its order while messages arrive;
    - Back is instant.
11. **App: topic thread.** Header, composer blocking, drafts, echoes, read and tail per thread, `ForumInvalidated`/`TopicRemoved` handling. *Done when:*
    - the remaining app tests pass;
    - in `cargo dev`, a send into a topic appears only in that topic;
    - a closed demo topic blocks the composer;
    - a new message into a read topic shows the dot, then the server count;
    - reading the last unread topic of a forum lowers the forum's badge within one `peer_dialogs` round trip.
12. **Live verification** (blocked on credentials, like Stage 1 task 12). The checklist in section 13. *Done when:* every item is checked off in the roadmap, with U1–U5 resolved, and any resulting code change has landed as its own reviewed task.

## 13. Unknowns needing live verification

- **U1:** how General's history is fetched (`getReplies(msg_id = 1)`, or `messages.search(top_msg_id = 1)`), and that `getReplies` paging matches `history_offsets` (including After).
- **U2:** how General is marked read (`readDiscussion(1)` or `channels.readHistory`), and whether `channels.readHistory` on a forum touches other topics.
- **U3:** that `dialog.unread_count` (also from `getPeerDialogs`) and the chat-level `still_unread_count` count unread topics for a forum (D1). Section 7 shows whatever the server sends, so a different meaning changes the copy and expectations, not the mechanism. It is recorded and decided then.
- **U4:** how topic deletion is signalled (the root message's deletion, `forumTopicDeleted` from by-id), and whether General can be hidden while it has unread messages.
- **U5:** which update carries a per-topic read from another device (`updateReadChannelDiscussionInbox` or `updateReadHistoryInbox` with `top_msg_id`), and whether the latter ever names a channel (it would hit the grammers assert, risk 2).
- `getForumTopics` in both orders: the cursor fields (especially `offset_id` in creation order), pinned topics on first pages only, and whether `order_by_create_date` is stable across one traversal.
  - Task 4 ends a traversal only on an empty page or when the topics listed so far reach `messages.forumTopics.count` (the cursor's `index` counts them); a short page continues. Check that `count` is the forum's total including pinned and hidden topics, and that `limit = 50` (clamped to 100) is accepted.
  - Task 4 builds the cursor from the last raw `forumTopic` of a page, including one skipped for a malformed id, and its `LastActivity` date from the raw top message (0 when the response lacks it). Check that a page whose last top message is missing still continues correctly.
  - `forumTopic.peer` (new in the pinned layer) is not checked against the requested chat. Check what it holds for a supergroup forum.
- `getForumTopicsByID`: whether an unknown or deleted id comes back as `forumTopicDeleted` or is left out. Task 4 reports only `forumTopicDeleted` as deleted; omitted ids are neither found nor deleted, so core keeps them due.
- `TOPIC_CLOSED`, `TOPIC_DELETED` and `CHANNEL_FORUM_MISSING` error names (task 4 maps them to `RejectReason::TopicClosed`, `TopicDeleted` and `NotForum` from Telegram's method documentation; grammers carries no error table), and posting in a closed topic as an admin (`can_post_closed`). `TOPIC_ID_INVALID` stays `Other`. `can_post_closed` is `Refused` when a topics response omits the channel entity; check that the entity is always included.
- `updateEditForumTopic` and `updatePinnedForumTopic(s)` arrive on the update stream for a member who didn't make the change.
- `updateChannel` arrives when a group is converted to a forum and back.
- A message sent from another device into a topic arrives with `forum_topic` and the right `reply_to_top_id`/`reply_to_msg_id`.
- `getPeerDialogs` at one call per `PEER_CALL_INTERVAL` (3 s) and `getForumTopicsByID` at one call per second stay clear of flood waits during busy forum traffic. Tune the constants from what is observed (risk 4).

## 14. Owner decisions (2026-11-17) and follow-ups

The owner accepted these defaults (formerly open questions Q1–Q5):

- **D1. Forum badge:** the forum's chat-list row shows the server's unread-topic count (section 7).
- **D2. Scope:** v1 forum support is read and post only. Creating, renaming, closing, pinning, hiding and deleting topics are out.
- **D3. Topic icon:** a neutral "#" glyph. No `icon_color` and no custom emoji.
- **D4. Opening a forum:** it always opens the topic list. `view_forum_as_messages` is ignored.
- **D5. Topic-list caching:** deferred, not part of Stage 2.

Follow-ups (not Stage 2 tasks):
- **Cache topic lists on disk** (D5): an encrypted `topics (kind, chat, topic, ord, body)` table with the same row validation, read on `OpenForum` before the server page so a forum opens instantly offline. Plan it when there is a reason to; it must reuse section 6.4's merge rule (a cached list is just the oldest snapshot).
- **Upstream issue for the grammers channel assert** (`A/:520-521`, risk 2).

## 15. Risks

1. **General's endpoints (U1, U2)** may need a different call than other topics. That is contained behind one function each in the grammers backend, and the fake can't tell us. Until live verification, General's history and read marks are unproven.
2. **grammers assert on channel `updateReadHistoryInbox`** (`A/:520-521`). If forums produce it, every such update panics the consumer. It's contained, but costs a rebuild each time. Mitigation: file the upstream issue now. If it reproduces, patch it in a pinned fork, or bump grammers once it's fixed upstream (a deliberate, reviewed bump).
3. **Lost non-PTS topic updates.** Topic edits, pins and topic reads can be lost without a trace. They are corrected only by the next snapshot that applies: the drain, the light refresh (activation, every 5 minutes, always including the open topic) or a reconcile. A very busy topic can keep conflicting; it then shows its last server count with the freshness hint until a refetch lands between messages (backoff up to 30 s). Like S1 risk 7, this is a residual risk.
4. **Count latency and request cost.** Figures are nominal: one `peer_dialogs` per `PEER_CALL_INTERVAL` (3 s, conservative, account-wide), excluding slow responses, contention and flood waits, which all stretch them. The shown forum's badge trails by about one interval plus a round trip. Another dirty forum is reached within `ceil(n / 9)` calls for n dirty forums: at most 111 calls, about 333 s nominal for 1,000. Kind checks take at most 10 slots per call, so they can't extend that. Topic counts trail by one drain call (1 s apart, shown forum only). Nothing is dropped under a burst; it is only delayed. Nothing establishes even these rates as flood-safe, so live verification checks them (section 13). No count ever shows a number the server did not send.
5. **`MAX_WINDOWS` shared with topics.** Switching between many topics evicts other chats' windows sooner. It's acceptable because windows reload from the encrypted cache. Revisit if live use shows churn.
6. **The topic id is a message id.** Deletion and hiding semantics around it (U4) are inferred from docs. A wrong inference shows a deleted topic until the next refresh, never a crash: by-id confirmation is required before removal.
7. **Fencing delays** history for a chat whose kind is being re-checked, by its wait in the FIFO (at least 10 chats per 3 s call, nominal) plus retries, and always costs that chat a tail reload (continuity is revoked). After 3 failures the check is abandoned and the chat is marked `suspect`. Badges keep their reserved slots during a storm of fences.
8. **Schema churn.** The `getForumTopics`, `getPeerDialogs` and `sendMessage` fields must be rechecked on every grammers bump, like S1 risk 11.

## 16. Review log

### Round 1 (verdict: needs rework)

| # | Finding | Resolution (v2) |
|---|---|---|
| 1 | P1: unconditional live cache writes in topics break history continuity; `ReconcileTail`'s overlap check and replace scope were per chat | Live writes are gated on `is_cache_contiguous(thread)` as today (section 6.2). `ReconcileTail` is re-keyed as a whole: the overlap query is the thread's cached tail, and a gap replaces that thread's rows only (section 10). Tests: the closed-thread gap and sibling-topic preservation (sections 10, 11; task 5, 7) |
| 2 | P2: topic pagination ignored `order_by_create_date` | `TopicOrder` and `Topic.created` are carried through mapping, cursor and list. The cursor is built per mode (creation mode uses `forumTopic.date`). Live re-ordering is defined per mode: only `LastActivity` moves topics. An order change resets the list (sections 3, 5, 6.1). Tests in both modes |
| 3 | P2: same-generation topic fetches could overwrite newer live state, and the "newer edit will arrive" claim was unsupported for non-PTS updates | One merge policy for every snapshot: a per-list `FetchSeq` orders page and by-id results whatever their completion order, with per-topic `seen` and tombstones; plus a bounded per-fetch replay log of live changes, idempotent by message id and field sets (section 6.4). The convergence claim is removed, and lost non-PTS updates are a stated residual risk corrected by snapshots (risk 3). Tests: crossing close/reopen, hide, delete, read, and page-versus-by-id reordering |
| 4 | P2: an unread-topic badge can't be maintained from the retained subset | The badge is set only from authoritative sources and changed locally only on observed 0↔unread transitions of known topics. Unknown state schedules a bounded, coalesced `peer_dialogs` refresh. Remote topic reads never adjust the badge. Topic snapshots never change it. `base_top` prevents double counting, with one follow-up after an overlapping fetch (section 7). Tests: unloaded topics, no list, both read orders |
| 5 | P2: a first-page dialogs refresh can't resolve the forum flag of a chat beyond page one | A targeted `peer_dialogs([chat])` check (`messages.getPeerDialogs`, `api.tl:2678`), plus a fence: `ChatOnly` routing and no history or cache work for that chat until the kind applies, with bounded retries and a `MAX_FENCED` fallback to reconcile (section 6.6). Test: a chat beyond page one |
| 6 | P2: reconcile didn't refresh the open topic's metadata beyond page one, so a stale `closed` could block posting | Bootstrap, reconcile and the light refresh fetch the open topic by id when the first page lacks it. The open topic is kept retained past the cap, and a `deleted` result runs the deletion path (sections 6.1, 6.5). Test: an open topic beyond page one across a missed close/reopen and a rebuild |
| 7 | P2: `TopicId(pub i32)` admitted 0 and negative ids; `TopicId(0)` collided with the cache's `None` sentinel | Private field, `TryFrom<i32>` rejecting `<= 0`, serde `try_from`, `GENERAL` const. Malformed network ids drop the message or item; bad cached rows are deleted (sections 3, 10). Tests use real inputs, not `debug_assert` |
| 8 | P3: topic-list caching was still in the executable scope | Removed from sections 10 and 12. Recorded as decision D5 and a follow-up (section 14) |

### Round 2 (verdict: needs rework)

| # | Finding | Resolution (v3) |
|---|---|---|
| 1 | P1: fencing could recreate the continuity bug (cache 100, 101–199 dropped while fenced, 200 cached after an unchanged-kind lift) | On fence entry every window of the chat loses `cache_contiguous` (and its `ViewGen`). Lifting never restores it, even for an unchanged kind: windows are invalidated, the open thread reloads, and only its thread-scoped `ReconcileTail` sets the flag. General rule: any `ChatOnly` message for a chat with windows revokes their continuity (sections 6.2, 6.6). Test: that exact gap |
| 2 | P2: replay-above-snapshot is incompatible with absolute read counts | The replay log is removed. Field groups (meta, unread, monotone activity) with per-group `seen`/`dirty` stamps: a snapshot is never applied over a group changed after its dispatch; the group keeps its live state and is refetched by id (section 6.4). Tests: snapshots containing none, a prefix, or all of the crossing changes, including the read-then-101 case |
| 3 | P2: the second-overflow fallback let a stale snapshot overwrite observed live state | No log, no overflow, no fallback. A conflicting group is never overwritten. Refetches are bounded (3 per topic per refresh period with backoff, then `unsettled` until the light refresh) (sections 6.4, 9) |
| 4 | P2: replaying a root deletion removed the topic without confirmation | `Presence { Listed, Hidden, DeletionSuspected { since } }`. A root-message deletion only suspects. Removal and the tombstone need a by-id result dispatched after the suspicion that lists the id as deleted. A finding result clears the suspicion (sections 6.3, 6.4) |
| 5 | P2: `seen > 0` didn't prove a topic count matched the badge baseline, so transitions could be missed or stale reads could decrement | The badge is never adjusted locally. It is only set from authoritative sources, ordered by a dispatch stamp. Every possible change (messages, local and remote reads) schedules a coalesced `peer_dialogs` refresh, with one follow-up for triggers that cross an in-flight call (section 7). This is stricter than an epoch scheme, at the cost of a few seconds of badge latency (risk 4). Tests: crossing snapshots, stale local reads, both read orders |
| 6 | P2: kind checks shared the droppable badge queue, so a fenced chat could be stranded | A separate kind-check queue with 32 non-droppable slots, first in every batch. Completion and abandonment free the slot and lift the fence. A rebuild re-dispatches. Exhaustion runs a reconcile that releases every fence and answers deferred commands (section 6.6). Test: badge overflow with 3 fences |
| 7 | P2: tombstones had no cap | `MAX_TOMBSTONES = 64` per list. Before discarding one, every older in-flight fetch is invalidated and re-queued. Tombstones, pending ids, in-flight records and both queues are in the budget table and the memory-bound test (sections 6.4, 9) |
| 8 | P2: edits went through the new-message path (unread, preview) | Separate new-message and edit reducers (section 6.3). Edits keep Stage 1's replacement-only path and version check, never change unread or order, and update the preview only for the current top message with a newer version (`last_version`). Tests: edits of an old message and of the top message |

### Round 3 (verdict: needs rework; no P0/P1)

| # | Finding | Resolution (v4) |
|---|---|---|
| 1 | P2: a snapshot that already included message 101 set unread = 1, then the delayed live 101 incremented it to 2 | Topic unread counts are server values only, like the badge. Live messages, completed local reads and remote reads are triggers: they set a `Freshness` display hint and make the topic due for a by-id refetch, and never change the count (sections 6.3, 6.4, 8). Tests: the snapshot-then-message case, and a later absolute count |
| 2 | P2: activity wasn't monotone after a deletion, so a deleted top message's preview survived every snapshot | No monotone rule: activity has its own stamp, and a snapshot dispatched after the last live change is authoritative even when it lowers `last_id`. Deleting the current top message is a live activity change that makes the topic due (sections 6.3, 6.4). Tests: deleting the latest and the last remaining message with an older snapshot in flight |
| 3 | P2: required refetches were dropped with the 64-id pending set, so unsettled or suspected topics beyond page one could starve | Required work is state on the entries (`due`, with backoff up to 30 s), drained round-robin with a cursor, open topic first, ≤ 20 per call: any due topic is served within 26 calls. Unknown-topic discovery is a separate, disposable set. `unsettled` is gone (section 6.4). Test: 80 due topics beyond page one |
| 4 | P2: the badge refresh was scheduled at read admission, so it could return the old badge | Read completions carry `ThreadKey` and `max_id`. Only a successful completion schedules the topic trigger and the badge refresh; admission schedules nothing, and a failure schedules nothing (sections 6.8, 7). Test: a delayed read with an earlier badge response and no read update |
| 5 | P2: fence exhaustion had no progress rule (reconcile could loop, the 33rd chat had no slot) | Slots and the reconcile fallback are removed. Fenced chats wait in one deduplicated FIFO, bounded because only known chats are fenced (≤ `MAX_DIALOGS + MAX_WINDOWS`). It drains 20 per call with recheck marks, 3 tries then abandonment, and re-dispatch after a rebuild. Deferred loads use the existing `MAX_DEFERRED` cap, answering `Busy` beyond it (section 6.6). Test: 40 fenced chats beyond page one with rebuilds and repeated triggers |
| 6 | P2: the deletion rules contradicted each other for a topic deleted while offline | One rule: a *fresh* by-id `deleted` (dispatched after every live change and applied snapshot of the meta group) removes the topic, with or without a prior suspicion. A suspicion only makes the topic due. Pages never remove (sections 6.3, 6.5). Test: deletion while offline |
| 7 | P3: hidden and suspected couldn't coexist in one enum, and clearing a suspicion could expose a hidden General | Two enums: `Visibility { Shown, Hidden }` and `Deletion { None, Suspected }`. A fresh find sets both from the result (section 6.4). Test: hidden → suspected → found-hidden |
| — | Note: one `peer_dialogs` per second wasn't shown to be server-safe | `PEER_CALL_INTERVAL = 3 s` per account is an explicit, conservative constant, and the by-id rate is `TOPICS_CALL_INTERVAL = 1 s`, shown forum only. Flood waits pause the scheduler. Both stay on the live-verification list (sections 7, 9, 13) |

### Round 4 (verdict: needs rework; no P0/P1)

| # | Finding | Resolution (v5) |
|---|---|---|
| 1 | P2: the drain cursor indexed the display-ordered vector, so reordering could skip a topic repeatedly; the reserved open slot made the bound 27, not 26 | Due topics live in a `due` map keyed by `TopicId`, drained in ascending id order from a cursor id, independent of display order. One slot is reserved for the open topic and 19 are left for the rest, so any due topic is served within `ceil(500 / 19) = 27` calls (section 6.4). Tests: continuous reordering, and a continuously due open topic |
| 2 | P2: "known when enqueued" didn't bound the fence queue under dialogs churn, rebuilds and flood waits | Fencing is limited to the required-peer set core already maintains for `retain_peers` (≤ `MAX_PINNED_PEERS`). When a chat leaves that set, its queued and in-flight check is retired (the result is ignored), its marks are dropped, its fence is released (it has no windows) and its deferred commands are answered (section 6.6). Test: membership churn while dispatch is paused |
| 3 | P2: rebuild deadlock: the bootstrap waited for a fenced open tail while the kind check waited for `Online` | Kind checks are dispatched as soon as the baseline is established. A fenced open thread's tail is excluded from bootstrap's awaited tails and reloaded when the fence lifts, by any outcome (section 6.5). Abandonment is unchanged. Test: a rebuild with the open chat fenced beyond page one, with success and with abandonment |
| 4 | P2: deleting a non-top unread message changed server counts without a trigger, and deletions never refreshed the badge | Deletions in a forum always schedule a badge refresh. A deleted id held by a window marks its topic due; unassignable ids mark every listed topic with `unread > 0` due (bounded by the list) (sections 6.3, 7). Tests: deleting a non-top unread message, and deleting the last unread message without a read update |
| 5 | P2: overflow-driven first-page refreshes bypassed the rate limits | One account-wide limiter each for automatic topics and dialogs first-page refreshes: `AUTO_REFRESH_COOLDOWN = 30 s`, one coalesced trailing request, extended by flood waits; non-shown lists only mark `needs_first_page` (sections 6.4, 7, 9). Test: repeated overflow over several completed calls |

### Round 5 (verdict: ready after fixes)

| # | Finding | Resolution (v5.1) |
|---|---|---|
| 1 | P2: filtering unassignable deletions by the stored `unread > 0` missed a crossing snapshot (stored 0, the snapshot captured 2, a deletion before the response, which then applied 2 while the server had 1) | An unassignable deletion stamps **every** retained topic's unread group as changed and marks it due, zero counts included (bounded at 501). A snapshot dispatched before the deletion is then a conflict (section 6.3). Test: that exact crossing example |
| 2 | P2: badge overflow dropped work and substituted a first-page refresh, so a dropped chat beyond page one could stay stale forever | The droppable badge queue is replaced by a dirty stamp on each retained dialog (≤ 1,000), cleared only by an authoritative result dispatched after it was set, and drained in stable `ChatId` order with a cursor through the rate-limited `peer_dialogs` scheduler, after kind checks. This also subsumes the follow-up rule (section 7). Test: 50 forums beyond page one, no later updates, no navigation |
| 3 | P2: the by-id drain had no flood-wait deadline (`invoke.rs` returns waits over 5 s to the caller) | Account-level `topics_not_before` (and `peer_not_before` for `peer_dialogs`), only extended by `FLOOD_WAIT`, and preserved across navigation, list eviction, backoff resets and rebuilds. No automatic call is dispatched before it, and user loads inside it are answered `FloodWait` (sections 6.4, 7, 9). Test: a 60 s wait with topics due, a forum switch, a refresh and a rebuild |

### Round 6 (verdict: ready after fixes)

| # | Finding | Resolution (v5.2) |
|---|---|---|
| 1 | P2: kind checks could take all 20 slots on every call, so a dirty forum beyond page one could stay stale indefinitely | Each `peer_dialogs` call is split into reserved slots: 10 kind checks, 1 shown forum, 9 badge cursor, with unused slots spilling over. Any dirty non-shown forum is reached within `ceil(n / 9)` calls (at most 111), and checks within `ceil(m / 10)` (sections 6.6, 7). Latency bounds were recalculated. Test: sustained successful kind checks, an always-dirty shown forum, and an unrelated dirty forum beyond page one |
| 2 | P3: behaviour on expiry of a flood wait during navigation was unspecified (the app backoff gives up after 5 failures) | The UI keeps the load intent until the deadline, shows a countdown, and sends it once at expiry. A failure of that request falls back to the Stage 1 retry button. Flood waits don't count as `Backoff` failures, and navigation is never blocked (section 8). Tests: a cold open, a retained-list open and an invalidated list |
| 3 | P3: "2.5 minutes" wasn't a worst case | Reworded as nominal-rate latency excluding slow responses, contention and flood waits, and recalculated for the slot split: 111 calls, about 333 s for 1,000 dirty forums (risk 4, section 7) |

### Round 7 (verdict: ready after fixes)

| # | Finding | Resolution (v5.3) |
|---|---|---|
| 1 | P2: snapshot visibility couldn't reach core, because backend responses carried `Vec<Topic>` and `Topic` excludes visibility | The backend returns `TopicSnapshot { topic, visibility }` from both `topics` and `topics_by_id`; core applies `visibility` under the meta stamp and emits only `Topic` (sections 3, 5, 6.4). Mapping and fake tests cover an initially hidden General and a missed hide/unhide |
| 2 | P3: "127 calls" bounded first service, not fence completion; the 40-chat test assumed two calls | Reworded as "each queued check receives an attempt within `ceil(n / 10)` calls, excluding rebuild re-dispatch", with fence lifetime stated separately (at most 5 attempts; superseded in round 8 by the 4-attempt bound). The test separates first attempts from rechecks and retries, with 10 guaranteed slots under badge contention and 20 only with full spill-over (sections 6.6, 11) |

### Round 8 (verdict: ready after fixes)

| # | Finding | Resolution (v5.3) |
|---|---|---|
| 1 | P2: the fake still stored `Vec<Topic>`, so it couldn't hold a hidden General or a missed hide/unhide | `FakeScript.topics` holds `Vec<TopicSnapshot>`; scripted hides update stored visibility even when the update is dropped (section 5) |
| 2 | P3: `Visibility` was called core-internal and redeclared privately in section 6.4 | One public definition (section 3); section 6.4 refers to it, and only `Deletion` is core-internal |
| 3 | P3: the "5 attempts" fence lifetime didn't follow from the rules | Per-fence recheck budget 1 and cumulative failure budget 3 (third failure abandons); rebuild-discarded attempts are not counted. The tight bound is 4 completed attempts (section 6.6) |
| 4 | P3: the status line still said v5.2 | Updated to v5.3 |

### Round 9 (verdict: ready after fixes; no P0–P2 remains)

| # | Finding | Resolution (v5.3) |
|---|---|---|
| 1 | P3: the budget table still said "3 tries each, then abandoned" | Reworded to the per-fence budgets and the 4-attempt bound (section 9) |
| 2 | P3: no test of cumulative failures across a check and its recheck, or of budgets across a rebuild | Fence-budget trace test added (section 11) |

### Task 1: re-key by `ThreadKey` (verdict: ready after fixes; no P0–P2)

| # | Finding | Resolution |
|---|---|---|
| 1 | P3: "topic is always `None`" was false for message payloads; the key-equality invariant doesn't hold yet | Task 1 text says `None` applies to keys only; normalization and window/event equality deferred to task 7, cache enforcement to task 5; `shown_in` documents the hand-off |
| 2 | P3: no regression test for a topic-tagged message in a whole-chat window | `actor/tests/topic_keys.rs` (deliver and edit into an open window; no cache write-through for a closed one) and an invalid `TopicCreate` id case in `map.rs` |

### Task 2: topic model, ordering and merge policy (4 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready after fixes) | P2: unlisted edits bypassed `unlisted_change`; page placement undid newer activity; cap evictions not emitted and not enforced on `set_open`; posting permission followed completion order; live changes didn't make topics due; memory test didn't prove the bound | `note_unlisted` for edits; rank kept/repositioned; evictions emitted as `Removed`, consumer contract on `ListChange`; `post_closed_seen` stamp; live changes mark due; measured capacities, inline due state |
| 2 (ready after fixes) | P2: a refresh demoted omitted topics with newer live activity; replay divergence hidden by a `Reset` fallback. P3: budget was an estimate. Flaky closed-window test | Omitted topics keep earned rank; shown-only placement, `debug_assert` on replay, deliberate `ServerOrder` reset only for later pages; "retained payload capacity" budget with 17% margin, `MAX_TOPIC_LISTS` 6; command-ordering barrier in the test |
| 3 (ready after fixes) | P2: omitted topics with a newer applied snapshot were demoted | `keep_rank` filters on `activity.judge(seq) != Apply`; cap-case regression |
| 4 | Pass | — |

### Task 3: backend trait and fake (2 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready after fixes) | P2: the fake reused a deleted topic's id. P3: the delayed `peer_dialogs` test posted into an already-unread topic | Per-chat id high-water mark, never lowered, `checked_add` allocation; delayed test posts into a read topic (21 delayed, 22 next) |
| 2 | Pass | — |

### Task 4: grammers implementation (2 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready after fixes) | P2: a short `getForumTopics` page ended pagination, but Telegram may return fewer than requested | `next` is `None` only for an empty page or when the listed count reaches `count`; regression test; §5/§13 corrected. RPC names confirmed against core.telegram.org (`TOPIC_CLOSED`/`TOPIC_DELETED` are 406 on sendMessage; `CHANNEL_FORUM_MISSING` on getForumTopics) |
| 2 | Pass. Termination is guaranteed by the cursor's cumulative `index`; duplicate pages could still cause redundant requests, so task 6 now stops paging on a page that adds nothing | — |

### Task 5: cache schema v4 (2 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready) | P3: the query-plan test explained a simplified query | Fixing it exposed a real bug: topic pages used the primary key and filtered other topics row by row. Page SQL now comes from one builder shared with the plan test; topic keys read `INDEXED BY messages_thread`; all three page kinds asserted |
| 2 | Pass | Follow-up (non-blocking): a v4 database whose `messages_thread` index was dropped externally isn't repaired; topic reads then fail as `CacheError::Sql` and degrade to cache misses. Consider recreating missing indexes at open |

### Task 6: core topic lists (3 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready after fixes) | P2: flood deadline not rechecked at admission, so a queued topics job could start during a flood wait; P2: by-id panic path skipped the 1 s interval. P3: an unlisted open topic retried ~1/s forever | Admission skips topics jobs during the wait (order kept, wake at deadline); shared `by_id_ended`; retry record with 1→30 s backoff, reset on open/refresh. Accepted deviations: list reducers for live updates pulled forward from task 8, receipt-time stamping, interval from call end |
| 2 (changes requested) | P2: discovery lost when the open topic changed. P3: cache-test barrier race | Open id stays in discovery but is skipped by dispatch while open; cache delete queued before `ThreadInvalidated` |
| 3 | Pass | — |

### Task 7: core topic threads (2 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 (ready after fixes) | P1: a tail in flight across a loss of continuity restored it, recreating the round-2 gap. P2: forums rendered as an empty conversation (whole-chat view filtered out every forum message). P2: forum status was inferred from view shape | Per-window continuity clock with a `ContinuityMark` captured at dispatch; forum placeholder pane with no composer, and core refuses a whole-chat `OpenThread` on a known forum; `KnownKinds` remembers the last stated kind for chats with views (≤ 14) |
| 2 (pass) | P3: the continuity clock could wrap | Saturating clock; restoration refused once exhausted |

**Dependency for task 8 (from this review):** after a loss of continuity nothing re-requests the tail, so an open window can stay non-contiguous and live messages stay uncached until the next tail. Task 8's fence lift must invalidate and reload the affected open windows (already in its spec); its tests must assert continuity is restored afterwards.

### Task 8: live routing, forum badge, forum flag (6 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 | P2: `peer_dialogs` interval started at planning, not admission; retired-check results still applied; a provisional kind survived abandonment; queued history/sends bypassed a new fence | Reserve vs dispatch split; retired chats filtered from results; kind committed only at settlement; fence entry unqueues history (answered) and holds sends; admission skips fenced jobs |
| 2 | P2: a kind stated during a check was discarded; settled kind not persisted; non-forum summaries never cleared badge dirty stamps | Fence keeps the newest authoritative statement; `PutChats` after a changed commit; non-forum results settle dirty stamps |
| 3 | P2 (one class): older snapshots from one source overwrote newer ones from another (kind after settlement, folder via a cleared move record, unread via a non-forum snapshot) | Per-field accepted stamps (kind, folder, unread) on each row on one shared dispatch clock; every snapshot source passes `Dialogs::guard`; replaced `DialogsSeq`, the move-record map, `Badges::offer` and `keep_fenced_kind` |
| 4 | P2: unlisted chats kept a stamp without its value | Unlisted records keep (value, stamp) pairs until admission; independent source-log reference model with membership transitions |
| 5 | P2: eviction during `guard()` bypassed the cutoff; `commit_kind` stamped only listed chats | Room made before checking, never evicting chats the response lists; unlisted records carry committed kinds |
| 6 | Pass | Also fixed: a task 2 store bug found by `merge_policy` (an edit while a topic's activity is unsettled now counts as a live change; §6.3 amended; seed kept in proptest-regressions). Notes: `guard()` relies on callers' input bounds (≤ 1,000); continuous edits can keep a topic due with 30 s backoff |

### Task 9: sending and read marks per topic (3 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 | P2: timeouts and flood waits dropped an accepted read intent, leaving the thread unread while the UI suppresses a repeat `MarkRead` | Read intents kept per thread until a covering read lands, one flight per thread, backoff 1→30 s, flood waits after their deadline, rejections terminal; only a read's first timeout counts toward stall detection |
| 2 | P2: excluding retried timeouts also bypassed the exhausted-permit recovery, so retried reads could park every permit | Independent check at the end of `on_done`: all permits parked on the current client triggers a stall rebuild (no-op once retiring); the existing lifecycle test extends to cover it |
| 3 | Pass | Accepted: closed-topic refusal at admission only; held sends keep their original thread across a kind change |

### Task 10: forum navigation and the topic list (6 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 | P2: app truncation broke core's replay contract; a forum flag change left the wrong pane; removing the open topic left its composer active | Exact replay; panes reconcile with the chat's kind; open-topic removal returns to the list with a notice |
| 2 | P2: refused reconciliation looped every frame; window-only chats never reconciled; overflow text could land in another chat | Backoff with a scheduled wake; new `Event::KindChanged`; (overflow slot, later replaced) |
| 3 | P2: the overflow draft slot could block navigation or panic | Overflow slot removed; drafts made unsendable by a kind change or deletion are discarded with a Copy notice; cap stays 64 |
| 4 | P2: cleanup depended on the shown pane; bulk discards kept one text | Cleanup runs on the invalidating event; one notice accumulates up to 64 texts, Copy takes all |
| (lead) | Cap evictions were reported like deletions, discarding valid drafts | `TopicRemoved` carries `TopicGone { Deleted, Hidden, Evicted }`; only `Deleted` discards drafts |
| 5 | P2: a deletion in the same response as its eviction surfaced only as `Evicted`; resets dropped `Hidden` | By-id deletions always emit `Deleted`; semantic removals precede resets; an open topic fetched hidden emits `Hidden` |
| 6 | Pass | Note: re-discovering an already deleted id can emit `Deleted` again (bounded, idempotent cleanup) |

### Task 11: topic thread (2 rounds; pass, no P0–P2)

| Round | Findings | Resolution |
|---|---|---|
| 1 | P2: a send between `KindChanged` and its summary created an unmarked echo in an obsolete key. P3: obsolete-send cleanup waited for a painted pane | The latest kind core stated governs the pane and send admission; cleanup runs on events with its own backoff; discarded sends of unknown outcome say they may have been sent |
| 2 | Pass. P3 applied by the lead: a reset cleanup backoff now wakes at once (not while core is stopped) | — |
