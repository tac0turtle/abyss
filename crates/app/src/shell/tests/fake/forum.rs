//! Forums (Stage 2 task 10): the pane moves between the chat list, a
//! forum's topic list and a topic only once core admitted the command, Back
//! (the "‹ Topics" button and Cmd+[) shows the kept list at once, and a
//! flood wait on topics counts down and then sends the load it interrupted,
//! once. The last test drives the real core over the `cargo dev` demo data.

use std::time::{Duration, Instant};

use abyss_core::api::{LoadTarget, UserError};
use abyss_core::model::{
    ChatKind, ChatSummary, Freshness, MsgId, Topic, TopicGone, TopicId, TopicOrder,
};
use abyss_core::store::TopicsGen;

use super::*;
use crate::chat::Pane;
use crate::chat::list::Pages;

const FORUM: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 100,
};

fn tid(n: i32) -> TopicId {
    TopicId::try_from(n).unwrap()
}

fn topic_key(n: i32) -> ThreadKey {
    ThreadKey {
        chat: FORUM,
        topic: Some(tid(n)),
    }
}

fn topic(n: i32) -> Topic {
    Topic {
        id: tid(n),
        title: format!("topic {n}"),
        created: i64::from(n),
        closed: false,
        pinned: false,
        unread: 0,
        unread_mentions: 0,
        freshness: Freshness::Current,
        read_inbox_max: MsgId(n),
        last: None,
        last_date: 0,
        last_id: Some(MsgId(n)),
    }
}

fn pinned(n: i32) -> Topic {
    Topic {
        pinned: true,
        ..topic(n)
    }
}

/// Signed in, with chats 1 and 2 and the forum listed, and chat 1 open.
fn forum_listed(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    cx.update(|cx| cx.bind_keys(crate::shell::key_bindings()));
    let (root, cx, harness) = chats_open(cx, 1);
    let mut dialogs = FakeScript::demo(2, 0).dialogs;
    dialogs.push(forum_chat(IS_FORUM));
    core_sends(&harness, cx, Event::DialogsReset(dialogs));
    (root, cx, harness)
}

const IS_FORUM: ChatKind = ChatKind::Supergroup {
    forum: true,
    gigagroup: false,
};
/// What the forum is once it stopped being one.
const NOT_FORUM: ChatKind = ChatKind::Supergroup {
    forum: false,
    gigagroup: false,
};

/// The forum's summary, with `kind`.
fn forum_chat(kind: ChatKind) -> ChatSummary {
    ChatSummary {
        kind,
        ..summary(FORUM, "Forum")
    }
}

fn core_sends(harness: &Harness, cx: &mut VisualTestContext, event: Event) {
    harness.events.unbounded_send(event).unwrap();
    cx.run_until_parked();
}

/// Core's copy of the forum's list (generation 1), then its paging.
fn topics_listed(harness: &Harness, cx: &mut VisualTestContext, topics: Vec<Topic>, pages: Pages) {
    let view = TopicsGen(1);
    core_sends(
        harness,
        cx,
        Event::TopicsReset {
            chat: FORUM,
            view,
            order: TopicOrder::LastActivity,
            topics,
            can_post_closed: false,
        },
    );
    core_sends(
        harness,
        cx,
        Event::TopicsPaging {
            chat: FORUM,
            view,
            has_more: pages == Pages::More,
        },
    );
}

fn pane(root: &Entity<Root>, cx: &mut VisualTestContext) -> Pane {
    read(root, cx, |s| s.shell.pane())
}

fn shown_topics(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<Vec<i32>> {
    read(root, cx, |s| {
        s.shell
            .topics()
            .filter(|l| l.is_loaded())
            .map(|l| l.topics().iter().map(|t| t.id.get()).collect())
    })
}

fn select_forum(root: &Entity<Root>, cx: &mut VisualTestContext) {
    act(root, cx, |s, w, cx| s.select_chat(FORUM, w, cx));
}

fn select_topic(root: &Entity<Root>, cx: &mut VisualTestContext, n: i32) {
    act(root, cx, |s, w, cx| s.select_topic(tid(n), w, cx));
}

/// What `f` sent to core.
fn sent_by(harness: &Harness, f: impl FnOnce()) -> Vec<String> {
    let from = sent_count(harness);
    f();
    commands_since(harness, from)
}

fn debug(cmd: &Command) -> String {
    format!("{cmd:?}")
}

const LIST: Pane = Pane::Forum {
    chat: FORUM,
    open: None,
};

#[gpui::test]
fn a_forum_shows_its_topics_and_back_returns_to_them_at_once(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    assert_eq!(
        sent_by(&harness, || select_forum(&root, cx)),
        [
            debug(&Command::OpenForum(FORUM)),
            debug(&Command::CloseThread(ThreadKey::whole(user(1))))
        ]
    );
    assert_eq!(pane(&root, cx), LIST);
    assert_eq!(shown_topics(&root, cx), None, "loading");
    assert_eq!(read(&root, cx, |s| s.shell.send_target()), Ok(None));

    // Core's order stands: pinned topics first.
    let topics = vec![pinned(7), pinned(23), topic(5), topic(3)];
    topics_listed(&harness, cx, topics, Pages::Done);
    assert_eq!(shown_topics(&root, cx), Some(vec![7, 23, 5, 3]));
    assert_eq!(
        sent_by(&harness, || select_topic(&root, cx, 5)),
        [debug(&Command::OpenThread(topic_key(5)))]
    );
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(topic_key(5)));
    cx.simulate_input("draft in 5");

    // Cmd+[ goes back: the list shows at once, nothing is reloaded, and
    // the topic's text waits as its draft.
    assert_eq!(
        sent_by(&harness, || cx.simulate_keystrokes("cmd-[")),
        [debug(&Command::CloseThread(topic_key(5)))]
    );
    assert_eq!(pane(&root, cx), LIST);
    assert_eq!(shown_topics(&root, cx), Some(vec![7, 23, 5, 3]));
    select_topic(&root, cx, 5);
    assert_eq!(composer_text(&root, cx), "draft in 5");

    // The cap evicts a topic with a stashed draft: only its row goes. The
    // topic still exists, so its draft stays.
    select_topic(&root, cx, 3);
    let (chat, topic, reason) = (FORUM, tid(5), TopicGone::Evicted);
    core_sends(
        &harness,
        cx,
        Event::TopicRemoved {
            chat,
            topic,
            reason,
        },
    );
    assert_eq!(shown_topics(&root, cx), Some(vec![7, 23, 3]));
    assert_eq!(discarded(&root, cx), None);
    act(&root, cx, |s, _, _| {
        let draft = s.shell.take_draft(topic_key(5));
        assert_eq!(draft.as_deref(), Some("draft in 5"));
    });
}

/// Topic 5 open with "in 5" typed, while every draft slot is taken
/// (chats 2..=65).
fn topic_open_with_drafts_full(
    cx: &mut TestAppContext,
) -> (Entity<Root>, &mut VisualTestContext, Harness) {
    let (root, cx, harness) = forum_listed(cx);
    let max = i64::try_from(crate::chat::drafts::MAX_DRAFTS).unwrap();
    act(&root, cx, |s, _, _| {
        for id in 2..=max + 1 {
            let draft = s
                .shell
                .stash_draft(ThreadKey::whole(user(id)), format!("t{id}"));
            draft.unwrap();
        }
    });
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5)], Pages::Done);
    select_topic(&root, cx, 5);
    cx.simulate_input("in 5");
    (root, cx, harness)
}

#[gpui::test]
fn the_open_topic_outlives_the_cap_until_it_is_removed(cx: &mut TestAppContext) {
    let (root, cx, harness) = topic_open_with_drafts_full(cx);

    // At the cap, core appends before it removes: the replay holds 502
    // topics in between and ends with core's 501, the open topic kept.
    let at_cap = (1000..1500).rev().map(topic).chain([topic(5)]).collect();
    topics_listed(&harness, cx, at_cap, Pages::Done);
    let (chat, view) = (FORUM, TopicsGen(1));
    let topics = vec![topic(2000)];
    core_sends(&harness, cx, Event::TopicsAppended { chat, view, topics });
    let topic = tid(1000);
    let reason = TopicGone::Evicted;
    core_sends(
        &harness,
        cx,
        Event::TopicRemoved {
            chat,
            topic,
            reason,
        },
    );
    let shown = shown_topics(&root, cx).unwrap();
    assert_eq!((shown.len(), shown[0], shown[500]), (501, 2000, 5));

    // The open topic is removed: back to the list, with a notice. Its
    // text can't be sent any more: it is discarded (taking no draft
    // slot), with a notice that copies it.
    let topic = tid(5);
    assert_eq!(
        sent_by(&harness, || core_sends(
            &harness,
            cx,
            Event::TopicRemoved {
                chat,
                topic,
                reason: TopicGone::Deleted
            }
        )),
        [debug(&Command::CloseThread(topic_key(5)))]
    );
    assert_eq!(pane(&root, cx), LIST);
    assert_eq!(
        read(&root, cx, |s| s.notice.clone()),
        Some(crate::chat::copy::TOPIC_REMOVED.into())
    );
    let reason = "Your draft in a topic of “Forum” was discarded because the topic was deleted.";
    let copied = "a topic of “Forum”\nin 5";
    assert_eq!(discarded(&root, cx), Some((reason.into(), copied.into())));
    act(&root, cx, crate::shell::session::Session::copy_discarded);
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some(copied)
    );
    assert_eq!(discarded(&root, cx), None);
    // Navigation is free: chat 1 opens with an empty box, chat 2 with its
    // own draft.
    select(&root, cx, 1);
    assert_eq!(pane(&root, cx), Pane::Thread(ThreadKey::whole(user(1))));
    assert_eq!(composer_text(&root, cx), "");
    select(&root, cx, 2);
    assert_eq!(composer_text(&root, cx), "t2");
}

/// The discarded-drafts notice, if one is up: its newest line, and what
/// Copy takes.
fn discarded(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<(String, String)> {
    read(root, cx, |s| {
        let line = s.discarded.lines().next()?.to_string();
        Some((line, s.discarded.copy_text()))
    })
}

#[gpui::test]
fn the_forum_row_and_other_chats_leave_a_topic(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5)], Pages::Done);
    select_topic(&root, cx, 5);

    // The forum's row while a topic is open goes back to the list.
    assert_eq!(
        sent_by(&harness, || select_forum(&root, cx)),
        [debug(&Command::CloseThread(topic_key(5)))]
    );
    assert_eq!(pane(&root, cx), LIST);
    assert_eq!(shown_topics(&root, cx), Some(vec![5]));

    // Cmd+[ on the list leaves the forum.
    assert_eq!(
        sent_by(&harness, || cx.simulate_keystrokes("cmd-[")),
        [debug(&Command::CloseForum(FORUM))]
    );
    assert_eq!(pane(&root, cx), Pane::Empty);

    // Another chat from a topic: core closes the topic and the forum view
    // itself when it opens the chat.
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5)], Pages::Done);
    select_topic(&root, cx, 5);
    assert_eq!(
        sent_by(&harness, || select(&root, cx, 2)),
        [debug(&Command::OpenThread(ThreadKey::whole(user(2))))]
    );
    assert_eq!(pane(&root, cx), Pane::Thread(ThreadKey::whole(user(2))));
    assert!(read(&root, cx, |s| s.shell.topics().is_none()));
}

#[gpui::test]
fn a_changed_forum_flag_moves_the_pane(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    select_forum(&root, cx);
    select_topic(&root, cx, 5);
    cx.simulate_input("in 5");
    let whole = ThreadKey::whole(FORUM);
    let lost = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        let (line, copied) = discarded(root, cx).unwrap();
        (line.ends_with("forum."), copied)
    };
    let opens = |sent: &[String]| {
        let open = debug(&Command::OpenThread(whole));
        sent.iter().filter(|c| **c == open).count()
    };

    // The forum stops being one while a topic is open (core invalidates
    // the list, then sends the summary), with core's queue full: nothing
    // spins, and the whole chat opens once the backoff ends.
    harness.sent.borrow_mut().full = true;
    let refused = sent_by(&harness, || {
        core_sends(&harness, cx, Event::ForumInvalidated(FORUM));
        core_sends(&harness, cx, Event::ChatBumped(forum_chat(NOT_FORUM)));
    });
    assert!(refused.is_empty());
    assert_eq!(
        pane(&root, cx),
        Pane::Forum {
            chat: FORUM,
            open: Some(tid(5))
        }
    );
    harness.sent.borrow_mut().full = false;
    let retried = sent_by(&harness, || wait(cx, 1));
    assert_eq!(opens(&retried), 1, "{retried:?}");
    assert_eq!(pane(&root, cx), Pane::Thread(whole));
    let topic_text = "a topic of “Forum”\nin 5";
    assert_eq!(lost(&root, cx), (true, topic_text.into()));
    assert_eq!(composer_text(&root, cx), "");
    cx.simulate_input("in whole");

    // And becomes one again: its topic list opens, and the whole chat's
    // text, now unsendable, joins the notice, newest first.
    assert_eq!(
        sent_by(&harness, || {
            core_sends(&harness, cx, Event::ChatBumped(forum_chat(IS_FORUM)));
        }),
        [
            debug(&Command::OpenForum(FORUM)),
            debug(&Command::CloseThread(whole))
        ]
    );
    assert_eq!(pane(&root, cx), LIST);
    let both = format!("“Forum”\nin whole\n\n{topic_text}");
    assert_eq!(lost(&root, cx), (true, both));
}

#[gpui::test]
fn a_forum_flag_change_off_screen_drops_its_drafts(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5), topic(6)], Pages::Done);
    select_topic(&root, cx, 5);
    cx.simulate_input("in 5");
    select_topic(&root, cx, 6);
    cx.simulate_input("in 6");
    select(&root, cx, 2);

    // The forum stops being one while chat 2 is shown: both topic drafts
    // go at once, into one notice that copies both, and free their slots.
    let forum = abyss_core::store::ForumStatus::NotForum;
    let sent = sent_by(&harness, || {
        core_sends(&harness, cx, Event::KindChanged { chat: FORUM, forum });
        core_sends(&harness, cx, Event::ForumInvalidated(FORUM));
    });
    assert!(sent.is_empty(), "the pane stays: {sent:?}");
    let line = "2 drafts were discarded because the chat is no longer a forum.";
    let copied = "a topic of “Forum”\nin 6\n\na topic of “Forum”\nin 5";
    assert_eq!(discarded(&root, cx), Some((line.into(), copied.into())));
    act(&root, cx, |s, _, _| {
        assert_eq!(s.shell.take_draft(topic_key(5)), None);
        assert_eq!(s.shell.take_draft(topic_key(6)), None);
    });

    // Shown but no longer listed, the forum changes again: core's
    // announcement alone moves the pane.
    select_forum(&root, cx);
    let whole = ThreadKey::whole(FORUM);
    core_sends(
        &harness,
        cx,
        Event::DialogsReset(FakeScript::demo(2, 0).dialogs),
    );
    let changed = sent_by(&harness, || {
        let forum = abyss_core::store::ForumStatus::NotForum;
        core_sends(&harness, cx, Event::KindChanged { chat: FORUM, forum });
        core_sends(&harness, cx, Event::ForumInvalidated(FORUM));
    });
    assert_eq!(changed, [debug(&Command::OpenThread(whole))]);
    assert_eq!(pane(&root, cx), Pane::Thread(whole));
}

fn flood(seconds: u32) -> Event {
    Event::LoadEnded {
        target: LoadTarget::Topics(FORUM),
        error: Some(UserError::FloodWait { seconds }),
    }
}

fn countdown(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<u64> {
    let now = cx.executor().now();
    read(root, cx, |s| {
        s.shell.topics().and_then(|l| l.countdown(now))
    })
}

fn wait(cx: &mut VisualTestContext, seconds: u64) {
    cx.executor().advance_clock(Duration::from_secs(seconds));
    cx.run_until_parked();
}

#[gpui::test]
fn a_topics_flood_wait_counts_down_and_then_loads_once(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    select_forum(&root, cx);

    // A cold open inside a flood wait: the countdown replaces "Loading",
    // and the open is sent again once, when the wait ends. Repeated waits
    // are not failures: retries never give up on them.
    for _ in 0..6 {
        core_sends(&harness, cx, flood(30));
        assert_eq!(countdown(&root, cx), Some(30));
        assert!(sent_by(&harness, || wait(cx, 29)).is_empty(), "waiting");
        assert_eq!(countdown(&root, cx), Some(1));
        assert_eq!(
            sent_by(&harness, || wait(cx, 5)),
            [debug(&Command::OpenForum(FORUM))],
            "sent once"
        );
        assert!(!read(&root, cx, |s| s.shell.topics_gave_up()));
    }

    // A next page inside a flood wait counts down under the kept list.
    let topics = (2..=60).map(topic).collect();
    topics_listed(&harness, cx, topics, Pages::More);
    assert_eq!(
        sent_by(&harness, || act(&root, cx, |s, w, cx| s
            .topics_seen(55, w, cx))),
        [debug(&Command::LoadTopics(FORUM))]
    );
    core_sends(&harness, cx, flood(10));
    assert_eq!(countdown(&root, cx), Some(10));
    assert_eq!(shown_topics(&root, cx).map(|t| t.len()), Some(59));
    assert_eq!(
        sent_by(&harness, || wait(cx, 11)),
        [debug(&Command::LoadTopics(FORUM))]
    );

    // Leaving drops the kept load; coming back shows core's remaining wait.
    core_sends(&harness, cx, flood(10));
    select(&root, cx, 2);
    assert!(sent_by(&harness, || wait(cx, 20)).is_empty(), "dropped");
    select_forum(&root, cx);
    core_sends(&harness, cx, flood(4));
    assert_eq!(countdown(&root, cx), Some(4));
}

/// The real core over the `cargo dev` demo account: a forum lists General
/// and 60 topics with the pinned ones first, over two pages; the
/// creation-ordered forum (chat 12) keeps its order while messages arrive,
/// where an activity-ordered one moves the busy topic up; Back needs
/// nothing from core.
#[test]
fn the_demo_forums_list_page_and_keep_their_order() {
    use abyss_core::api::AuthState;
    use abyss_core::backend::fake::{FAKE_CODE, FakeBackend};
    use abyss_core::{BackendChoice, CoreConfig};

    let script = crate::chat::demo::script(crate::chat::format::now());
    let (backend, fake) = FakeBackend::new(script);
    let core = abyss_core::start(CoreConfig::default(), BackendChoice::Fake(backend)).unwrap();
    let (commands, events) = core.split();
    let mut live = Live {
        shell: crate::chat::ChatShell::default(),
        events,
        auth: None,
    };
    live.until(|_, auth| matches!(auth, Some(AuthState::Phone { .. })));
    commands
        .send(Command::SubmitPhone("+15550100".into()))
        .unwrap();
    live.until(|_, auth| matches!(auth, Some(AuthState::Code { .. })));
    commands
        .send(Command::SubmitCode(FAKE_CODE.into()))
        .unwrap();
    live.until(|shell, auth| {
        matches!(auth, Some(AuthState::SignedIn { .. })) && shell.list.len() > 12
    });
    let mut admit = |cmd| {
        commands.send(cmd).unwrap();
        Ok(())
    };

    let orders = [(2, TopicOrder::LastActivity), (12, TopicOrder::Created)]
        .map(|(id, order)| live.forum(&fake, &mut admit, id, order));
    assert_ne!(orders[0], orders[1], "the two orders differ");
    commands.cancel();
}

impl Live {
    /// Opens demo forum `id`, served in `order`, pages it to the end, sends
    /// a message into its last topic, and goes into a topic and back.
    /// Returns the order after the message.
    fn forum(
        &mut self,
        fake: &abyss_core::backend::fake::FakeHandle,
        admit: crate::chat::Admit<'_>,
        id: i64,
        order: TopicOrder,
    ) -> Vec<i32> {
        let chat = ChatId {
            kind: PeerKind::Channel,
            id,
        };
        self.shell.select(chat, admit).unwrap();
        self.until(|shell, _| !ids(shell).is_empty());
        assert_eq!(ids(&self.shell).len(), 50, "one page");
        assert!(self.shell.topics_seen(49, Instant::now()));
        self.shell.request_topics(Instant::now(), admit).unwrap();
        self.until(|shell, _| ids(shell).len() == 61);
        let pinned = |shell: &crate::chat::ChatShell| -> Vec<bool> {
            shell
                .topics()
                .unwrap()
                .topics()
                .iter()
                .map(|t| t.pinned)
                .collect()
        };
        assert_eq!(pinned(&self.shell)[..2], [true, true]);
        assert!(pinned(&self.shell)[2..].iter().all(|p| !p), "pinned first");

        // A message into the last topic.
        let before = ids(&self.shell);
        let bottom = *before.last().unwrap();
        let thread = ThreadKey {
            chat,
            topic: Some(tid(bottom)),
        };
        let m = futures::executor::block_on(fake.incoming(thread, "new")).unwrap();
        self.until(|shell, _| {
            let topic = shell.topics().and_then(|l| l.get(tid(bottom)));
            topic.is_some_and(|t| t.last_id == Some(m.id))
        });
        let after = ids(&self.shell);
        match order {
            TopicOrder::Created => assert_eq!(after, before, "creation order holds"),
            TopicOrder::LastActivity => assert_eq!(after[2], bottom, "first after the pinned"),
        }

        // Into a topic and back: the list shows at once.
        self.shell.open_topic(tid(bottom), admit).unwrap();
        self.shell.back(admit).unwrap();
        assert_eq!(ids(&self.shell), after);
        after
    }
}

fn ids(shell: &crate::chat::ChatShell) -> Vec<i32> {
    shell
        .topics()
        .filter(|l| l.is_loaded())
        .map_or_else(Vec::new, |l| {
            l.topics().iter().map(|t| t.id.get()).collect()
        })
}

/// The UI state fed by a real core's events.
struct Live {
    shell: crate::chat::ChatShell,
    events: abyss_core::CoreEvents,
    /// Core's last auth state.
    auth: Option<abyss_core::api::AuthState>,
}

impl Live {
    /// Applies core's events until `done` holds, or fails after a generous
    /// deadline.
    fn until(
        &mut self,
        done: impl Fn(&crate::chat::ChatShell, Option<&abyss_core::api::AuthState>) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            while let Some(event) = self.events.try_recv() {
                self.on(event);
            }
            if done(&self.shell, self.auth.as_ref()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("core never got there: {:?}", self.shell);
    }

    fn on(&mut self, event: Event) {
        match event {
            Event::Auth(state) => self.auth = Some(state),
            Event::LoadEnded { target, error } => {
                self.shell.load_ended(target, error, Instant::now());
            }
            other => self.shell.apply(other),
        }
    }
}
