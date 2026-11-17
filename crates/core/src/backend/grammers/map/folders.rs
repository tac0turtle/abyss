//! Chat folders: `messages.getDialogFilters` to [`ChatFolder`]s (folders
//! plan, part B).

use grammers_client::tl;

use super::lists::Oversized;
use super::peers::{channel_chat, group_chat, user_chat};
use crate::model::{ChatFolder, ChatId, FolderEdit, FolderRules, FolderTag};
use crate::sanitize::display_name;

/// Folders accepted from one response. Telegram allows 30 (with Premium);
/// the bound only guards against a malformed response.
pub(crate) const MAX_FOLDERS: usize = 64;
/// Peers accepted per folder, across its pinned, included and excluded
/// lists. Telegram allows 200 included chats (with Premium) plus as many
/// excluded ones.
pub(crate) const MAX_FOLDER_PEERS: usize = 512;
/// The archive's peer folder id (`folders.editPeerFolders`).
pub(crate) const ARCHIVE: i32 = super::lists::ARCHIVE_FOLDER;

/// The chat an `InputPeer` of a folder names, if its id is in range.
/// `inputPeerSelf` is the signed-in user (Saved Messages).
fn folder_peer(peer: &tl::enums::InputPeer, self_id: Option<i64>) -> Option<ChatId> {
    use tl::enums::InputPeer as P;
    match peer {
        P::User(p) => user_chat(p.user_id),
        P::UserFromMessage(p) => user_chat(p.user_id),
        P::Chat(p) => group_chat(p.chat_id),
        P::Channel(p) => channel_chat(p.channel_id),
        P::ChannelFromMessage(p) => channel_chat(p.channel_id),
        P::PeerSelf => user_chat(self_id?),
        P::Empty => None,
    }
}

fn peers(list: &[tl::enums::InputPeer], self_id: Option<i64>) -> Vec<ChatId> {
    list.iter()
        .filter_map(|p| folder_peer(p, self_id))
        .collect()
}

fn title(t: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = t;
    display_name(&t.text)
}

fn rules(f: &tl::types::DialogFilter) -> FolderRules {
    [
        (f.contacts, FolderRules::CONTACTS),
        (f.non_contacts, FolderRules::NON_CONTACTS),
        (f.groups, FolderRules::GROUPS),
        (f.broadcasts, FolderRules::BROADCASTS),
        (f.bots, FolderRules::BOTS),
        (f.exclude_muted, FolderRules::EXCLUDE_MUTED),
        (f.exclude_read, FolderRules::EXCLUDE_READ),
        (f.exclude_archived, FolderRules::EXCLUDE_ARCHIVED),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(FolderRules::default(), |acc, (_, rule)| acc.with(rule))
}

/// The raw folders of a response, or [`Oversized`] when it carries more
/// folders, or a folder more peers, than abyss accepts. Checked before
/// anything is mapped.
pub(crate) fn raw_folders(
    resp: &tl::enums::messages::DialogFilters,
) -> Result<&[tl::enums::DialogFilter], Oversized> {
    let tl::enums::messages::DialogFilters::Filters(resp) = resp;
    if resp.filters.len() > MAX_FOLDERS {
        return Err(Oversized);
    }
    for f in &resp.filters {
        let n = match f {
            tl::enums::DialogFilter::Filter(f) => {
                f.pinned_peers.len() + f.include_peers.len() + f.exclude_peers.len()
            }
            tl::enums::DialogFilter::Chatlist(c) => c.pinned_peers.len() + c.include_peers.len(),
            tl::enums::DialogFilter::Default => 0,
        };
        if n > MAX_FOLDER_PEERS {
            return Err(Oversized);
        }
    }
    Ok(&resp.filters)
}

/// Maps checked raw folders in server order. The "All chats" position
/// (`dialogFilterDefault`) is not a folder; a folder whose title is not
/// valid after sanitizing is left out.
pub(crate) fn chat_folders(
    raw: &[tl::enums::DialogFilter],
    self_id: Option<i64>,
) -> Vec<ChatFolder> {
    raw.iter()
        .filter_map(|f| match f {
            tl::enums::DialogFilter::Filter(f) => Some(ChatFolder {
                id: f.id,
                title: title(&f.title),
                rules: rules(f),
                pinned: peers(&f.pinned_peers, self_id),
                include: peers(&f.include_peers, self_id),
                exclude: peers(&f.exclude_peers, self_id),
                shared: false,
            }),
            tl::enums::DialogFilter::Chatlist(c) => Some(ChatFolder {
                id: c.id,
                title: title(&c.title),
                rules: FolderRules::default(),
                pinned: peers(&c.pinned_peers, self_id),
                include: peers(&c.include_peers, self_id),
                exclude: Vec::new(),
                shared: true,
            }),
            tl::enums::DialogFilter::Default => None,
        })
        .filter(ChatFolder::is_valid)
        .collect()
}

/// The id of a raw folder (`None` for the "All chats" position).
pub(crate) fn raw_id(f: &tl::enums::DialogFilter) -> Option<i32> {
    match f {
        tl::enums::DialogFilter::Filter(f) => Some(f.id),
        tl::enums::DialogFilter::Chatlist(c) => Some(c.id),
        tl::enums::DialogFilter::Default => None,
    }
}

/// Why a folder edit cannot be made from the folders just fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditError {
    /// No such folder.
    Unknown,
    /// The folder's visible tag differs from the one the edit carries.
    Changed,
    /// A shared folder: changing it needs the `chatlists.*` calls.
    Shared,
    /// The result would pass the bounds: [`MAX_FOLDERS`] folders (or no
    /// free id), or [`MAX_FOLDER_PEERS`] peers in one folder.
    Limit,
}

/// Folder ids Telegram accepts for user folders (0 and 1 are reserved).
const FIRST_FOLDER_ID: i32 = 2;
const LAST_FOLDER_ID: i32 = 255;

/// The raw folder `id`, if it is a regular one whose visible tag (mapped
/// as [`chat_folders`] maps it) equals `tag`. Best effort: a replacement
/// identical in id, title, rules and sharing also matches.
fn regular(
    raw: &[tl::enums::DialogFilter],
    id: i32,
    tag: &FolderTag,
    self_id: Option<i64>,
) -> Result<tl::types::DialogFilter, EditError> {
    let found = raw
        .iter()
        .find(|f| raw_id(f) == Some(id))
        .ok_or(EditError::Unknown)?;
    let tl::enums::DialogFilter::Filter(f) = found else {
        return Err(EditError::Shared);
    };
    let mapped = chat_folders(std::slice::from_ref(found), self_id);
    match mapped.first() {
        Some(folder) if folder.tag() == *tag => Ok(f.clone()),
        _ => Err(EditError::Changed),
    }
}

/// `f` as an edit result, unless it names more peers than abyss accepts.
fn bounded(f: tl::types::DialogFilter) -> Result<tl::enums::DialogFilter, EditError> {
    let peers = f.pinned_peers.len() + f.include_peers.len() + f.exclude_peers.len();
    if peers > MAX_FOLDER_PEERS {
        return Err(EditError::Limit);
    }
    Ok(tl::enums::DialogFilter::Filter(f))
}

/// Whether a folder's rules take in chats by kind (so a chat removed from
/// it by name must be excluded to stay out).
fn has_kind_rules(f: &tl::types::DialogFilter) -> bool {
    f.contacts || f.non_contacts || f.groups || f.broadcasts || f.bots
}

/// The `messages.updateDialogFilter` arguments for `edit`, made from the
/// raw folders just fetched so that everything abyss does not model
/// (emoticon, color, title entities, peers it could not map or that lie
/// beyond the loaded list) is sent back unchanged. `peer` addresses the
/// edit's chat; `self_id` maps `inputPeerSelf` as in [`chat_folders`].
/// `filter: None` deletes.
pub(crate) fn edited(
    raw: &[tl::enums::DialogFilter],
    edit: &FolderEdit,
    peer: Option<tl::enums::InputPeer>,
    self_id: Option<i64>,
) -> Result<(i32, Option<tl::enums::DialogFilter>), EditError> {
    let is = |chat: ChatId| move |p: &tl::enums::InputPeer| folder_peer(p, self_id) == Some(chat);
    match edit {
        FolderEdit::AddChat { folder, tag, chat } => {
            let mut f = regular(raw, *folder, tag, self_id)?;
            f.exclude_peers.retain(|p| !is(*chat)(p));
            let named = f.pinned_peers.iter().chain(&f.include_peers).any(is(*chat));
            if !named && let Some(peer) = peer {
                f.include_peers.push(peer);
            }
            Ok((*folder, Some(bounded(f)?)))
        }
        FolderEdit::RemoveChat { folder, tag, chat } => {
            let mut f = regular(raw, *folder, tag, self_id)?;
            f.pinned_peers.retain(|p| !is(*chat)(p));
            f.include_peers.retain(|p| !is(*chat)(p));
            let excluded = f.exclude_peers.iter().any(is(*chat));
            if has_kind_rules(&f)
                && !excluded
                && let Some(peer) = peer
            {
                f.exclude_peers.push(peer);
            }
            Ok((*folder, Some(bounded(f)?)))
        }
        FolderEdit::Create { title, .. } => {
            let folders = raw.iter().filter(|f| raw_id(f).is_some()).count();
            if folders >= MAX_FOLDERS {
                return Err(EditError::Limit);
            }
            let taken = |id: i32| raw.iter().any(|f| raw_id(f) == Some(id));
            let id = (FIRST_FOLDER_ID..=LAST_FOLDER_ID)
                .find(|&id| !taken(id))
                .ok_or(EditError::Limit)?;
            let f = tl::types::DialogFilter {
                contacts: false,
                non_contacts: false,
                groups: false,
                broadcasts: false,
                bots: false,
                exclude_muted: false,
                exclude_read: false,
                exclude_archived: false,
                title_noanimate: false,
                id,
                title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
                    text: title.clone(),
                    entities: Vec::new(),
                }),
                emoticon: None,
                color: None,
                pinned_peers: Vec::new(),
                include_peers: peer.into_iter().collect(),
                exclude_peers: Vec::new(),
            };
            Ok((id, Some(tl::enums::DialogFilter::Filter(f))))
        }
        FolderEdit::Delete { folder, tag } => {
            regular(raw, *folder, tag, self_id)?;
            Ok((*folder, None))
        }
    }
}
