# abyss plan: archive and chat folders

Status: **draft v1** (2026-11-17), proposed by an outside contributor in three PRs. It has not been through the GPT-6 Astra review loop; the PRs ask for it.

Telegram has two unrelated things called folders:
- **Peer folders**: every chat is in exactly one, either the main list (folder 0) or the archive (folder 1). Moving a chat is `folders.editPeerFolders` (`api.tl:3172`); moves arrive as `updateFolderPeers` (`api.tl:393`).
- **Chat folders** (`dialogFilter`, `api.tl:1394`): user-defined filtered views of the whole list ("Work", "Unread"), synced across devices. They are listed by `messages.getDialogFilters` (`api.tl:2734`) and edited with `messages.updateDialogFilter` (`api.tl:2736`). Changes arrive as `updateDialogFilter`, `updateDialogFilterOrder` and `updateDialogFilters` (`api.tl:402-404`).

Today abyss shows one flat list. Stage 1 requests dialogs without a `folder_id` (section 7), which returns every dialog of both peer folders, each carrying its own `folder_id` (risk 8). Archived chats are therefore mixed into the main list, which makes a long-lived account's list hard to use.

The work is split so each part is reviewable and useful on its own:

| Part | PR | Scope | Writes to the account |
|---|---|---|---|
| A | 1 | Archive as a separate view | No |
| B | 2 | Chat folders, read-only | No |
| C | 3 | Archiving and folder editing | Yes |

## A. The archive (PR 1)

**Model.** `ChatSummary` gains `folder: PeerFolder` (`Main | Archive`). The flag lives on the summary because the list stays **one** list: core keeps a single server-ordered `Dialogs` holding both peer folders, and the UI shows one view of it at a time. Nothing about pagination, merging, provisional rows or the 1,000-dialog cap changes.

**Mapping.** `dialog.folder_id == 1` maps to `Archive`. Any other id, or none, maps to `Main`. The dialogs request is unchanged (`folder_id` unset).

**Live moves.** `updateFolderPeers` maps to `Update::Folders(Vec<(ChatId, PeerFolder)>)`, capped at 1,000 entries and with out-of-range peers dropped. Core's `Dialogs::set_folder` changes a listed chat in place: no reorder and no `rev` bump. Each change is sent as `ChatBumped`, which the UI already applies in place when no newer message comes with it. The cached list is rewritten (`PutChats`), so a restart shows the same split. Unlisted chats are ignored; the next dialogs page brings them with their folder.

**Crossing responses.** A dialogs response may have been taken before a live move and arrive after it. Each dialogs request takes a `DialogsSeq` when it is dispatched, and `Dialogs::set_folder` records every move (listed chat or not, at most 1,000) with the seq current at the time. Before a page is applied, `Dialogs::guard_folders` gives each chat moved at or after the page's dispatch its live folder, so neither a first page nor a later page moves it back, and the cached list keeps the move. If the page disagreed with a kept move, core fetches the first page again. A page dispatched after a move is authoritative for that chat and settles it. Past 1,000 records the oldest move (in move order) is evicted, and every page dispatched at or before its seq is dropped unapplied and the first page fetched again, since it can no longer be guarded. A refetch is dispatched after every recorded move, so it is authoritative for all of them: only a new move crossing it, from the user or the server, asks for another, so the refetches cannot sustain themselves. Bootstrap and refresh snapshots are covered twice: live updates are buffered until the snapshots are applied. Telegram unarchives a chat on a new message unless it is muted, and sends `updateFolderPeers` when it does, so no client-side rule is needed.

**Cache.** postcard rows are positional, so schema version 5 drops the cached chat list (version 4 rows cannot be decoded). Messages are kept. This is the same pattern as 2→3. Version 4 is Stage 2's topic column ([stage-2.md](stage-2.md) section 10); versions 1 to 3 upgrade through 4 to 5.

**UI.**
- `ChatList` has a `Shown` view (`Main | Archive`). `rows()` yields `SidebarRow::Archive(ArchiveEntry)` and then the main chats, or only the archived chats.
- In the main view, the archive's entry leads the list once an archived chat is loaded. It shows how many archived chats are loaded and how many are unread, and opens the archive. In the archive view, the heading reads "‹ Archive" and leads back.
- Pagination is driven by the shown view's length. The server traverses both folders together, so the next page may hold more of either. A view with no row wants the next page at once, because nothing renders to ask for it.
- Selection, drafts and the open chat do not depend on the view.
- Each view is its own list (`chats-main`, `chats-archive`), so each keeps its own scroll position.

**Pinned archived chats.** `Dialogs` keeps pinned chats as one block at the head of the list, and `merge_first_page` unpins a kept chat that is outside that block. If Telegram pins archived chats per folder and returns them later in the combined traversal, they would lose their pin on the next refresh. Task 12 must check how pins are ordered in the combined traversal. If pins are per folder, `merge_first_page` must become folder-aware.

**Limits.** The 1,000-dialog cap counts both views. An account with more than 1,000 dialogs may see fewer archived chats than it has. This is the same bound as today, and the entry's count says "loaded".

**Tests.**
- Mapping: folder ids and `updateFolderPeers`, including out-of-range peers.
- Store: `set_folder` changes in place and is idempotent.
- Actor: a live move emits one `ChatBumped` and ignores repeats and unlisted chats. A first page and a later page that cross a move keep it and trigger one refetch. A page older than an evicted move is dropped and refetched, and the cache keeps the move.
- Cache: the 4→5 drop and the `folder` round trip.
- App: the views, the archive entry and pagination by view.
- A snapshot pair (`archive-day/night.png`), and the demo archives every sixth chat.

**Live check (task 12).**
- Confirm that an unset `folder_id` returns archived dialogs with `folder_id = 1`, as Telethon documents. If it instead returns a `dialogFolder` entry in their place, part A needs a second traversal with `folder_id = 1`.
- Check where pinned archived chats appear in the combined traversal. If pins are per folder, `merge_first_page` must become folder-aware (see "Pinned archived chats").

## B. Chat folders, read-only (PR 2)

**Backend.** `Client::chat_folders() -> Vec<ChatFolder>` calls `messages.getDialogFilters` with retry class `Read`.
- Each `dialogFilter` maps to `ChatFolder { id, title, rules, pinned, include, exclude }`.
- `dialogFilterDefault` (the "All chats" position) is skipped.
- `dialogFilterChatlist` (a shared folder) maps with `include` only and `shared = true`.
- Titles go through `display_name`. Entities and animated emoji are dropped.
- `rules` holds the flags: contacts, non-contacts, groups, broadcasts, bots, exclude muted, exclude read, exclude archived.
- Peers are `InputPeer`s. Each is mapped to a `ChatId` with the same range checks as `chat_id`. `inputPeerSelf` maps to the signed-in user's id, and anything else that does not map is dropped.
- Bounds: at most 64 folders, at most 512 peers per folder across `pinned + include + exclude` (Telegram's own limits are 30 and 200), and the response is rejected whole above them (`Oversized`).

**Model additions for the rules.**
- `ChatSummary` gains `muted_until: i64`, the raw `dialog.notify_settings.mute_until` (0 when not muted). Matching checks it against the clock (`ChatSummary::muted_at`), so an expired mute stops counting without a new page.
- A mute changed on another device (`updateNotifySettings`) is not applied live yet; the next dialogs page brings it. Applying it would need the same crossing-response guard as folder moves (part A). This is a follow-up.
- `ChatKind::Private` becomes `Private { contact }`, from `user.contact`. It mirrors `Supergroup { forum, gigagroup }` and keeps `ChatSummary` within `struct_excessive_bools`.
- `FolderRules` is a `u8` flag set, like `StyleFlags`.
- Cache schema version 6 (5→6 drops the cached chat list; versions 1 to 5 upgrade through to 6).

**Core.**
- Folders are loaded after each successful first dialogs page (bootstrap and refresh) and sent as `Event::ChatFolders(Vec<ChatFolder>)`.
- `updateDialogFilter`, `updateDialogFilterOrder` and `updateDialogFilters` all map to `Update::ChatFoldersChanged`. Core refetches, coalesced through `FoldersLoad { Idle, Running, Again }`: at most one load in flight and one wanted after it.
- A failed load is only logged. The next first page asks again. A load that panics still runs the load wanted after it.
- Folders are not cached on disk. They come back with the first page.

**Matching** follows Telegram's rule and runs in the UI over the loaded list:
- A chat in `pinned` or `include` is always shown.
- A chat in `exclude` is never shown.
- Any other chat is shown when its kind matches a flag: `contacts` and `non_contacts` for users, `bots`, `groups` for basic groups and supergroups. Broadcasts never reach the list.
- A chat that matches is then dropped if `exclude_muted` and it is muted (unless it has an unread mention and is not archived, as Telegram Desktop's `ChatFilter::contains` keeps it), if `exclude_read` and it has no unread messages, or if `exclude_archived` and it is archived.
- Forums are supergroups, so `groups` takes them in (Telegram Desktop counts megagroups as groups).
- The folder's `pinned` chats come first, in folder order, then the rest in list order.

**UI.** A strip of folder tabs under the heading: "All", then each folder in server order. "All" is today's view, archive entry included. A folder view shows only matching chats from both peer folders, with no archive entry. The `Shown` enum gains `Folder(id)`. A folder that is removed while shown falls back to "All".

**Limits.** Matching sees only loaded dialogs (at most 1,000). A folder's include list may name chats beyond the cap, which simply don't show. That is the same limit as the main list.

## C. Archiving and folder editing (PR 3)

These are the first account-changing calls besides sending and read marks, so they stay narrow.

**Commands** (both account-scoped):
- `SetPeerFolder { chat, folder }` calls `folders.editPeerFolders`. On success core applies the move itself (`on_folders`, the same path as `updateFolderPeers`), so the echoed update changes nothing.
- `EditChatFolder(FolderEdit)`, where `FolderEdit` is `AddChat { folder, tag, chat }`, `RemoveChat { folder, tag, chat }`, `Create { title, chat }` or `Delete { folder, tag }`. It calls `messages.updateDialogFilter`, and on success core refetches the folders.
- `tag` is the `FolderTag` of the folder as the UI showed it: its id, title, rules and shared flag, compared field by field (no hash, so no collisions; bounded by the title's bound, and never logged). The backend compares it with the folder as fetched right before the write. A folder that is gone, or whose tag differs, refuses the edit with `UserError::FolderChanged` ("That folder changed in the meantime"). This is best-effort detection of observable identity changes: a renamed folder, changed rules, or a replacement that differs in those fields. A replacement identical in id, title, rules and shared flag (for example "Work" deleted and recreated as it was) is not detected, and the edit applies to it. Changes to the folder's chats keep the tag, so two toggles in a row both apply.
- While a logout hides the account ("Logging out", "Couldn't start logout"), both commands are refused (`Busy`) like sends, and writes not yet handed to the server are dropped. Core then reports `UserError::FolderWritesCancelled`; the app keeps that notice up through the logout screen and shows it as a passing notice once the account is back ("Stay signed in") or the logout is done.
- A `Create` title must pass `model::folder_title`: sanitized, trimmed, 1 to 12 characters. Otherwise the command is answered `Rejected` and nothing is sent.
- `Debug` of `FolderEdit` leaves the title and the tag out.

**Round-tripping what abyss does not model.** Each edit fetches the raw `dialogFilter` values itself, right before its write, and builds the write from that response alone (no copy is shared between calls). `map::folders::edited`, a pure and tested function, changes only what the edit names and sends the rest back unchanged: emoticons, colors, title entities, other flags, and peers abyss could not map or that lie beyond the dialog cap.
- `AddChat` drops the chat from the exclusions and adds it to the included chats, unless it is already named.
- `RemoveChat` drops the chat from the pinned and included chats. If the folder has kind rules, the chat is also added to the exclusions, as Telegram Desktop does, so the rules don't take it back in.
- `Create` takes the smallest free id from 2 to 255.
- The answer is `FolderChanged` for a folder that is gone or whose tag differs, `FolderLimit` for an edit past the bounds (a 65th folder, no free id, or more than 512 peers in the folder), and `Rejected` for a shared (chatlist) folder, which needs the `chatlists.*` calls. All are refused before anything is written.
- The fresh fetch matters: an older copy could undo a change made since (on another device, or by the previous write), because the write replaces the whole folder, and an instance that never fetched would pick id 2 for a new folder.
- The fake backend applies the same rules, and also fetches, then writes: a write is made from the folders at call time and lands after any injected delay, so tests can interleave writes.

**Jobs.** Folder calls are `Job::Folders(FolderJob::{Load, SetPeerFolder, Edit})`. Both writes set state, so they use the `Read` retry class. A failure is sent as `Error`, and the UI changes only when core reports the change.

**One write at a time** (`actor/folder_writes.rs`). An edit is a fetch, then a whole-folder write, so two in flight together could both start from the same state: one add would be lost, two creates would pick the same id, a delete racing an edit would bring the folder back. Core admits one folder write per account; the others wait in order (at most 16, then `Busy`). The next starts when the previous one's outcome is known, or known to be unknown: a timeout, or its instance retiring (retirement hands folder writes back like sends).

**Uncertain writes: best-effort recovery.** A write whose outcome is unknown may still sit in the client's transport, or be applied by the server later. That covers a timeout, a transport error (`Transient`, for example an I/O failure after the request was sent or a reply that could not be decoded), a panic in its task, and a write whose completion was lost with a panicked retirement. After one, core holds the next write. It ends the hold only when the instance that carried the uncertain write is retired (or the account is reset). Core retires that instance at once, through the stall path (`Reason::Stall`). Progress after an uncertain write depends on that retirement completing, as every other client operation does under the single rebuild mechanism: nothing is sent while no client is live, and a retirement that hangs holds folder writes along with everything else. Then core reloads the folders and admits the next write, which fetches its own state on the new instance. This is a mitigation, not a guarantee: retirement only ends this client's side. The admitted write, if still queued, survives an instance rebuild, as sends do. A logout drops the old account's writes; an outcome that arrives for one later is ignored.

**UI** (`shell/folder_menu.rs`):
- Right-clicking a chat row opens a menu: Archive or Unarchive, then one ✓ toggle per regular folder, computed with `ChatFolder::contains`, then "New folder with this chat…". That last entry turns into a title field (`TextInput`, single line); Enter creates the folder.
- Right-clicking a folder tab offers "Delete folder", which needs a second click. The menu keeps the folder's tag from when it opened, and the delete carries that tag. If the folder is gone or its tag changed before the second click (new `ChatFolders`), the menu closes; a click that finds the tag changed deletes nothing and says so. An identical replacement is not told apart (see `tag` above). Shared folders say they can't be changed yet.
- A backdrop closes the menu, and the menu is kept inside the window.

**Risks.**
- Lost update after an uncertain write. Telegram may process requests in any order, and `messages.updateDialogFilter` replaces the whole folder with no version check. If the server applies an uncertain write after the next write's fetch, the next write is built without it, and one of the two changes is lost (an added chat goes missing, or a deleted folder comes back). The hold and reload above make this unlikely, but they cannot rule it out. Telegram offers no compare-and-set to close it.
- A retried `Create` whose first attempt was applied but not answered makes a second folder.
- A near-simultaneous edit of the same folder on another device can be overwritten for the same reason.
- `FolderTag` only detects observable identity changes. A folder deleted and recreated with the same id, title, rules and shared flag passes the check, so a queued edit or a pending delete applies to the new one.
- Progress after an uncertain write depends on the instance's retirement completing; a retirement that hangs holds folder writes, as it holds every other client operation.

**Out of scope.** Editing folder rules, renaming or reordering folders, shared folders and invite links.
