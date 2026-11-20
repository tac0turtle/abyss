use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use abyss_core::api::Notice;
use abyss_core::model::{ChatKind, ChatSummary, MsgId, TopicId};

use super::*;
use crate::chat::Pane;
use crate::notifications::{
    Authorization, BadgePreference, Banner, Center, Cleared, Delivery, Notifications, Payload,
    Receipt, SystemEvent, SystemSettings,
};
use crate::shell::session::Session;

const EPOCH: i64 = 42;
const PRIVATE: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};
const FORUM: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 1,
};

#[derive(Default)]
struct Calls {
    authorizations: usize,
    banners: Vec<Banner>,
    removed: Vec<Receipt>,
    badge: Vec<Option<String>>,
    cleared: usize,
}

struct Recording(Rc<RefCell<Calls>>);

impl Center for Recording {
    fn settings(&self) {}

    fn authorize(&self) {
        self.0.borrow_mut().authorizations += 1;
    }

    fn deliver(&self, banner: Banner) {
        self.0.borrow_mut().banners.push(banner);
    }

    fn remove(&self, ids: &[Receipt]) {
        self.0.borrow_mut().removed.extend_from_slice(ids);
    }

    fn clear(&self, done: Cleared) {
        self.0.borrow_mut().cleared += 1;
        done();
    }

    fn badge(&self, label: Option<&str>) {
        self.0.borrow_mut().badge.push(label.map(str::to_owned));
    }

    fn save_badge_preference(&self, _: BadgePreference) {}
}

fn recording() -> (Notifications, Rc<RefCell<Calls>>) {
    let calls = Rc::new(RefCell::new(Calls::default()));
    let center = Recording(Rc::clone(&calls));
    (
        Notifications::new(
            Box::new(center),
            Some(EPOCH.cast_unsigned()),
            BadgePreference::On,
        ),
        calls,
    )
}

fn system(authorization: Authorization) -> SystemSettings {
    SystemSettings {
        authorization,
        badges: authorization == Authorization::Allowed,
    }
}

fn signed_in() -> AuthState {
    AuthState::SignedIn {
        display_name: String::new(),
        new_login: None,
    }
}

fn payload(chat: ChatId, topic: i64) -> Payload {
    Payload {
        epoch: EPOCH,
        chat: chat.dialog_id().unwrap_or_default(),
        topic,
    }
}

/// Reviews A5 and A28: a click opens only what a banner of this epoch was
/// delivered for, listed chat or not.
#[gpui::test]
fn bad_clicks_only_activate_and_delivered_clicks_open_chat_or_topic(cx: &mut TestAppContext) {
    let (root, cx, harness, _calls) = allowed_root(cx);
    let mut forum = summary(FORUM, "Forum");
    forum.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    harness
        .events
        .unbounded_send(Event::DialogsReset(vec![
            summary(PRIVATE, "Private"),
            forum,
        ]))
        .unwrap();
    cx.run_until_parked();
    let general = ThreadKey {
        chat: FORUM,
        topic: Some(TopicId::GENERAL),
    };
    // Delivered: both targets are listed, and each has its banner.
    notify(&root, &harness, cx, ThreadKey::whole(PRIVATE));
    notify(&root, &harness, cx, general);
    for invalid in [
        None,
        Some(Payload {
            epoch: EPOCH - 1,
            ..payload(PRIVATE, 0)
        }),
        Some(Payload {
            chat: 99,
            ..payload(PRIVATE, 0)
        }),
        Some(payload(FORUM, i64::MAX)),
        Some(payload(PRIVATE, 1)),
    ] {
        click(&root, cx, invalid);
        assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
    }
    click(&root, cx, Some(payload(PRIVATE, 0)));
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(PRIVATE))
    );
    click(&root, cx, Some(payload(FORUM, 1)));
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(general));
}

/// Review A28: a well-formed payload for a listed chat that no banner was
/// delivered for (or whose evidence was forgotten) only activates the window.
#[gpui::test]
fn a_listed_target_without_delivery_evidence_only_activates(cx: &mut TestAppContext) {
    let (root, cx, harness, _calls) = allowed_root(cx);
    let mut forum = summary(FORUM, "Forum");
    forum.kind = ChatKind::Supergroup {
        forum: true,
        gigagroup: false,
    };
    harness
        .events
        .unbounded_send(Event::DialogsReset(vec![
            summary(PRIVATE, "Private"),
            forum,
        ]))
        .unwrap();
    cx.run_until_parked();
    for undelivered in [payload(PRIVATE, 0), payload(FORUM, 1)] {
        click(&root, cx, Some(undelivered));
        assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
    }
    // A banner for the private chat is no evidence for the forum's topic.
    notify(&root, &harness, cx, ThreadKey::whole(PRIVATE));
    click(&root, cx, Some(payload(FORUM, 1)));
    assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
    click(&root, cx, Some(payload(PRIVATE, 0)));
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(PRIVATE))
    );
}

#[test]
fn authorization_waits_for_sign_in_and_the_async_settings() {
    let (mut notices, calls) = recording();
    notices.apply_settings(system(Authorization::NotDetermined));
    assert_eq!(calls.borrow().authorizations, 0, "nobody is signed in");
    notices.signed_in(0);
    assert_eq!(calls.borrow().authorizations, 1);
    assert_eq!(notices.authorization, Authorization::Unknown);
    notices.apply_settings(system(Authorization::Denied));
    notices.end_account();
    notices.signed_in(1);
    assert_eq!(
        calls.borrow().authorizations,
        1,
        "a decision is not repeated"
    );

    // The settings reply may come after the sign-in, too.
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Unknown));
    assert_eq!(calls.borrow().authorizations, 0);
    notices.apply_settings(system(Authorization::NotDetermined));
    assert_eq!(calls.borrow().authorizations, 1);
}

#[test]
fn missing_epoch_seed_allows_removal_without_delivery_or_click_through() {
    let calls = Rc::new(RefCell::new(Calls::default()));
    let center = Recording(Rc::clone(&calls));
    let mut notices = Notifications::new(Box::new(center), None, BadgePreference::On);
    notices.apply_settings(system(Authorization::NotDetermined));
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let thread = ThreadKey::whole(PRIVATE);
    notices.deliver(0, message(thread, 1, 1), None, |_, _| false);
    assert_eq!(notices.click_target(Some(payload(PRIVATE, 0))), None);
    let cleared = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cleared);
    notices.clear(Box::new(move || observed.store(true, Ordering::Relaxed)));
    let calls = calls.borrow();
    assert_eq!(calls.authorizations, 0);
    assert!(calls.banners.is_empty());
    assert_eq!(calls.cleared, 1);
    assert!(cleared.load(Ordering::Relaxed));
}

/// Review A6: a quit between the sign-in and the system's settings reply
/// leaves the next launch with a restored session (`new_login: None`), which
/// still gets the prompt.
#[test]
fn a_restart_before_the_settings_reply_still_gets_the_prompt() {
    let (mut first_launch, calls) = recording();
    first_launch.signed_in(0);
    assert_eq!(calls.borrow().authorizations, 0, "no settings reply yet");
    drop(first_launch);
    let (mut restored, calls) = recording();
    restored.signed_in(0);
    restored.apply_settings(system(Authorization::NotDetermined));
    assert_eq!(calls.borrow().authorizations, 1);
}

#[test]
fn settings_can_ask_only_while_the_system_has_not() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.request_authorization();
    assert_eq!(calls.borrow().authorizations, 0, "state still unknown");
    notices.apply_settings(system(Authorization::NotDetermined));
    assert_eq!(calls.borrow().authorizations, 1, "the sign-in's own prompt");
    notices.authorization_finished();
    notices.apply_settings(system(Authorization::NotDetermined));
    assert!(notices.can_request_authorization());
    notices.request_authorization();
    assert_eq!(calls.borrow().authorizations, 2);
    assert!(!notices.can_request_authorization(), "asked: waiting");
    notices.authorization_finished();
    notices.apply_settings(system(Authorization::NotDetermined));
    notices.request_authorization();
    assert_eq!(
        calls.borrow().authorizations,
        3,
        "asks again if still undecided"
    );
    for decided in [Authorization::Allowed, Authorization::Denied] {
        notices.apply_settings(system(decided));
        assert!(!notices.can_request_authorization());
        notices.request_authorization();
        assert_eq!(calls.borrow().authorizations, 3);
    }
}

/// Review A30: the control and its handler use the same active delivery epoch
/// as the automatic prompt. Signed out (Welcome), after the account ended, or
/// on a removal-only center (the epoch seed failed), nothing can ask.
#[test]
fn settings_cannot_ask_without_an_account_or_a_delivery_epoch() {
    let (mut signed_out, calls) = recording();
    signed_out.apply_settings(system(Authorization::NotDetermined));
    assert!(!signed_out.can_request_authorization());
    signed_out.request_authorization();
    assert_eq!(calls.borrow().authorizations, 0, "nobody is signed in");
    signed_out.signed_in(0);
    signed_out.authorization_finished();
    signed_out.apply_settings(system(Authorization::NotDetermined));
    assert!(signed_out.can_request_authorization(), "signed in: it can");
    signed_out.end_account();
    assert!(!signed_out.can_request_authorization(), "account ended");
    signed_out.request_authorization();
    assert_eq!(calls.borrow().authorizations, 1, "only the sign-in's own");

    let calls = Rc::new(RefCell::new(Calls::default()));
    let mut removal_only = Notifications::new(
        Box::new(Recording(Rc::clone(&calls))),
        None,
        BadgePreference::On,
    );
    removal_only.apply_settings(system(Authorization::NotDetermined));
    removal_only.signed_in(0);
    assert!(!removal_only.can_request_authorization());
    removal_only.request_authorization();
    assert_eq!(calls.borrow().authorizations, 0, "no epoch: removal only");
}

#[gpui::test]
fn enable_notifications_from_welcome_never_asks_the_system(cx: &mut TestAppContext) {
    let (root, cx, _harness) = open(cx);
    let (mut notifications, calls) = recording();
    notifications.apply_settings(system(Authorization::NotDetermined));
    root.update(cx, |root, _| root.notifications = Some(notifications));
    root.update_in(cx, |root, _, cx| root.enable_notifications(cx));
    cx.run_until_parked();
    assert_eq!(calls.borrow().authorizations, 0, "Welcome: no account");
}

#[gpui::test]
fn settings_offers_enable_notifications_and_asks_once_clicked(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    let (mut notifications, calls) = recording();
    notifications.apply_settings(system(Authorization::NotDetermined));
    root.update(cx, |root, _| root.notifications = Some(notifications));
    core_says(&harness, cx, signed_in());
    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();
    // Signed in and not asked: the prompt already fired from the sign-in;
    // undo it to stand for a system that has not shown it (a dismissed one).
    assert_eq!(calls.borrow().authorizations, 1);
    root.update(cx, |root, cx| {
        if let Some(n) = &mut root.notifications {
            n.authorization_finished();
            n.apply_settings(system(Authorization::NotDetermined));
        }
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("enable-notifications").is_some(),
        "Settings shows Enable notifications while NotDetermined"
    );
    root.update_in(cx, |root, _, cx| root.enable_notifications(cx));
    cx.run_until_parked();
    assert_eq!(calls.borrow().authorizations, 2);
    assert!(
        cx.debug_bounds("enable-notifications").is_none(),
        "gone once the system is asked"
    );
}

fn message(thread: ThreadKey, msg: i32, count: u32) -> Notice {
    Notice::Message {
        chat: thread.chat,
        topic: thread.topic,
        msg: MsgId(msg),
        chat_title: "Chat".into(),
        sender: Some("Sender".into()),
        count,
    }
}

#[test]
fn visible_threads_stale_epochs_and_confirmed_reads_do_not_deliver() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let thread = ThreadKey {
        chat: FORUM,
        topic: Some(TopicId::GENERAL),
    };
    let other = ThreadKey {
        chat: FORUM,
        topic: Some(TopicId::try_from(7).unwrap()),
    };
    notices.deliver(0, message(thread, 20, 1), Some(thread), |_, _| false);
    notices.deliver(1, message(thread, 20, 1), None, |_, _| false);
    notices.deliver(0, message(thread, 20, 1), None, |_, _| true);
    assert!(calls.borrow().banners.is_empty());

    notices.deliver(0, message(thread, 20, 1), Some(other), |_, _| false);
    notices.deliver(0, message(other, 21, 3), None, |_, _| false);
    notices.deliver(0, message(other, 22, 1), None, |_, _| false);
    {
        let calls = calls.borrow();
        let bodies: Vec<_> = calls
            .banners
            .iter()
            .map(|banner| banner.body.as_str())
            .collect();
        assert_eq!(bodies, ["New message", "3 new messages", "New message"]);
        assert_eq!(calls.banners[0].title, "Chat");
        assert_eq!(calls.banners[0].subtitle.as_deref(), Some("Sender"));
    }
    notices.read(other, MsgId(21));
    notices.read(ThreadKey::whole(FORUM), MsgId(22));
    let calls = calls.borrow();
    assert_eq!(
        calls.removed,
        [
            calls.banners[1].receipt,
            calls.banners[0].receipt,
            calls.banners[2].receipt
        ]
    );
}

#[test]
fn a_catch_up_summary_has_generic_copy_and_no_chat_target() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    notices.deliver(0, Notice::Summary, None, |_, _| false);
    let calls = calls.borrow();
    assert_eq!(calls.banners.len(), 1);
    let banner = &calls.banners[0];
    assert_eq!(banner.title, "abyss");
    assert_eq!(banner.subtitle, None);
    assert_eq!(banner.body, "New messages while you were away");
    assert_eq!(notices.click_target(Some(banner.receipt.payload)), None);
}

#[test]
fn dock_badge_counts_unread_non_muted_chats_caps_and_clears() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let mut unread = summary(PRIVATE, "Unread");
    unread.read.unread = 15;
    unread.last_id = Some(MsgId(5));
    unread.reading = unread.last_id;
    let mut muted = summary(FORUM, "Muted");
    muted.read.unread = 7;
    muted.scope_muted_until = 10;
    let read = summary(ChatId { id: 3, ..PRIVATE }, "Read");
    assert_eq!(
        notices.refresh_badge(&[unread.clone(), muted.clone(), read], 9),
        Some(10)
    );
    assert_eq!(notices.refresh_badge(&[unread.clone(), muted], 10), None);
    let all: Vec<_> = (1..=100)
        .map(|id| ChatSummary {
            id: ChatId { id, ..PRIVATE },
            ..unread.clone()
        })
        .collect();
    notices.refresh_badge(&all, 10);
    let ack = Arc::new(AtomicBool::new(false));
    let completion = Arc::clone(&ack);
    notices.clear(Box::new(move || completion.store(true, Ordering::Release)));
    assert!(ack.load(Ordering::Acquire));
    assert_eq!(calls.borrow().cleared, 1);
    assert_eq!(
        calls.borrow().badge,
        [Some("1".into()), Some("2".into()), Some("99+".into()), None]
    );
    assert!(notices.click_target(Some(payload(PRIVATE, 0))).is_none());
}

#[gpui::test]
fn closing_a_signed_in_window_hides_but_quit_starts_shutdown(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    core_says(&harness, cx, signed_in());
    let mut hidden = false;
    root.update_in(cx, |root, window, cx| {
        assert!(!root.request_window_close(window, cx, |_| hidden = true));
    });
    assert!(hidden);
    assert_eq!(closing(&root, cx), Some(Closing::Open));
    root.update_in(cx, |root, window, cx| {
        assert!(!root.request_quit(window, cx));
    });
    assert_eq!(closing(&root, cx), Some(Closing::Waiting));
}

#[gpui::test]
fn welcome_close_starts_shutdown(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    core_says(
        &harness,
        cx,
        AuthState::NeedCredentials {
            error: None,
            source: None,
        },
    );
    assert!(!cx.simulate_close());
    assert_eq!(closing(&root, cx), Some(Closing::Waiting));
}

#[gpui::test]
fn sign_in_close_starts_shutdown(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    core_says(&harness, cx, AuthState::Phone { error: None });
    assert!(!cx.simulate_close());
    assert_eq!(closing(&root, cx), Some(Closing::Waiting));
}

#[gpui::test]
fn deletion_events_remove_delivered_banners_by_identifier(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    let (mut notifications, calls) = recording();
    notifications.apply_settings(system(Authorization::Allowed));
    root.update(cx, |root, _| root.notifications = Some(notifications));
    core_says(&harness, cx, signed_in());

    // Account-wide ids can overlap with ids in a channel.
    let forum = ThreadKey {
        chat: FORUM,
        topic: Some(TopicId::GENERAL),
    };
    for (thread, id) in [(ThreadKey::whole(PRIVATE), 5), (forum, 5), (forum, 6)] {
        harness
            .events
            .unbounded_send(Event::Notify(message(thread, id, 1)))
            .unwrap();
    }
    cx.run_until_parked();
    let identifiers: Vec<_> = calls
        .borrow()
        .banners
        .iter()
        .map(|banner| banner.receipt.identifier())
        .collect();
    assert_eq!(identifiers.len(), 3);

    for (chat, id) in [(None, MsgId(5)), (Some(FORUM), MsgId(6))] {
        harness
            .events
            .unbounded_send(Event::NotifyDeleted {
                chat,
                ids: vec![id],
            })
            .unwrap();
    }
    cx.run_until_parked();
    let removed: Vec<_> = calls
        .borrow()
        .removed
        .iter()
        .map(|receipt| receipt.identifier())
        .collect();
    assert_eq!(removed, [identifiers[0].clone(), identifiers[2].clone()]);
}

const NEWCOMER: ChatId = ChatId {
    kind: PeerKind::User,
    id: 7,
};
const UNLISTED_FORUM: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 9,
};

/// A signed-in root whose center is allowed; no dialogs are listed.
fn allowed_root(
    cx: &mut TestAppContext,
) -> (
    Entity<Root>,
    &mut VisualTestContext,
    Harness,
    Rc<RefCell<Calls>>,
) {
    let (root, cx, harness) = open(cx);
    let (mut notifications, calls) = recording();
    notifications.apply_settings(system(Authorization::Allowed));
    root.update(cx, |root, _| root.notifications = Some(notifications));
    core_says(&harness, cx, signed_in());
    (root, cx, harness, calls)
}

/// Core notifies about `thread`; the system accepts the banner.
fn notify(root: &Entity<Root>, harness: &Harness, cx: &mut VisualTestContext, thread: ThreadKey) {
    harness
        .events
        .unbounded_send(Event::Notify(message(thread, 5, 1)))
        .unwrap();
    cx.run_until_parked();
    let receipt = root.update(cx, |root, _| {
        root.notifications
            .as_ref()
            .and_then(|n| n.submitted_receipts().last().copied())
    });
    if let Some(receipt) = receipt {
        root.update_in(cx, |root, window, cx| {
            root.on_system_event(
                SystemEvent::Delivered {
                    receipt,
                    outcome: Delivery::Accepted,
                },
                window,
                cx,
            );
        });
    }
}

fn click(root: &Entity<Root>, cx: &mut VisualTestContext, payload: Option<Payload>) {
    root.update_in(cx, |root, window, cx| {
        root.activate_notification(payload, window, cx, |_, _| {});
    });
}

/// Review A5: core notifies about chats the dialogs list doesn't hold (a new
/// message doesn't list one), and the click opens what the app delivered.
#[gpui::test]
fn a_delivered_banner_for_an_unlisted_chat_opens_it(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    assert!(read(&root, cx, |s| s.shell.list.get(NEWCOMER).is_none()));
    notify(&root, &harness, cx, ThreadKey::whole(NEWCOMER));
    let banner = calls.borrow().banners.first().map(|b| b.receipt.payload);
    assert_eq!(banner, Some(payload(NEWCOMER, 0)), "the app delivered it");
    click(&root, cx, banner);
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(NEWCOMER))
    );
    let title = read(&root, cx, |s| {
        s.shell.list.get(NEWCOMER).map(|c| c.title.clone())
    });
    assert_eq!(title.as_deref(), Some("Chat"), "the banner's title");
    assert!(
        matches!(
            harness.sent.borrow().commands.last(),
            Some(Command::OpenThread(t) | Command::OpenThreadAt { thread: t, .. })
                if *t == ThreadKey::whole(NEWCOMER)
        ),
        "core was asked to open it"
    );
}

#[gpui::test]
fn a_delivered_banner_for_an_unlisted_forum_topic_opens_the_topic(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let thread = ThreadKey {
        chat: UNLISTED_FORUM,
        topic: Some(TopicId::GENERAL),
    };
    notify(&root, &harness, cx, thread);
    let banner = calls.borrow().banners.first().map(|b| b.receipt.payload);
    click(&root, cx, banner);
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(thread));
}

#[gpui::test]
fn an_undelivered_or_malformed_target_still_only_activates(cx: &mut TestAppContext) {
    let (root, cx, harness, _calls) = allowed_root(cx);
    // A well-formed payload for the current epoch that no banner was
    // delivered for, a stale epoch, and malformed ones.
    notify(&root, &harness, cx, ThreadKey::whole(NEWCOMER));
    for payload in [
        Some(payload(UNLISTED_FORUM, 1)),
        Some(Payload {
            epoch: EPOCH - 1,
            ..payload(NEWCOMER, 0)
        }),
        Some(payload(PRIVATE, 1)),
        Some(Payload {
            chat: 99,
            ..payload(NEWCOMER, 0)
        }),
        None,
    ] {
        click(&root, cx, payload);
        assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
        assert!(read(&root, cx, |s| s
            .shell
            .list
            .get(UNLISTED_FORUM)
            .is_none()));
    }
}

#[test]
fn delivery_evidence_is_bounded_and_ends_with_the_account() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let chat = |id| {
        ThreadKey::whole(ChatId {
            kind: PeerKind::User,
            id,
        })
    };
    for id in 1..=200 {
        submit(&mut notices, &calls, chat(id), 5, Delivery::Accepted);
    }
    assert!(
        notices.provisional(chat(1), 0).is_none(),
        "oldest forgotten"
    );
    assert!(notices.provisional(chat(200), 0).is_some());
    notices.end_account();
    assert!(
        notices.provisional(chat(200), 0).is_none(),
        "ended with the account"
    );
    notices.signed_in(1);
    assert!(
        notices.provisional(chat(200), 0).is_none(),
        "not across epochs"
    );
}

/// Delivers a banner for `thread` and reports the system's answer: `ok`, or a
/// native failure.
fn submit(
    notices: &mut Notifications,
    calls: &Rc<RefCell<Calls>>,
    thread: ThreadKey,
    msg: i32,
    outcome: Delivery,
) -> Receipt {
    notices.deliver(0, message(thread, msg, 1), None, |_, _| false);
    let receipt = calls.borrow().banners.last().map(|b| b.receipt).unwrap();
    notices.delivered(receipt, outcome);
    receipt
}

/// Review A40: a native failure commits nothing. A failed request neither
/// evicts the evidence of banners still on screen (the oldest tracked
/// notification, the oldest click target) nor leaves evidence of its own.
#[test]
fn a_failed_submission_leaves_the_existing_evidence_intact() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let chat = |id| ThreadKey::whole(ChatId { id, ..PRIVATE });
    // Fill both bounds: 512 tracked notifications, 128 click targets.
    let first = submit(&mut notices, &calls, chat(1), 5, Delivery::Accepted);
    for id in 2..=512 {
        submit(&mut notices, &calls, chat(id), 5, Delivery::Accepted);
    }
    assert!(notices.provisional(chat(512), 0).is_some());
    let oldest_target = chat(512 - 127);
    assert!(notices.provisional(oldest_target, 0).is_some());
    assert!(notices.provisional(chat(512 - 128), 0).is_none());

    for id in 600..650 {
        let failed = submit(&mut notices, &calls, chat(id), 5, Delivery::Failed);
        assert!(notices.provisional(chat(id), 0).is_none());
        assert_eq!(notices.click_target(Some(failed.payload)), Some(chat(id)));
    }
    assert!(calls.borrow().removed.is_empty(), "nothing was evicted");
    assert!(notices.provisional(oldest_target, 0).is_some());
    notices.read(chat(1), MsgId(5));
    assert_eq!(
        calls.borrow().removed,
        [first],
        "the first is still tracked"
    );

    // Successes past the bounds evict one each, as before. The read freed a
    // notification slot, so the first of them only fills it.
    submit(&mut notices, &calls, chat(700), 5, Delivery::Accepted);
    assert_eq!(calls.borrow().removed.len(), 1);
    assert!(
        notices.provisional(oldest_target, 0).is_none(),
        "targets full"
    );
    submit(&mut notices, &calls, chat(701), 5, Delivery::Accepted);
    assert_eq!(calls.borrow().removed.len(), 2);
}

/// A read or deletion that arrives while the system is still taking a banner
/// removes it too; a late answer for a removed banner commits nothing, and an
/// answer from a finished account is ignored.
#[test]
fn a_banner_removed_in_flight_is_never_committed() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let thread = ThreadKey::whole(PRIVATE);
    notices.deliver(0, message(thread, 5, 1), None, |_, _| false);
    let receipt = calls.borrow().banners[0].receipt;
    notices.read(thread, MsgId(5));
    assert_eq!(calls.borrow().removed, [receipt]);
    notices.delivered(receipt, Delivery::Accepted);
    assert!(notices.provisional(thread, 0).is_none());
    notices.read(thread, MsgId(6));
    assert_eq!(
        calls.borrow().removed.len(),
        1,
        "not tracked: nothing to do"
    );

    notices.deliver(0, message(thread, 7, 1), None, |_, _| false);
    let late = calls.borrow().banners[1].receipt;
    notices.end_account();
    notices.signed_in(1);
    notices.delivered(late, Delivery::Accepted);
    assert!(notices.provisional(thread, 0).is_none(), "an old epoch");
}

const FULL_ROW_BASE: i64 = 1_000;

/// Review A25: a stand-in never says a chat can be written to: a group that
/// restricts posting still delivers an incoming banner.
#[gpui::test]
fn a_stand_in_opened_from_a_banner_is_read_only(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let group = ChatId {
        kind: PeerKind::Chat,
        id: 11,
    };
    for thread in [
        ThreadKey::whole(NEWCOMER),
        ThreadKey::whole(group),
        ThreadKey {
            chat: UNLISTED_FORUM,
            topic: Some(TopicId::GENERAL),
        },
    ] {
        notify(&root, &harness, cx, thread);
        let banner = calls.borrow().banners.last().map(|b| b.receipt.payload);
        click(&root, cx, banner);
        assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(thread));
        let can_send = read(&root, cx, |s| {
            s.shell.selected_chat().map(|chat| chat.can_send)
        });
        assert_eq!(
            can_send,
            Some(false),
            "{thread:?} is read-only until listed"
        );
    }
}

/// Review A25: the listed row is authoritative. A stale banner for a topic of
/// a chat that is no longer a forum opens nothing and replaces nothing.
#[gpui::test]
fn a_stale_topic_banner_never_replaces_a_listed_summary(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let thread = ThreadKey {
        chat: UNLISTED_FORUM,
        topic: Some(TopicId::GENERAL),
    };
    notify(&root, &harness, cx, thread);
    let banner = calls.borrow().banners.last().map(|b| b.receipt.payload);
    let mut newer = summary(UNLISTED_FORUM, "Newer summary");
    newer.can_send = true;
    harness
        .events
        .unbounded_send(Event::DialogsReset(vec![newer.clone()]))
        .unwrap();
    cx.run_until_parked();
    click(&root, cx, banner);
    assert_eq!(read(&root, cx, |s| s.shell.selected()), None);
    let kept = read(&root, cx, |s| s.shell.list.listed(UNLISTED_FORUM).cloned());
    assert_eq!(kept, Some(newer), "the listed summary is untouched");
}

/// Review A27: a full sidebar cannot lose an evidenced click.
#[gpui::test]
fn a_full_sidebar_still_opens_an_evidenced_banner(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let full: Vec<_> = (0..abyss_core::store::MAX_DIALOGS)
        .map(|n| {
            let id = ChatId {
                kind: PeerKind::User,
                id: FULL_ROW_BASE + i64::try_from(n).unwrap(),
            };
            summary(id, "Listed")
        })
        .collect();
    harness
        .events
        .unbounded_send(Event::DialogsReset(full))
        .unwrap();
    cx.run_until_parked();
    assert!(read(&root, cx, |s| s.shell.list.len()
        == abyss_core::store::MAX_DIALOGS));
    for thread in [
        ThreadKey::whole(NEWCOMER),
        ThreadKey {
            chat: UNLISTED_FORUM,
            topic: Some(TopicId::GENERAL),
        },
    ] {
        notify(&root, &harness, cx, thread);
        let banner = calls.borrow().banners.last().map(|b| b.receipt.payload);
        click(&root, cx, banner);
        assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(thread));
        assert_eq!(
            read(&root, cx, |s| s.shell.list.len()),
            abyss_core::store::MAX_DIALOGS,
            "the sidebar's cap holds: the stand-in sits in its own slot"
        );
    }
    // The first list that holds the chat drops the stand-in.
    harness
        .events
        .unbounded_send(Event::ChatBumped(summary(UNLISTED_FORUM, "Real")))
        .unwrap();
    cx.run_until_parked();
    assert!(read(&root, cx, |s| s
        .shell
        .list
        .listed(UNLISTED_FORUM)
        .is_none()));
}

fn existing(chat: ChatId, topic: i64, msg: i32, epoch: i64) -> Receipt {
    Receipt::from_identifier(&format!(
        "{epoch}:{}:{topic}:{msg}",
        chat.dialog_id().unwrap_or_default()
    ))
    .unwrap()
}

/// Review A19: a confirmed deletion that arrives before the launch's
/// enumeration of delivered notifications must still remove what that
/// enumeration then reports, in either order.
#[test]
fn a_deletion_before_the_enumeration_still_removes_the_old_banner() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.deleted(Some(FORUM), &[MsgId(6)]);
    notices.deleted(None, &[MsgId(5)]);
    let gone = [existing(PRIVATE, 0, 5, old), existing(FORUM, 1, 6, old)];
    let stays = [existing(PRIVATE, 0, 8, old), existing(FORUM, 1, 5, old)];
    notices.existing(gone.iter().chain(&stays).copied().collect());
    assert_eq!(calls.borrow().removed, gone);

    // The other order: the enumeration first, then the deletion.
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.existing(gone.iter().chain(&stays).copied().collect());
    assert!(calls.borrow().removed.is_empty());
    notices.deleted(Some(FORUM), &[MsgId(6)]);
    notices.deleted(None, &[MsgId(5)]);
    assert_eq!(calls.borrow().removed, [gone[1], gone[0]]);
}

#[test]
fn deletion_evidence_is_bounded_and_overflow_removes_what_it_cannot_vouch_for() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    let ids: Vec<_> = (1..=2_000).map(MsgId).collect();
    notices.deleted(Some(PRIVATE), &ids);
    let enumerated = [
        existing(FORUM, 1, 5_000, old),
        existing(PRIVATE, 0, 9_999, old),
    ];
    notices.existing(enumerated.to_vec());
    assert_eq!(
        calls.borrow().removed,
        enumerated,
        "evidence was incomplete"
    );
    // Done once: later enumerations and deletions are the ordinary path.
    notices.deleted(Some(PRIVATE), &ids);
    assert_eq!(calls.borrow().removed.len(), 2);
}

#[gpui::test]
fn a_deletion_event_before_the_enumeration_removes_the_banner_through_the_window(
    cx: &mut TestAppContext,
) {
    let (root, cx, harness, calls) = allowed_root(cx);
    harness
        .events
        .unbounded_send(Event::NotifyDeleted {
            chat: None,
            ids: vec![MsgId(5)],
        })
        .unwrap();
    cx.run_until_parked();
    let old = existing(PRIVATE, 0, 5, EPOCH - 1);
    let kept = existing(PRIVATE, 0, 6, EPOCH - 1);
    root.update_in(cx, |root, window, cx| {
        root.on_system_event(SystemEvent::Existing(vec![old, kept]), window, cx);
    });
    assert_eq!(calls.borrow().removed, [old]);
}

const SECOND: ChatId = ChatId {
    kind: PeerKind::User,
    id: 8,
};

/// Review A39: a banner click that core refuses must not take the open
/// stand-in's place: chat A stays open with its summary, and the provisional
/// slot holds A, not the refused B.
#[gpui::test]
fn a_refused_banner_click_keeps_the_open_stand_in(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    notify(&root, &harness, cx, ThreadKey::whole(NEWCOMER));
    notify(&root, &harness, cx, ThreadKey::whole(SECOND));
    let (first, second) = {
        let calls = calls.borrow();
        (
            calls.banners[0].receipt.payload,
            calls.banners[1].receipt.payload,
        )
    };
    click(&root, cx, Some(first));
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(NEWCOMER))
    );

    harness.sent.borrow_mut().full = true;
    click(&root, cx, Some(second));
    cx.run_until_parked();
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(NEWCOMER)),
        "refused: the pane stays on A"
    );
    let title = read(&root, cx, |s| {
        s.shell.selected_chat().map(|chat| chat.title.clone())
    });
    assert_eq!(title.as_deref(), Some("Chat"), "A still has its summary");
    assert!(read(&root, cx, |s| s.shell.list.get(SECOND).is_none()));

    // Admitted, B takes the slot.
    harness.sent.borrow_mut().full = false;
    click(&root, cx, Some(second));
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(SECOND))
    );
}

/// Review A36: a read that arrives before the launch's enumeration of
/// delivered banners, for a chat the lists don't hold, must still remove what
/// the enumeration reports: the floors wait with the deletion evidence.
#[test]
fn a_read_before_the_enumeration_still_removes_the_old_banner() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    let topic = TopicId::try_from(7).unwrap();
    notices.read(ThreadKey::whole(PRIVATE), MsgId(10));
    notices.read(
        ThreadKey {
            chat: FORUM,
            topic: Some(topic),
        },
        MsgId(20),
    );
    // A later, lower pointer never lowers the floor.
    notices.read(ThreadKey::whole(PRIVATE), MsgId(3));
    let gone = [
        existing(PRIVATE, 0, 10, old),
        existing(PRIVATE, 0, 4, old),
        existing(FORUM, 7, 20, old),
    ];
    let stays = [
        existing(PRIVATE, 0, 11, old),
        existing(FORUM, 7, 21, old),
        existing(FORUM, 8, 5, old),
        existing(FORUM, 0, 5, old),
    ];
    notices.existing(gone.iter().chain(&stays).copied().collect());
    assert_eq!(calls.borrow().removed, gone);

    // A chat's own pointer covers its topics' banners.
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.read(ThreadKey::whole(FORUM), MsgId(30));
    let covered = existing(FORUM, 8, 30, old);
    notices.existing(vec![covered, existing(FORUM, 8, 31, old)]);
    assert_eq!(calls.borrow().removed, [covered]);
}

#[test]
fn read_floors_are_bounded_and_overflow_removes_what_they_cannot_vouch_for() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    for id in 1..=2_000 {
        notices.read(ThreadKey::whole(ChatId { id, ..PRIVATE }), MsgId(1));
    }
    let enumerated = [
        existing(FORUM, 1, 5_000, old),
        existing(
            ChatId {
                id: 9_999,
                ..PRIVATE
            },
            0,
            7,
            old,
        ),
    ];
    notices.existing(enumerated.to_vec());
    assert_eq!(calls.borrow().removed, enumerated, "evidence incomplete");
}

#[gpui::test]
fn an_inbox_read_before_the_enumeration_removes_the_banner_through_the_window(
    cx: &mut TestAppContext,
) {
    let (root, cx, harness, calls) = allowed_root(cx);
    harness
        .events
        .unbounded_send(Event::InboxRead {
            thread: ThreadKey::whole(NEWCOMER),
            max_id: MsgId(9),
        })
        .unwrap();
    cx.run_until_parked();
    let old = existing(NEWCOMER, 0, 5, EPOCH - 1);
    let kept = existing(NEWCOMER, 0, 10, EPOCH - 1);
    root.update_in(cx, |root, window, cx| {
        root.on_system_event(SystemEvent::Existing(vec![old, kept]), window, cx);
    });
    assert_eq!(calls.borrow().removed, [old]);
}

/// Review A48: the logout confirmation is modal. A banner click for another
/// listed, sendable chat only activates the window: it must not move
/// keyboard focus to that chat's composer (which restores a saved draft)
/// behind the confirmation, where Return would send it.
#[gpui::test]
fn a_banner_click_during_the_logout_confirmation_only_activates(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    harness
        .events
        .unbounded_send(Event::DialogsReset(vec![
            summary(PRIVATE, "Private"),
            summary(SECOND, "Second"),
        ]))
        .unwrap();
    cx.run_until_parked();
    notify(&root, &harness, cx, ThreadKey::whole(SECOND));
    let banner = calls.borrow().banners.first().map(|b| b.receipt.payload);
    // SECOND gets a saved draft, then PRIVATE is the open chat.
    act(&root, cx, |s, window, cx| s.select_chat(SECOND, window, cx));
    cx.simulate_input("saved draft");
    act(&root, cx, |s, window, cx| {
        s.select_chat(PRIVATE, window, cx);
    });
    assert_eq!(composer_text(&root, cx), "");

    act(&root, cx, Session::logout);
    assert!(read(&root, cx, Session::confirming_logout));
    let from = sent_count(&harness);
    click(&root, cx, banner);
    cx.run_until_parked();
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(PRIVATE)),
        "the confirmation keeps the open chat"
    );
    assert!(read(&root, cx, Session::confirming_logout), "still asking");
    assert_eq!(composer_text(&root, cx), "", "the draft stays stashed");
    cx.simulate_keystrokes("enter");
    let sent = commands_since(&harness, from);
    assert!(
        sent.iter().all(|c| !c.starts_with("SendText")),
        "Return must not send the target's draft: {sent:?}"
    );

    // Once the confirmation is gone, the same banner opens its chat.
    act(&root, cx, Session::cancel_logout);
    click(&root, cx, banner);
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(SECOND))
    );
    assert_eq!(composer_text(&root, cx), "saved draft");
}

/// A refused topic click preserves the pane and its current stand-in.
#[gpui::test]
fn a_refused_topic_click_keeps_the_open_stand_in(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let topic = ThreadKey {
        chat: UNLISTED_FORUM,
        topic: Some(TopicId::GENERAL),
    };
    notify(&root, &harness, cx, ThreadKey::whole(NEWCOMER));
    notify(&root, &harness, cx, topic);
    let (first, second) = {
        let calls = calls.borrow();
        (
            calls.banners[0].receipt.payload,
            calls.banners[1].receipt.payload,
        )
    };
    click(&root, cx, Some(first));
    assert_eq!(
        read(&root, cx, |s| s.shell.selected()),
        Some(ThreadKey::whole(NEWCOMER))
    );

    harness.sent.borrow_mut().refuse = Some(|c| matches!(c, Command::OpenForumTopic { .. }));
    let from = sent_count(&harness);
    click(&root, cx, Some(second));
    cx.run_until_parked();
    let sent = commands_since(&harness, from);
    assert_eq!(
        read(&root, cx, |s| s.shell.pane()),
        Pane::Thread(ThreadKey::whole(NEWCOMER)),
        "refused: the pane stays on A ({sent:?})"
    );
    let title = read(&root, cx, |s| {
        s.shell.selected_chat().map(|chat| chat.title.clone())
    });
    assert_eq!(title.as_deref(), Some("Chat"), "A still has its summary");
    assert!(read(&root, cx, |s| s
        .shell
        .list
        .get(UNLISTED_FORUM)
        .is_none()));
    assert!(
        sent.iter()
            .all(|c| !c.starts_with("OpenForum") && !c.starts_with("CloseForum")),
        "core admitted no forum navigation: {sent:?}"
    );

    // Admitted, the topic opens.
    harness.sent.borrow_mut().refuse = None;
    click(&root, cx, Some(second));
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(topic));
}

/// Review N49: a removal-only center never delivers and its badge is always
/// 0, so Settings does not offer the Dock badge switch.
#[gpui::test]
fn a_removal_only_center_has_no_dock_badge_switch(cx: &mut TestAppContext) {
    let (root, cx, _harness) = open(cx);
    let calls = Rc::new(RefCell::new(Calls::default()));
    let removal_only = Notifications::new(
        Box::new(Recording(Rc::clone(&calls))),
        None,
        BadgePreference::On,
    );
    root.update(cx, |root, _| root.notifications = Some(removal_only));
    assert!(!root.read_with(cx, |root, _| {
        crate::shell::appearance::notifications::Options::of(root).badge_switch_available()
    }));
    root.update(cx, |root, _| root.notifications = Some(recording().0));
    assert!(root.read_with(cx, |root, _| {
        crate::shell::appearance::notifications::Options::of(root).badge_switch_available()
    }));
}

/// Review A50 (a): a snapshot whose pointer covers a banner the system has
/// not answered for yet removes it, and the later success commits nothing.
#[test]
fn a_snapshot_reaches_a_banner_still_in_flight() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let thread = ThreadKey::whole(PRIVATE);
    notices.deliver(0, message(thread, 5, 1), None, |_, _| false);
    let receipt = calls.borrow().banners.last().map(|b| b.receipt).unwrap();
    assert!(calls.borrow().removed.is_empty());

    notices.read_snapshot([(thread, MsgId(5))].into_iter());
    assert_eq!(calls.borrow().removed, [receipt], "read while in flight");
    notices.delivered(receipt, Delivery::Accepted);
    assert!(
        notices.provisional(thread, 0).is_none(),
        "an already-read banner commits no evidence"
    );
    let calls = calls.borrow();
    assert_eq!(calls.removed, [receipt], "removed once, nothing tracked");
}

/// Review N44 (b): a topic snapshot that arrives before the launch's
/// enumeration is kept as a floor, because the shell holds only the shown
/// forum's topics: switching forums before the enumeration must not lose it.
#[test]
fn a_topic_snapshot_before_the_enumeration_is_kept_as_a_floor() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    let topic = |n| ThreadKey {
        chat: FORUM,
        topic: Some(TopicId::try_from(n).unwrap()),
    };
    notices.read_snapshot([(topic(7), MsgId(20)), (topic(8), MsgId(0))].into_iter());
    let covered = existing(FORUM, 7, 20, old);
    notices.existing(vec![
        covered,
        existing(FORUM, 7, 21, old),
        existing(FORUM, 8, 1, old),
    ]);
    assert_eq!(calls.borrow().removed, [covered]);

    // After the enumeration nothing is kept: the lists hold what they hold.
    notices.read_snapshot([(topic(9), MsgId(30))].into_iter());
    assert!(notices.early_is_empty());
}

/// Review A51: past 64 outstanding requests a new banner is not handed to the
/// system at all, so no request that was handed over loses its evidence: a
/// delayed first success is still tracked and still opens its chat.
#[test]
fn the_cap_on_unanswered_requests_is_applied_at_admission() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let chat = |id| {
        ThreadKey::whole(ChatId {
            kind: PeerKind::User,
            id,
        })
    };
    for id in 1..=64 {
        notices.deliver(0, message(chat(id), 5, 1), None, |_, _| false);
    }
    assert_eq!(calls.borrow().banners.len(), 64);
    // The 65th is refused, and nothing already handed over is touched.
    notices.deliver(0, message(chat(65), 5, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 64, "not handed over");
    assert!(calls.borrow().removed.is_empty());
    let first = calls.borrow().banners[0].receipt;
    notices.delivered(first, Delivery::Accepted);
    assert!(notices.provisional(chat(1), 0).is_some(), "first committed");
    assert!(notices.provisional(chat(65), 0).is_none());

    // An answer frees a slot: the next banner goes out.
    notices.deliver(0, message(chat(66), 5, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 65);
    // A failure frees one too, and evicts nothing.
    let second = calls.borrow().banners[1].receipt;
    notices.delivered(second, Delivery::Failed);
    notices.deliver(0, message(chat(67), 5, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 66);
    assert!(notices.provisional(chat(1), 0).is_some());
    assert!(calls.borrow().removed.is_empty());
}

/// Review A60 / N52: the handoff serves a click before an event, so a click
/// can run before the answer that confirms its own banner. The click still
/// opens the chat, from the request still outstanding for it, and the answer
/// that follows commits its evidence as usual.
#[gpui::test]
fn a_click_that_overtakes_its_delivery_answer_still_opens_the_chat(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let thread = ThreadKey::whole(NEWCOMER);
    harness
        .events
        .unbounded_send(Event::Notify(message(thread, 5, 1)))
        .unwrap();
    cx.run_until_parked();
    let receipt = calls.borrow().banners.first().map(|b| b.receipt).unwrap();

    // The answer was queued first, then the banner was clicked.
    let (system, clicks, events) = crate::notifications::channels();
    let answer = SystemEvent::Delivered {
        receipt,
        outcome: Delivery::Accepted,
    };
    assert!(system.try_send(answer).is_ok());
    clicks.push(Some(receipt.payload));
    let mut order = Vec::new();
    for _ in 0..2 {
        let event = futures::executor::block_on(events.recv()).unwrap();
        order.push(matches!(event, SystemEvent::Click(_)));
        root.update_in(cx, |root, window, cx| {
            root.on_system_event(event, window, cx);
        });
    }
    assert_eq!(order, [true, false], "the click went first");
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(thread));
    let known = root.read_with(cx, |root, _| {
        root.notifications
            .as_ref()
            .and_then(|n| n.provisional(thread, 0))
    });
    assert!(known.is_some(), "the answer committed its evidence");
}

/// Review A60: a banner nobody delivered (a click for a payload of this epoch
/// with no request outstanding), one already covered by a read, and one of a
/// finished account give a click nothing to open with.
#[test]
fn only_an_outstanding_uncovered_request_of_this_epoch_backs_a_click() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    let thread = ThreadKey::whole(PRIVATE);
    assert!(notices.provisional(thread, 0).is_none(), "never submitted");

    notices.deliver(0, message(thread, 5, 1), None, |_, _| false);
    let receipt = calls.borrow().banners[0].receipt;
    assert!(notices.provisional(thread, 0).is_some(), "outstanding");
    let other = ThreadKey::whole(ChatId { id: 2, ..PRIVATE });
    assert!(notices.provisional(other, 0).is_none(), "another thread");

    notices.read(thread, MsgId(5));
    assert!(
        notices.provisional(thread, 0).is_none(),
        "covered by a read"
    );
    notices.delivered(receipt, Delivery::Accepted);

    notices.deliver(0, message(thread, 6, 1), None, |_, _| false);
    assert!(notices.provisional(thread, 0).is_some());
    notices.end_account();
    notices.signed_in(1);
    assert!(notices.provisional(thread, 0).is_none(), "an old epoch");
}

fn user_chat(id: i64) -> ThreadKey {
    ThreadKey::whole(ChatId {
        kind: PeerKind::User,
        id,
    })
}

/// Review A59: a read or a deletion removes the banner from the center, but
/// the native call is still outstanding until its answer arrives. The slot
/// stays taken, for summaries as for messages.
#[test]
fn a_covered_request_keeps_its_slot_until_the_system_answers() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    for id in 1..=64 {
        notices.deliver(0, message(user_chat(id), 5, 1), None, |_, _| false);
    }
    assert_eq!(calls.borrow().banners.len(), 64);
    for id in 1..=64 {
        notices.read(user_chat(id), MsgId(5));
    }
    assert_eq!(calls.borrow().removed.len(), 64);

    notices.deliver(0, message(user_chat(65), 5, 1), None, |_, _| false);
    notices.deliver(0, Notice::Summary, None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 64, "no slot is free yet");

    // One answer frees one slot, whether or not the banner was covered, and
    // a covered banner commits no click evidence.
    let first = calls.borrow().banners[0].receipt;
    notices.delivered(first, Delivery::Accepted);
    assert!(notices.provisional(user_chat(1), 0).is_none());
    notices.deliver(0, Notice::Summary, None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 65);
    notices.deliver(0, message(user_chat(65), 5, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 65, "the summary took it");
}

/// Review A59: a summary is a native call like any other, and its answer
/// (which names no chat) is what releases its slot.
#[test]
fn a_summary_counts_against_the_cap_until_its_answer() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    notices.deliver(0, Notice::Summary, None, |_, _| false);
    let summary = calls.borrow().banners[0].receipt;
    for id in 1..=63 {
        notices.deliver(0, message(user_chat(id), 5, 1), None, |_, _| false);
    }
    assert_eq!(calls.borrow().banners.len(), 64);
    notices.deliver(0, message(user_chat(64), 5, 1), None, |_, _| false);
    notices.deliver(0, Notice::Summary, None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 64, "full, summary included");

    notices.delivered(summary, Delivery::Accepted);
    notices.deliver(0, message(user_chat(64), 5, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 65);
}

/// Review A59: answers that outlive their account still release their slots.
#[test]
fn answers_after_the_account_ended_release_their_slots() {
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    notices.apply_settings(system(Authorization::Allowed));
    for id in 1..=64 {
        notices.deliver(0, message(user_chat(id), 5, 1), None, |_, _| false);
    }
    let receipts: Vec<_> = calls.borrow().banners.iter().map(|b| b.receipt).collect();
    notices.end_account();
    notices.signed_in(1);
    notices.deliver(1, message(user_chat(1), 6, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 64, "still outstanding");
    notices.delivered(receipts[0], Delivery::Failed);
    notices.deliver(1, message(user_chat(1), 6, 1), None, |_, _| false);
    assert_eq!(calls.borrow().banners.len(), 65);
}

/// Review A61: a whole-chat pointer in a dialogs snapshot is kept as an early
/// floor too. A later reset can drop the chat from the list before the
/// launch's enumeration returns its old banner.
#[test]
fn a_chat_snapshot_before_the_enumeration_is_kept_as_a_floor() {
    let old = EPOCH - 1;
    let (mut notices, calls) = recording();
    notices.signed_in(0);
    let (a, b) = (user_chat(1), user_chat(2));
    notices.read_snapshot([(a, MsgId(10)), (b, MsgId(0))].into_iter());
    // A reset that no longer lists A.
    notices.read_snapshot([(b, MsgId(0))].into_iter());
    let covered = existing(a.chat, 0, 5, old);
    notices.existing(vec![
        covered,
        existing(a.chat, 0, 11, old),
        existing(b.chat, 0, 1, old),
    ]);
    assert_eq!(calls.borrow().removed, [covered]);
    assert!(notices.early_is_empty());
}

/// Review A60 with A48: a click backed only by an unanswered request obeys
/// the logout confirmation like any other, and takes no provisional slot
/// while it is open.
#[gpui::test]
fn an_early_click_during_the_logout_confirmation_only_activates(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    let thread = ThreadKey::whole(NEWCOMER);
    harness
        .events
        .unbounded_send(Event::Notify(message(thread, 5, 1)))
        .unwrap();
    cx.run_until_parked();
    let banner = calls.borrow().banners.first().map(|b| b.receipt.payload);
    let before = read(&root, cx, |s| s.shell.selected());

    act(&root, cx, Session::logout);
    click(&root, cx, banner);
    cx.run_until_parked();
    assert_eq!(read(&root, cx, |s| s.shell.selected()), before);
    assert!(read(&root, cx, |s| s.shell.list.stand_in().is_none()));

    act(&root, cx, Session::cancel_logout);
    click(&root, cx, banner);
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(thread));
}

/// Reviews A64, N57: a banner for the open chat is suppressed only while its
/// messages are on screen. Any modal overlay covers them.
#[gpui::test]
fn banners_for_the_open_chat_show_behind_any_modal_overlay(cx: &mut TestAppContext) {
    let (root, cx, harness, calls) = allowed_root(cx);
    harness
        .events
        .unbounded_send(Event::DialogsReset(vec![summary(PRIVATE, "Private")]))
        .unwrap();
    cx.run_until_parked();
    act(&root, cx, |s, window, cx| {
        s.select_chat(PRIVATE, window, cx);
    });
    let open = ThreadKey::whole(PRIVATE);
    key_window(cx);

    notify(&root, &harness, cx, open);
    assert!(calls.borrow().banners.is_empty(), "on screen: no banner");

    // Settings, then the logout confirmation. (Closing delivers nothing.)
    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();
    notify(&root, &harness, cx, open);
    assert_eq!(calls.borrow().banners.len(), 1, "behind Settings");
    root.update_in(cx, Root::toggle_appearance);
    cx.run_until_parked();

    act(&root, cx, Session::logout);
    notify(&root, &harness, cx, open);
    assert_eq!(calls.borrow().banners.len(), 2, "behind the confirmation");
    act(&root, cx, Session::cancel_logout);

    notify(&root, &harness, cx, open);
    assert_eq!(calls.borrow().banners.len(), 2, "uncovered: no banner");
}

/// Makes the test window the key window. The test platform's windows start
/// inactive, and activating the window that is already the platform's active
/// one does nothing, so another window takes it first.
fn key_window(cx: &mut VisualTestContext) {
    use gpui::AppContext as _;

    cx.update(|_, cx| {
        cx.open_window(gpui::WindowOptions::default(), |_, cx| {
            cx.new(|_| gpui::Empty)
        })
        .expect("a second window");
    });
    cx.run_until_parked();
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
}
