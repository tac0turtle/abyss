//! Offline tests of the grammers backend: hand-built TL values for the pure
//! mappers, a scripted invoker for the call layer, a scripted source for the
//! update stream.

mod auth;
mod calls;
mod errors;
mod forum;
mod map;
mod pins;
mod stream;

use grammers_client::tl;

pub(super) fn peer_user(id: i64) -> tl::enums::Peer {
    tl::enums::Peer::User(tl::types::PeerUser { user_id: id })
}

pub(super) fn peer_chat(id: i64) -> tl::enums::Peer {
    tl::enums::Peer::Chat(tl::types::PeerChat { chat_id: id })
}

pub(super) fn peer_channel(id: i64) -> tl::enums::Peer {
    tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: id })
}

pub(super) fn user(id: i64, first: &str, last: &str) -> tl::types::User {
    tl::types::User {
        is_self: false,
        contact: false,
        mutual_contact: false,
        deleted: false,
        bot: false,
        bot_chat_history: false,
        bot_nochats: false,
        verified: false,
        restricted: false,
        min: false,
        bot_inline_geo: false,
        support: false,
        scam: false,
        apply_min_photo: false,
        fake: false,
        bot_attach_menu: false,
        premium: false,
        attach_menu_enabled: false,
        bot_can_edit: false,
        close_friend: false,
        stories_hidden: false,
        stories_unavailable: false,
        contact_require_premium: false,
        bot_business: false,
        bot_has_main_app: false,
        bot_forum_view: false,
        bot_forum_can_manage_topics: false,
        bot_can_manage_bots: false,
        bot_guestchat: false,
        bot_guard: false,
        bot_forum_sidebar: false,
        id,
        access_hash: Some(id * 7),
        first_name: Some(first.to_owned()),
        last_name: (!last.is_empty()).then(|| last.to_owned()),
        username: None,
        phone: Some("+440000000000".to_owned()),
        photo: None,
        status: None,
        bot_info_version: None,
        restriction_reason: None,
        bot_inline_placeholder: None,
        lang_code: None,
        emoji_status: None,
        usernames: None,
        stories_max_id: None,
        color: None,
        profile_color: None,
        bot_active_users: None,
        bot_verification_icon: None,
        send_paid_messages_stars: None,
        linked_community_id: None,
    }
}

pub(super) fn users(list: Vec<tl::types::User>) -> Vec<tl::enums::User> {
    list.into_iter().map(tl::enums::User::User).collect()
}

/// A megagroup (supergroup) the user is a member of.
pub(super) fn channel(id: i64, title: &str) -> tl::types::Channel {
    tl::types::Channel {
        creator: false,
        left: false,
        broadcast: false,
        verified: false,
        megagroup: true,
        restricted: false,
        signatures: false,
        min: false,
        scam: false,
        has_link: false,
        has_geo: false,
        slowmode_enabled: false,
        call_active: false,
        call_not_empty: false,
        fake: false,
        gigagroup: false,
        noforwards: false,
        join_to_send: false,
        join_request: false,
        forum: false,
        stories_hidden: false,
        stories_hidden_min: false,
        stories_unavailable: false,
        signature_profiles: false,
        autotranslation: false,
        broadcast_messages_allowed: false,
        monoforum: false,
        forum_tabs: false,
        in_news: false,
        forum_sidebar: false,
        id,
        access_hash: Some(id * 11),
        title: title.to_owned(),
        username: None,
        photo: tl::enums::ChatPhoto::Empty,
        date: 0,
        restriction_reason: None,
        admin_rights: None,
        banned_rights: None,
        default_banned_rights: None,
        participants_count: None,
        usernames: None,
        stories_max_id: None,
        color: None,
        profile_color: None,
        emoji_status: None,
        level: None,
        subscription_until_date: None,
        bot_verification_icon: None,
        send_paid_messages_stars: None,
        linked_monoforum_id: None,
        linked_community_id: None,
        pinned_msg_id: None,
    }
}

pub(super) fn basic_group(id: i64, title: &str) -> tl::types::Chat {
    tl::types::Chat {
        creator: false,
        left: false,
        deactivated: false,
        call_active: false,
        call_not_empty: false,
        noforwards: false,
        id,
        title: title.to_owned(),
        photo: tl::enums::ChatPhoto::Empty,
        participants_count: 3,
        date: 0,
        version: 1,
        migrated_to: None,
        admin_rights: None,
        default_banned_rights: None,
    }
}

pub(super) fn message(peer: tl::enums::Peer, id: i32, text: &str) -> tl::types::Message {
    tl::types::Message {
        out: false,
        mentioned: false,
        media_unread: false,
        silent: false,
        post: false,
        from_scheduled: false,
        legacy: false,
        edit_hide: false,
        pinned: false,
        noforwards: false,
        invert_media: false,
        offline: false,
        video_processing_pending: false,
        paid_suggested_post_stars: false,
        paid_suggested_post_ton: false,
        id,
        from_id: None,
        from_boosts_applied: None,
        from_rank: None,
        peer_id: peer,
        saved_peer_id: None,
        fwd_from: None,
        via_bot_id: None,
        via_business_bot_id: None,
        guestchat_via_from: None,
        reply_to: None,
        date: 1_700_000_000 + id,
        message: text.to_owned(),
        media: None,
        reply_markup: None,
        entities: None,
        views: None,
        forwards: None,
        replies: None,
        edit_date: None,
        post_author: None,
        grouped_id: None,
        reactions: None,
        restriction_reason: None,
        ttl_period: None,
        quick_reply_shortcut_id: None,
        effect: None,
        factcheck: None,
        report_delivery_until_date: None,
        paid_message_stars: None,
        suggested_post: None,
        schedule_repeat_period: None,
        summary_from_language: None,
        rich_message: None,
    }
}

pub(super) fn service(
    peer: tl::enums::Peer,
    id: i32,
    from: Option<tl::enums::Peer>,
    action: tl::enums::MessageAction,
) -> tl::types::MessageService {
    tl::types::MessageService {
        out: false,
        mentioned: false,
        media_unread: false,
        reactions_are_possible: false,
        silent: false,
        post: false,
        legacy: false,
        id,
        from_id: from,
        peer_id: peer,
        saved_peer_id: None,
        reply_to: None,
        date: 1_700_000_000 + id,
        action,
        reactions: None,
        ttl_period: None,
    }
}

pub(super) fn dialog(peer: tl::enums::Peer, top: i32) -> tl::types::Dialog {
    tl::types::Dialog {
        pinned: false,
        unread_mark: false,
        view_forum_as_messages: false,
        peer,
        top_message: top,
        read_inbox_max_id: 0,
        read_outbox_max_id: 0,
        unread_count: 0,
        unread_mentions_count: 0,
        unread_reactions_count: 0,
        unread_poll_votes_count: 0,
        notify_settings: notify_settings(),
        pts: None,
        draft: None,
        folder_id: None,
        ttl_period: None,
    }
}

pub(super) fn reply_header(msg: Option<i32>, top: Option<i32>) -> tl::types::MessageReplyHeader {
    tl::types::MessageReplyHeader {
        reply_to_scheduled: false,
        forum_topic: false,
        quote: false,
        reply_to_ephemeral: false,
        reply_to_msg_id: msg,
        reply_to_peer_id: None,
        reply_from: None,
        reply_media: None,
        reply_to_top_id: top,
        quote_text: None,
        quote_entities: None,
        quote_offset: None,
        todo_item_id: None,
        poll_option: None,
        quote_rich: None,
    }
}

pub(super) fn notify_settings() -> tl::enums::PeerNotifySettings {
    tl::enums::PeerNotifySettings::Settings(notify_settings_raw())
}

/// Default notification settings: never muted.
pub(super) fn notify_settings_raw() -> tl::types::PeerNotifySettings {
    tl::types::PeerNotifySettings {
        show_previews: None,
        silent: None,
        mute_until: None,
        ios_sound: None,
        android_sound: None,
        other_sound: None,
        stories_muted: None,
        stories_hide_sender: None,
        stories_ios_sound: None,
        stories_android_sound: None,
        stories_other_sound: None,
    }
}

/// The archive folder's entry in the main list, led by `peer`'s top message.
pub(super) fn folder(peer: tl::enums::Peer, top: i32) -> tl::enums::Dialog {
    tl::enums::Dialog::Folder(tl::types::DialogFolder {
        pinned: false,
        folder: tl::enums::Folder::Folder(tl::types::Folder {
            autofill_new_broadcasts: false,
            autofill_public_groups: false,
            autofill_new_correspondents: false,
            id: 1,
            title: "Archived Chats".to_owned(),
            photo: None,
        }),
        peer,
        top_message: top,
        unread_muted_peers_count: 0,
        unread_unmuted_peers_count: 0,
        unread_muted_messages_count: 0,
        unread_unmuted_messages_count: 0,
    })
}

pub(super) fn community(id: i64) -> tl::enums::Dialog {
    tl::enums::Dialog::Community(tl::types::DialogCommunity {
        pinned: false,
        community_id: id,
        notify_settings: notify_settings(),
    })
}

pub(super) fn bold(offset: i32, length: i32) -> tl::enums::MessageEntity {
    tl::enums::MessageEntity::Bold(tl::types::MessageEntityBold { offset, length })
}

pub(super) fn text_url(offset: i32, length: i32, url: &str) -> tl::enums::MessageEntity {
    tl::enums::MessageEntity::TextUrl(tl::types::MessageEntityTextUrl {
        offset,
        length,
        url: url.to_owned(),
    })
}

pub(super) fn spoiler(offset: i32, length: i32) -> tl::enums::MessageEntity {
    tl::enums::MessageEntity::Spoiler(tl::types::MessageEntitySpoiler { offset, length })
}
