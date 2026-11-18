//! Forums (Stage 2 tasks 10 and 11): the pane moves between the chat list,
//! a forum's topic list and a topic only once core admitted the command,
//! Back (the "‹ Topics" button and Cmd+[) shows the kept list at once, and a
//! flood wait on topics counts down and then sends the load it interrupted,
//! once. A topic's sends, reads and live tail stay in that topic, and the
//! sends of a deleted one end in the discarded-drafts notice. The last two
//! tests drive the real core over the `cargo dev` demo data.

use std::time::{Duration, Instant};

use abyss_core::api::{LoadTarget, UserError};
use abyss_core::model::{
    ChatKind, ChatSummary, Freshness, MsgId, Topic, TopicGone, TopicId, TopicOrder,
};
use abyss_core::store::TopicsGen;

use super::*;
use crate::chat::Pane;
use crate::chat::list::{Pages, SidebarRow};
use crate::shell::session::Session;

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

    // The forum stops being one while a topic is open (core announces the
    // kind and invalidates the list, and only then sends the summary), with
    // core's queue full: nothing spins. Text sent into the topic before the
    // summary arrives is not taken: the announced kind already governs.
    harness.sent.borrow_mut().full = true;
    let refused = sent_by(&harness, || {
        let forum = abyss_core::store::ForumStatus::NotForum;
        core_sends(&harness, cx, Event::KindChanged { chat: FORUM, forum });
        core_sends(&harness, cx, Event::ForumInvalidated(FORUM));
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
    let late = sent_by(&harness, || {
        cx.simulate_input("late");
        cx.simulate_keystrokes("enter");
    });
    assert!(late.is_empty(), "{late:?}");
    // The whole chat opens once the backoff ends; the late text joins the
    // discarded notice, and no echo is left behind.
    core_sends(&harness, cx, Event::ChatBumped(forum_chat(NOT_FORUM)));
    let retried = sent_by(&harness, || wait(cx, 1));
    assert_eq!(opens(&retried), 1, "{retried:?}");
    assert_eq!(pane(&root, cx), Pane::Thread(whole));
    assert_eq!(read(&root, cx, |s| s.shell.row_count()), 0);
    let topic_text = "a topic of “Forum”\nlate\n\na topic of “Forum”\nin 5";
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

/// Topic `n`'s tail, messages `ids`.
fn topic_tail(n: i32, ids: std::ops::RangeInclusive<i32>) -> Event {
    let messages = ids
        .map(|id| abyss_core::model::Message {
            thread: topic_key(n),
            ..message(FORUM, id)
        })
        .collect();
    Event::History {
        thread: topic_key(n),
        view: abyss_core::store::ViewGen(1),
        dir: abyss_core::api::HistoryDir::Tail,
        messages,
        removed: Vec::new(),
        has_more: false,
    }
}

/// A topic with news since its count (its dot) opens at its read
/// position: core's page lands at the divider, and the first paint reads
/// only what is above it.
#[gpui::test]
fn a_topic_with_news_opens_at_its_read_position(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(300.)));
    select_forum(&root, cx);
    let news = Topic {
        freshness: Freshness::NewSince,
        ..topic(5)
    };
    topics_listed(&harness, cx, vec![news, topic(6)], Pages::Done);
    let (thread, after) = (topic_key(5), MsgId(5));
    assert_eq!(
        sent_by(&harness, || select_topic(&root, cx, 5)),
        [debug(&Command::OpenThreadAt { thread, after })]
    );

    let messages = (1..=40)
        .map(|id| abyss_core::model::Message {
            thread,
            ..message(FORUM, id)
        })
        .collect();
    let page = Event::History {
        thread,
        view: abyss_core::store::ViewGen(1),
        dir: abyss_core::api::HistoryDir::Around,
        messages,
        removed: Vec::new(),
        has_more: true,
    };
    let sent = sent_by(&harness, || core_sends(&harness, cx, page));
    let (divider, top) = read(&root, cx, |s| {
        let rows = (0..s.shell.row_count()).filter(|ix| s.shell.divider_before(*ix));
        (
            rows.collect::<Vec<_>>(),
            s.shell.rows().logical_scroll_top().item_ix,
        )
    });
    assert_eq!((divider, top), (vec![5], 5), "at the divider, before 6");
    let read_up_to = |id| {
        debug(&Command::MarkRead {
            thread,
            max_id: MsgId(id),
        })
    };
    assert!(sent.iter().any(|c| c.starts_with("MarkRead")), "{sent:?}");
    assert!(
        (1..=5).all(|id| !sent.contains(&read_up_to(id))),
        "{sent:?}"
    );
    let tail = Command::ViewingTail(Some(thread));
    assert!(!sent.contains(&debug(&tail)), "detached: {sent:?}");
}

#[gpui::test]
fn sends_reads_and_the_tail_stay_in_their_topic(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(300.)));
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5), topic(6)], Pages::Done);
    select_topic(&root, cx, 5);
    let echoes =
        |root: &Entity<Root>, cx: &mut VisualTestContext| read(root, cx, |s| s.shell.row_count());

    // Two sends into topic 5: core fails one and refuses the other. Both
    // show there, and nowhere else.
    let failed = submit_text(cx, &harness, "failed in 5");
    core_sends(&harness, cx, Event::SendAdmitted { local_id: failed });
    let state = abyss_core::api::SendState::Failed;
    let local_id = failed;
    core_sends(&harness, cx, Event::SendState { local_id, state });
    let refused = submit_text(cx, &harness, "refused in 5");
    core_sends(&harness, cx, Event::SendRefused { local_id: refused });
    assert_eq!(echoes(&root, cx), 2);
    select_topic(&root, cx, 6);
    assert_eq!(echoes(&root, cx), 0, "not in another topic");
    act(&root, cx, crate::shell::session::Session::back_from_forum);
    select_topic(&root, cx, 1);
    assert_eq!(echoes(&root, cx), 0, "nor in General");

    // Reads and the live tail name the topic shown.
    select_topic(&root, cx, 6);
    let sent = sent_by(&harness, || {
        core_sends(&harness, cx, topic_tail(6, 10..=12));
    });
    let mark = Command::MarkRead {
        thread: topic_key(6),
        max_id: MsgId(12),
    };
    assert!(sent.contains(&debug(&mark)), "{sent:?}");
    let tail = Command::ViewingTail(Some(topic_key(6)));
    assert!(sent.contains(&debug(&tail)), "{sent:?}");

    // Topic 5 is deleted while topic 6 shows: core keeps the failed send,
    // which nobody can see any more, so the UI discards it. The refused
    // text and, once core confirms the discard, the failed one join the
    // notice.
    let (chat, topic) = (FORUM, tid(5));
    let removed = Event::TopicRemoved {
        chat,
        topic,
        reason: TopicGone::Deleted,
    };
    let sent = sent_by(&harness, || core_sends(&harness, cx, removed));
    assert_eq!(sent, [debug(&Command::DiscardSend(failed))]);
    let line =
        "Your pending message in a topic of “Forum” was discarded because the topic was deleted.";
    let (shown, _) = discarded(&root, cx).unwrap();
    assert_eq!(shown, line);
    core_sends(&harness, cx, Event::SendDiscarded { local_id: failed });
    let copied = "a topic of “Forum”\nfailed in 5\n\na topic of “Forum”\nrefused in 5";
    assert_eq!(discarded(&root, cx), Some((line.into(), copied.into())));
    assert!(read(&root, cx, |s| s.shell.echoes().get(failed).is_none()));
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

/// The rows right under `forum`'s sidebar row: its inline topics and their
/// More or pending row.
fn beneath(shell: &crate::chat::ChatShell, forum: ChatId) -> Vec<SidebarRow> {
    let rows = shell.list.rows();
    let Some(at) = rows.iter().position(|r| *r == SidebarRow::Chat(forum)) else {
        return Vec::new();
    };
    rows.iter()
        .skip(at + 1)
        .take_while(|r| !matches!(r, SidebarRow::Chat(_) | SidebarRow::Archive(_)))
        .copied()
        .collect()
}

fn inline(root: &Entity<Root>, cx: &mut VisualTestContext) -> Vec<SidebarRow> {
    read(root, cx, |s| beneath(&s.shell, FORUM))
}

fn topic_rows(ids: impl IntoIterator<Item = i32>) -> Vec<SidebarRow> {
    ids.into_iter()
        .map(|n| SidebarRow::Topic {
            forum: FORUM,
            topic: tid(n),
        })
        .collect()
}

const MORE: SidebarRow = SidebarRow::MoreTopics { forum: FORUM };
const PENDING: SidebarRow = SidebarRow::TopicsPending { forum: FORUM };

/// Plan section 17.3: the shown forum lists its first 8 topics under its
/// row, plus the open topic wherever it stands, then More; a topic row and
/// Option+Cmd+Down/Up open topics as the topic list does, and the inline
/// rows follow the list's events without asking core for anything.
#[gpui::test]
fn the_shown_forum_lists_its_topics_under_its_row(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    assert!(inline(&root, cx).is_empty(), "no forum shown");
    select_forum(&root, cx);
    assert_eq!(inline(&root, cx), [PENDING], "loading");

    let topics = [pinned(30), pinned(31)]
        .into_iter()
        .chain((1..=18).map(topic))
        .collect();
    topics_listed(&harness, cx, topics, Pages::More);
    let first = [30, 31, 1, 2, 3, 4, 5, 6];
    assert_eq!(inline(&root, cx), [topic_rows(first), vec![MORE]].concat());

    // Down from the forum's row opens its first topic, which takes the
    // selection mark.
    assert_eq!(
        sent_by(&harness, || cx.simulate_keystrokes("alt-cmd-down")),
        [debug(&Command::OpenThread(topic_key(30)))]
    );
    let marked = read(&root, cx, |s| s.shell.marked());
    assert_eq!(marked, topic_rows([30]).pop());

    // An open topic beyond the first 8 shows in its place: at most 8, the
    // open one and More.
    select_topic(&root, cx, 15);
    let shown = [30, 31, 1, 2, 3, 4, 5, 6, 15];
    assert_eq!(inline(&root, cx), [topic_rows(shown), vec![MORE]].concat());
    assert_eq!(
        sent_by(&harness, || cx.simulate_keystrokes("alt-cmd-up")),
        [debug(&Command::OpenThread(topic_key(6)))],
        "the previous row: the 8th topic"
    );
    select_topic(&root, cx, 15);

    // Core's events reach the inline rows through the same list, and
    // drawing them asks core for nothing, though more pages exist.
    let removed = Event::TopicRemoved {
        chat: FORUM,
        topic: tid(30),
        reason: TopicGone::Deleted,
    };
    let sent = sent_by(&harness, || {
        core_sends(&harness, cx, removed);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    });
    assert!(sent.is_empty(), "{sent:?}");
    let shown = [31, 1, 2, 3, 4, 5, 6, 7, 15];
    assert_eq!(inline(&root, cx), [topic_rows(shown), vec![MORE]].concat());

    // More goes back to the full list in the pane.
    assert_eq!(
        sent_by(&harness, || act(
            &root,
            cx,
            crate::shell::session::Session::more_topics
        )),
        [debug(&Command::CloseThread(topic_key(15)))]
    );
    assert_eq!(pane(&root, cx), LIST);

    core_sends(&harness, cx, Event::ForumInvalidated(FORUM));
    assert_eq!(inline(&root, cx), [PENDING], "until core's new copy");
}

/// Collapsing and expanding the shown forum's row leave the pane alone;
/// selecting the forum expands it again. The inline rows follow the forum's
/// row into every view that shows it (a built-in tab, a folder, the
/// archive), and never add it to one that doesn't.
#[gpui::test]
fn inline_topics_collapse_and_follow_the_forum_into_every_view(cx: &mut TestAppContext) {
    use crate::chat::inline::Inline;
    use crate::chat::list::{Shown, Tab};

    let (root, cx, harness) = forum_listed(cx);
    let groups = abyss_core::model::ChatFolder {
        id: 2,
        title: "Groups".into(),
        rules: abyss_core::model::FolderRules::GROUPS,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    core_sends(&harness, cx, Event::ChatFolders(vec![groups]));
    select_forum(&root, cx);
    topics_listed(&harness, cx, vec![topic(5), topic(6)], Pages::Done);
    let both = topic_rows([5, 6]);
    let expansion = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        read(root, cx, |s| s.shell.inline_of(FORUM))
    };

    let sent = sent_by(&harness, || cx.simulate_keystrokes("alt-cmd-left"));
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(pane(&root, cx), LIST);
    assert_eq!(expansion(&root, cx), Some(Inline::Collapsed));
    assert!(inline(&root, cx).is_empty());
    let sent = sent_by(&harness, || cx.simulate_keystrokes("alt-cmd-right"));
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(inline(&root, cx), both);

    // Collapsed, then selected again from its row: expanded.
    act(&root, cx, |s, _, cx| s.set_inline(Inline::Collapsed, cx));
    select_forum(&root, cx);
    assert_eq!(inline(&root, cx), both);

    let show = |root: &Entity<Root>, cx: &mut VisualTestContext, shown: Shown| {
        act(root, cx, |s, w, cx| s.show_chats(shown, w, cx));
        read(root, cx, |s| s.shell.list.rows().to_vec())
    };
    for shown in [Shown::Tab(Tab::Forums), Shown::Tab(Tab::Folder(2))] {
        show(&root, cx, shown);
        assert_eq!(inline(&root, cx), both, "{shown:?}");
    }
    let private = show(&root, cx, Shown::Tab(Tab::Private));
    assert!(private.iter().all(|r| matches!(r, SidebarRow::Chat(_))));

    // Archived elsewhere: the archive shows it with its topics.
    let archived = ChatSummary {
        folder: abyss_core::model::PeerFolder::Archive,
        ..forum_chat(IS_FORUM)
    };
    core_sends(&harness, cx, Event::ChatBumped(archived));
    let rows = show(&root, cx, Shown::Archive);
    assert_eq!(rows, [vec![SidebarRow::Chat(FORUM)], both].concat());

    // Leaving the forum takes its rows along.
    select(&root, cx, 1);
    let rows = read(&root, cx, |s| s.shell.list.rows().to_vec());
    assert_eq!(rows, [SidebarRow::Chat(FORUM)]);
    assert_eq!(expansion(&root, cx), None);
}

/// Plan section 17.4 under the inline rows: the unread filter shows the
/// unread topics (a count or a dot). Read topics leave even when open;
/// More shows while
/// the badge counts more unread topics than are shown. The pane's forum
/// with a dot topic counts as unread, so its archive entry shows.
#[gpui::test]
fn the_unread_filter_removes_read_topics_and_their_read_forum(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    act(&root, cx, Session::toggle_unread);
    select_forum(&root, cx);
    let unread = Topic {
        unread: 2,
        ..topic(1)
    };
    let dot = Topic {
        freshness: Freshness::NewSince,
        ..topic(2)
    };
    let topics = [unread.clone(), dot]
        .into_iter()
        .chain((3..=12).map(topic))
        .collect();
    topics_listed(&harness, cx, topics, Pages::Done);
    assert_eq!(inline(&root, cx), topic_rows([1, 2]));
    let badge = |unread| ChatSummary {
        read: abyss_core::model::ReadState {
            unread,
            read_inbox_max: MsgId(0),
        },
        ..forum_chat(IS_FORUM)
    };
    core_sends(&harness, cx, Event::ChatBumped(badge(5)));
    assert_eq!(inline(&root, cx), [topic_rows([1, 2]), vec![MORE]].concat());

    // Opening a read topic does not add it to the unread rows.
    select_topic(&root, cx, 3);
    select_topic(&root, cx, 4);
    let read_here = Topic {
        freshness: Freshness::ReadSince,
        ..unread
    };
    let bumped = Event::TopicBumped {
        chat: FORUM,
        topic: read_here,
    };
    core_sends(&harness, cx, bumped);
    assert_eq!(inline(&root, cx), [topic_rows([2]), vec![MORE]].concat());
    let removed = Event::TopicRemoved {
        chat: FORUM,
        topic: tid(3),
        reason: TopicGone::Deleted,
    };
    core_sends(&harness, cx, removed);
    select_topic(&root, cx, 5);
    assert_eq!(inline(&root, cx), [topic_rows([2]), vec![MORE]].concat());
    act(&root, cx, Session::toggle_unread);
    act(&root, cx, Session::toggle_unread);
    assert_eq!(inline(&root, cx), [topic_rows([2]), vec![MORE]].concat());

    // No badge, a dot topic: the archived forum's entry counts it unread.
    let archived = ChatSummary {
        folder: abyss_core::model::PeerFolder::Archive,
        ..badge(0)
    };
    core_sends(&harness, cx, Event::ChatBumped(archived));
    let first = read(&root, cx, |s| s.shell.list.rows().first().copied());
    assert_eq!(
        first,
        Some(SidebarRow::Archive(crate::chat::list::ArchiveEntry {
            chats: 1,
            unread: 1
        }))
    );
}

#[gpui::test]
fn a_read_forum_leaves_unread_without_closing_its_topic(cx: &mut TestAppContext) {
    let (root, cx, harness) = forum_listed(cx);
    act(&root, cx, Session::toggle_unread);
    select_forum(&root, cx);
    topics_listed(
        &harness,
        cx,
        vec![Topic {
            unread: 1,
            ..topic(5)
        }],
        Pages::Done,
    );
    select_topic(&root, cx, 5);
    let summary = |unread| ChatSummary {
        topic_unread: Some(unread),
        unread_mentions: 3,
        read: abyss_core::model::ReadState {
            unread: 37,
            ..Default::default()
        },
        ..forum_chat(IS_FORUM)
    };
    core_sends(&harness, cx, Event::ChatBumped(summary(1)));
    core_sends(
        &harness,
        cx,
        Event::TopicBumped {
            chat: FORUM,
            topic: topic(5),
        },
    );
    core_sends(&harness, cx, Event::ChatBumped(summary(0)));
    let visible = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        read(root, cx, |s| {
            s.shell.list.rows().contains(&SidebarRow::Chat(FORUM))
        })
    };
    assert!(!visible(&root, cx));
    assert_eq!(read(&root, cx, |s| s.shell.selected()), Some(topic_key(5)));
    assert!(inline(&root, cx).is_empty());
    // A new unread message brings the forum back, before its count refresh.
    core_sends(
        &harness,
        cx,
        Event::TopicBumped {
            chat: FORUM,
            topic: Topic {
                freshness: Freshness::NewSince,
                ..topic(5)
            },
        },
    );
    assert!(visible(&root, cx));
    assert_eq!(inline(&root, cx), topic_rows([5]));
}

/// The real core over the `cargo dev` demo account: a forum lists General
/// and 60 topics with the pinned ones first, over two pages; the
/// creation-ordered forum (chat 12) keeps its order while messages arrive,
/// where an activity-ordered one moves the busy topic up; Back needs
/// nothing from core.
#[test]
fn the_demo_forums_list_page_and_keep_their_order() {
    let script = crate::chat::demo::script(crate::chat::format::now());
    let (mut live, commands, fake) = Live::signed_in(script);
    let mut admit = |cmd| {
        commands.send(cmd).unwrap();
        Ok(())
    };

    let orders = [(2, TopicOrder::LastActivity), (12, TopicOrder::Created)]
        .map(|(id, order)| live.forum(&fake, &mut admit, id, order));
    assert_ne!(orders[0], orders[1], "the two orders differ");
    commands.cancel();
}

/// The demo forum that lists topics by activity.
const DEMO_FORUM: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 2,
};

fn demo_topic(n: i32) -> ThreadKey {
    ThreadKey {
        chat: DEMO_FORUM,
        topic: Some(tid(n)),
    }
}

/// The real core over the demo account, whose forum (chat 2) has one
/// unread topic left: a closed topic blocks the composer until it reopens,
/// a send into a topic shows only there, reading the last unread topic
/// clears the forum's badge, and a new message into a read topic shows the
/// dot, then the server's count.
#[test]
fn a_demo_topic_thread_keeps_to_itself() {
    let mut script = crate::chat::demo::script(crate::chat::format::now());
    // Every topic read to the newest message but 4.
    let last = script.history[&DEMO_FORUM]
        .iter()
        .map(|m| m.id)
        .max()
        .unwrap();
    for t in script.topics.get_mut(&DEMO_FORUM).unwrap() {
        if t.topic.id != tid(4) {
            t.topic.read_inbox_max = last;
        }
    }
    let (mut live, commands, fake) = Live::signed_in(script);
    let mut admit = |cmd| {
        commands.send(cmd).unwrap();
        Ok(())
    };
    assert_eq!(badge(&live.shell), Some(1));
    live.shell.select(DEMO_FORUM, &mut admit).unwrap();
    live.until(|shell, _| !ids(shell).is_empty());
    live.shell.topics_seen(49, Instant::now());
    live.shell
        .request_topics(Instant::now(), &mut admit)
        .unwrap();
    live.until(|shell, _| ids(shell).len() == 61);
    live.closed_then_sent(&fake, &mut admit);
    live.read_then_new(&fake, &mut admit);
    commands.cancel();
}

/// The real core over the demo account (`cargo dev`): the showcase, with
/// 120 unread messages among the user's own, opens around its read
/// position at the divider; reading part of them brings the server's
/// remaining count; Jump to latest reaches the tail. The forum's General
/// topic, with a page of unread posts, opens the same way.
#[test]
fn demo_unread_threads_open_at_their_first_unread_message() {
    use crate::chat::viewport::Seen;

    let showcase = ChatId {
        kind: PeerKind::User,
        id: 1,
    };
    let (mut live, commands, fake) =
        Live::signed_in(crate::chat::demo::script(crate::chat::format::now()));
    let mut admit = |cmd| {
        commands.send(cmd).unwrap();
        Ok(())
    };
    let after = live.shell.list.get(showcase).unwrap().read.read_inbox_max;
    live.shell.select(showcase, &mut admit).unwrap();
    live.until(|shell, _| loaded(shell));
    let divider = live.at_divider(after);

    // Read through the window's newest: a partial read.
    let window = live.shell.window().unwrap();
    let (newest, read_to) = (window.len() - 1, window.newest().unwrap());
    let seen = Seen {
        first_row: divider,
        last_message: Some(newest),
    };
    live.shell
        .frame_seen(Some(seen), Instant::now(), &mut admit)
        .unwrap();
    let left = fake
        .server_history(showcase)
        .iter()
        .filter(|m| !m.outgoing && m.id > read_to)
        .count();
    assert!(left > 0 && left < 120, "{left}");
    live.until(|shell, _| {
        let chat = shell.list.get(showcase).unwrap();
        chat.reading.is_none() && chat.displayed_unread() == u32::try_from(left).unwrap()
    });

    live.shell.jump_to_latest(&mut admit).unwrap();
    let last = fake.server_history(showcase).last().map(|m| m.id);
    live.until(|shell, _| {
        let w = shell.window().unwrap();
        !w.has_newer() && w.newest() == last
    });

    // General, read only up to its first message.
    live.shell.select(DEMO_FORUM, &mut admit).unwrap();
    live.until(|shell, _| !ids(shell).is_empty());
    let general = live.shell.topics().unwrap().get(TopicId::GENERAL).unwrap();
    let after = general.read_inbox_max;
    live.shell.open_topic(TopicId::GENERAL, &mut admit).unwrap();
    live.until(|shell, _| loaded(shell));
    live.at_divider(after);
    commands.cancel();
}

impl Live {
    /// The open thread shows a detached window around read position
    /// `after`, scrolled to the divider. Returns the divider's row.
    #[track_caller]
    fn at_divider(&self, after: MsgId) -> usize {
        let window = self.shell.window().unwrap();
        assert!(window.has_newer(), "around the read position, not the tail");
        let divider = window.divider_row(after).unwrap();
        assert!(self.shell.divider_before(divider));
        assert_eq!(self.shell.rows().logical_scroll_top().item_ix, divider);
        divider
    }
}

/// The demo forum's badge.
fn badge(shell: &crate::chat::ChatShell) -> Option<u32> {
    shell.list.get(DEMO_FORUM).map(|c| c.read.unread)
}

/// Whether the open thread's window loaded some messages.
fn loaded(shell: &crate::chat::ChatShell) -> bool {
    shell.window().is_some_and(|w| w.is_loaded() && w.len() > 0)
}

/// Echo rows the open thread shows.
fn echo_rows(shell: &crate::chat::ChatShell) -> usize {
    let messages = shell
        .window()
        .map_or(0, crate::chat::messages::MessageWindow::len);
    shell.row_count() - messages
}

impl Live {
    /// From the demo forum's list (or a topic), opens topic `n` and waits
    /// for its messages.
    fn reopen(&mut self, admit: crate::chat::Admit<'_>, n: i32) {
        self.shell.back(admit).unwrap();
        self.shell.open_topic(tid(n), admit).unwrap();
        self.until(|shell, _| loaded(shell));
    }

    /// Topic 14 is closed: no box and nothing to send, until it reopens.
    /// Then a send into it shows there once the server has it, and in no
    /// other thread.
    fn closed_then_sent(
        &mut self,
        fake: &abyss_core::backend::fake::FakeHandle,
        admit: crate::chat::Admit<'_>,
    ) {
        use crate::chat::Composer;

        self.shell.open_topic(tid(14), admit).unwrap();
        assert!(self.shell.shown_topic().is_some_and(|t| t.closed));
        assert_eq!(self.shell.composer(), Composer::TopicClosed);
        assert_eq!(self.shell.send_target(), Ok(None));
        futures::executor::block_on(fake.reopen_topic(DEMO_FORUM, tid(14))).unwrap();
        self.until(|shell, _| shell.composer() == Composer::Open);
        assert_eq!(self.shell.send_target(), Ok(Some(demo_topic(14))));

        self.until(|shell, _| loaded(shell));
        let now = crate::chat::format::now();
        self.shell
            .submit(demo_topic(14), "only here", now, admit)
            .unwrap();
        self.until(|_, _| !fake.sent().is_empty());
        let (id, thread) = fake
            .server_history(DEMO_FORUM)
            .last()
            .map(|m| (m.id, m.thread))
            .unwrap();
        assert_eq!(thread, demo_topic(14));
        let shown = |shell: &crate::chat::ChatShell| shell.window().is_some_and(|w| w.contains(id));
        for other in [2, 1] {
            self.reopen(admit, other);
            let (here, echoes) = (shown(&self.shell), echo_rows(&self.shell));
            assert_eq!((here, echoes), (false, 0), "topic {other}");
        }
        self.reopen(admit, 14);
        self.until(|shell, _| shown(shell) && echo_rows(shell) == 0);
    }

    /// Reading the last unread topic (4) marks that topic, and the forum's
    /// badge follows the server's count. A message into read topic 2 then
    /// shows the dot, then the server's count, and the badge again.
    fn read_then_new(
        &mut self,
        fake: &abyss_core::backend::fake::FakeHandle,
        admit: crate::chat::Admit<'_>,
    ) {
        use crate::chat::topics::UnreadMark;
        use crate::chat::viewport::Seen;

        self.reopen(admit, 4);
        let newest = self.shell.window().unwrap().len() - 1;
        let seen = Seen {
            first_row: newest,
            last_message: Some(newest),
        };
        self.shell
            .frame_seen(Some(seen), Instant::now(), admit)
            .unwrap();
        self.until(|shell, _| badge(shell) == Some(0));
        let reads = fake.read_calls();
        assert!(reads.iter().any(|(t, _)| *t == demo_topic(4)), "{reads:?}");
        assert!(reads.iter().all(|(t, _)| t.topic.is_some()), "{reads:?}");

        self.shell.back(admit).unwrap();
        let mark = |shell: &crate::chat::ChatShell| {
            let topic = shell.topics().and_then(|l| l.get(tid(2)));
            topic.map(UnreadMark::of)
        };
        assert_eq!(mark(&self.shell), Some(UnreadMark::None));
        futures::executor::block_on(fake.incoming(demo_topic(2), "new")).unwrap();
        self.until_each(|shell| mark(shell) == Some(UnreadMark::Dot));
        let count = UnreadMark::Count {
            unread: 1,
            mentions: 0,
        };
        self.until(|shell, _| mark(shell) == Some(count));
        self.until(|shell, _| badge(shell) == Some(1));
    }
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
        // Under the forum's sidebar row: its first topics, the pinned ones
        // leading, then More.
        let shown = ids(&self.shell);
        let rows = beneath(&self.shell, chat);
        let first = crate::chat::inline::INLINE_TOPICS;
        let topics = rows.iter().take(first).map(|r| match r {
            SidebarRow::Topic { topic, .. } => topic.get(),
            other => panic!("{other:?}"),
        });
        assert!(topics.eq(shown.iter().take(first).copied()));
        assert_eq!(
            rows.get(first),
            Some(&SidebarRow::MoreTopics { forum: chat })
        );

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

/// When [`Live::wait`] checks its condition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    /// Once the events queued so far are applied.
    Batch,
    /// After every event.
    Each,
}

/// The UI state fed by a real core's events.
struct Live {
    shell: crate::chat::ChatShell,
    events: abyss_core::CoreEvents,
    /// Core's last auth state.
    auth: Option<abyss_core::api::AuthState>,
}

impl Live {
    /// The real core over `script`, signed in, with its chat list shown.
    fn signed_in(
        script: FakeScript,
    ) -> (
        Self,
        abyss_core::CoreSender,
        abyss_core::backend::fake::FakeHandle,
    ) {
        use abyss_core::api::AuthState;
        use abyss_core::backend::fake::{FAKE_CODE, FakeBackend};
        use abyss_core::{BackendChoice, CoreConfig};

        let (backend, fake) = FakeBackend::new(script);
        let core = abyss_core::start(CoreConfig::default(), BackendChoice::Fake(backend)).unwrap();
        let (commands, events) = core.split();
        let mut live = Self {
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
        (live, commands, fake)
    }

    /// Applies core's events until `done` holds, or fails after a generous
    /// deadline.
    #[track_caller]
    fn until(
        &mut self,
        done: impl Fn(&crate::chat::ChatShell, Option<&abyss_core::api::AuthState>) -> bool,
    ) {
        self.wait(Check::Batch, done);
    }

    /// Like [`Self::until`], checked after every event, so a state the next
    /// event replaces is seen too.
    #[track_caller]
    fn until_each(&mut self, done: impl Fn(&crate::chat::ChatShell) -> bool) {
        self.wait(Check::Each, |shell, _| done(shell));
    }

    #[track_caller]
    fn wait(
        &mut self,
        check: Check,
        done: impl Fn(&crate::chat::ChatShell, Option<&abyss_core::api::AuthState>) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            while let Some(event) = self.events.try_recv() {
                self.on(event);
                if check == Check::Each && done(&self.shell, self.auth.as_ref()) {
                    return;
                }
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
