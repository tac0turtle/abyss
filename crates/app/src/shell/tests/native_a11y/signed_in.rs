//! The signed-in shell through `AppKit`: the tab strip, the chat and topic
//! lists, the open chat and its composer, and the live region. The same rules
//! as the sign-in scenes: assertions on the tree GPUI sends, focus through
//! real key events, frames from `AppKit` itself.

use std::time::Duration;

use abyss_core::api::{AuthState, Event, HistoryDir};
use abyss_core::model::{
    Body, ChatFolder, ChatId, ChatKind, ChatSummary, FolderRules, Freshness, MediaKind, Message,
    MsgId, PeerFolder, PeerKind, Preview, ReadState, Sender, ThreadKey, Topic, TopicId, TopicOrder,
};
use abyss_core::rich_text::RichText;
use abyss_core::store::{TopicsGen, ViewGen};
use gpui::{App, px, size};

use super::{
    AnyWindowHandle, AppContext, EventSink, NSAccessibility, NSRange, Root, Value, WindowHandle,
    appkit_window, apply, assert_focus, assert_native_role, assert_selection, descendant,
    find_node, key, open_sized, shell, tree, type_text,
};
use crate::chat::{copy as chat_copy, format};

/// An old, fixed time: rows show a date, the same on every run.
const OLD: i64 = 1_700_000_000;
const ALICE: ChatId = chat_id(PeerKind::User, 1);
const BO: ChatId = chat_id(PeerKind::User, 2);
const FAMILY: ChatId = chat_id(PeerKind::Chat, 3);
const FORUM: ChatId = chat_id(PeerKind::Channel, 4);
const NEWS: ChatId = chat_id(PeerKind::Channel, 5);

const fn chat_id(kind: PeerKind, id: i64) -> ChatId {
    ChatId { kind, id }
}

fn summary(id: ChatId, title: &str, kind: ChatKind) -> ChatSummary {
    ChatSummary {
        id,
        title: title.into(),
        kind,
        pinned: false,
        read: ReadState {
            unread: 0,
            read_inbox_max: MsgId(0),
        },
        unread_mentions: 0,
        last: None,
        last_date: OLD,
        last_id: None,
        can_send: true,
        muted_until: None,
        scope_muted_until: 0,
        folder: PeerFolder::Main,
        reading: None,
        topic_unread: None,
    }
}

/// Five chats: a muted, pinned, unread private chat, a media preview, a group
/// with a mention, a forum, and a group only admins post in.
fn dialogs() -> Vec<ChatSummary> {
    let private = ChatKind::Private { contact: true };
    let group = ChatKind::Group;
    let alice = ChatSummary {
        pinned: true,
        muted_until: Some(i64::from(i32::MAX)),
        read: ReadState {
            unread: 3,
            read_inbox_max: MsgId(1),
        },
        last: Some(Preview::Text("see you at five".into())),
        ..summary(ALICE, "Alice", private)
    };
    let bo = ChatSummary {
        last: Some(Preview::Media(MediaKind::Photo)),
        ..summary(BO, "Bo", private)
    };
    let family = ChatSummary {
        read: ReadState {
            unread: 2,
            read_inbox_max: MsgId(0),
        },
        unread_mentions: 1,
        ..summary(FAMILY, "Family", group)
    };
    let forum = summary(
        FORUM,
        "Forum",
        ChatKind::Supergroup {
            forum: true,
            gigagroup: false,
        },
    );
    let news = ChatSummary {
        can_send: false,
        ..summary(
            NEWS,
            "News",
            ChatKind::Supergroup {
                forum: false,
                gigagroup: false,
            },
        )
    };
    vec![alice, bo, family, forum, news]
}

fn when(date: i64) -> String {
    format::list_time(date, format::now(), &format::local_tz()).to_string()
}

fn send(handle: WindowHandle<Root>, cx: &mut App, events: Vec<Event>) {
    handle
        .update(cx, |root, window, cx| {
            root.apply(
                events.into_iter().map(|event| (1, event)).collect(),
                window,
                cx,
            );
        })
        .unwrap();
}

fn nodes_of<'a>(tree: &'a Value, role: &str) -> Vec<&'a Value> {
    let nodes = tree["nodes"].as_object().unwrap();
    nodes
        .values()
        .filter(|node| node["aria"]["role"] == role)
        .collect()
}

fn labels_of(tree: &Value, role: &str) -> Vec<String> {
    nodes_of(tree, role)
        .iter()
        .filter_map(|node| node["aria"]["label"].as_str().map(str::to_owned))
        .collect()
}

fn has_value(tree: &Value, role: &str, value: &str) -> bool {
    nodes_of(tree, role)
        .iter()
        .any(|node| node["aria"]["value"] == value)
}

/// Every node a person or a screen reader can act on has a name.
fn assert_named(tree: &Value) {
    for node in tree["nodes"].as_object().unwrap().values() {
        if node["aria"]["on_action"].is_null() {
            continue;
        }
        let role = node["aria"]["role"].as_str().unwrap();
        let label = node["aria"]["label"].as_str().unwrap_or_default();
        assert!(
            !label.trim().is_empty(),
            "no name for an interactive {role}"
        );
    }
}

/// Where `AppKit` puts the element named `label`, in the window's own
/// coordinates: its left edge, and its top edge counted from the top.
fn place(handle: WindowHandle<Root>, cx: &mut App, label: &str) -> (f64, f64) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            let native = appkit_window(window);
            let frame = native.frame();
            let item = descendant(native.accessibilityChildren(), label)
                .unwrap_or_else(|| panic!("{label} is not in AppKit's tree"))
                .accessibilityFrame();
            let top = frame.origin.y + frame.size.height - (item.origin.y + item.size.height);
            (item.origin.x - frame.origin.x, top)
        })
        .unwrap()
}

/// The column a point sits in: the rail (76 points wide), the sidebar (300
/// wide), the pane. A left edge within a few points of a boundary is past it.
fn column(x: f64) -> u8 {
    if x < 70. {
        0
    } else if x < 370. {
        1
    } else {
        2
    }
}

/// Tab walks `labels` in order, which is the order on screen: left to right
/// by column, top to bottom inside one.
fn assert_tab_order(handle: WindowHandle<Root>, cx: &mut App, labels: &[String]) {
    let mut last = (0, f64::MIN);
    for label in labels {
        key(handle, cx, "tab");
        assert_focus(&tree(handle, cx), label);
        let (x, top) = place(handle, cx, label);
        let here = (column(x), top);
        assert!(
            here.0 > last.0 || (here.0 == last.0 && here.1 >= last.1),
            "{label} comes before the one before it on screen: {here:?} after {last:?}"
        );
        last = here;
    }
}

#[path = "overlays.rs"]
mod overlays;
#[path = "press.rs"]
mod press;
#[path = "shutdown.rs"]
mod shutdown;

/// The scenes that wait on the executor; they end the process.
pub(super) fn run_async(cx: &mut App) {
    press::run(cx);
}

pub(super) fn run(cx: &mut App) {
    let (started, _harness) = shell::tests::started_fixture();
    let bounds = size(px(960.), px(640.));
    let handle = open_sized(cx, bounds, |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    signed_in(handle, cx);
    let chat_labels = tabs_and_rows(handle, cx);
    opening_and_escape(handle, cx, &chat_labels);
    history(handle, cx);
    live_region(handle, cx);
    keyboard_history(handle, cx);
    composer_lines(handle, cx);
    forum(handle, cx);
    composer_blocks(handle, cx);
    jump_button(handle, cx);
    rich_actions(cx);
    overlays::run(cx);
    shutdown::run(cx);
}

fn signed_in(handle: WindowHandle<Root>, cx: &mut App) {
    apply(
        handle,
        cx,
        AuthState::SignedIn {
            display_name: "Demo User".into(),
            new_login: None,
        },
    );
    let work = ChatFolder {
        id: 2,
        title: "Work".into(),
        rules: FolderRules::GROUPS,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    };
    send(
        handle,
        cx,
        vec![
            Event::DialogsReset(dialogs()),
            Event::ChatFolders(vec![work]),
        ],
    );
}

/// The strip, the unread switch, and the chat list with its row names.
/// Returns the row names in order, for the walk that follows.
fn tabs_and_rows(handle: WindowHandle<Root>, cx: &mut App) -> Vec<String> {
    let tree = tree(handle, cx);
    assert_named(&tree);
    assert_eq!(labels_of(&tree, "TabList"), ["Chat tabs"]);
    let tabs = labels_of(&tree, "Tab");
    assert_eq!(
        tabs,
        [
            "All, 2 unread",
            "Private, 1 unread",
            "Groups, 1 unread",
            "Forums",
            "Work, 1 unread"
        ]
    );
    let all = find_node(&tree, "Tab", "All, 2 unread");
    assert_eq!(all["aria"]["selected"], true);
    assert_eq!(
        find_node(&tree, "Tab", "Work, 1 unread")["aria"]["selected"],
        false
    );
    let switch = find_node(&tree, "Switch", "Unread, 2");
    assert_eq!(switch["aria"]["toggled"], "False");
    assert_eq!(labels_of(&tree, "List"), ["Chats, All"]);

    let date = when(OLD);
    let rows = vec![
        format!("Alice, 3 unread, muted, pinned, last: see you at five, {date}"),
        format!("Bo, last: [Photo], {date}"),
        format!("Family, 2 unread, 1 mention, group, {date}"),
        format!("Forum, forum, {date}"),
        format!("News, group, {date}"),
    ];
    // The rows on screen are in the tree, each one element with all of it.
    let found = labels_of(&tree, "ListItem");
    for row in &rows {
        assert!(found.contains(row), "{row:?} is not among {found:?}");
    }
    // Chat rows sit in the list in the order Telegram sorts them; the forum
    // has a disclosure only once it is open.
    assert!(labels_of(&tree, "Button").contains(&"Settings".to_owned()));
    let mut walk = vec![
        "All, 2 unread".to_owned(),
        "Unread, 2".to_owned(),
        "Settings".to_owned(),
    ];
    walk.extend(rows);
    walk
}

/// Tab visits every control in the order on screen; Return opens a chat and
/// moves focus to its composer; Escape returns to the row that was opened.
fn opening_and_escape(handle: WindowHandle<Root>, cx: &mut App, walk: &[String]) {
    assert_tab_order(handle, cx, &walk[..4]);
    let alice = walk[3].clone();
    key(handle, cx, "enter");
    let opened = tree(handle, cx);
    assert_focus(&opened, "Message to Alice");
    assert_eq!(
        find_node(&opened, "MultilineTextInput", "Message to Alice")["aria"]["placeholder"],
        crate::ui::copy::COMPOSER_PLACEHOLDER
    );
    assert_eq!(
        find_node(&opened, "ListItem", &alice)["aria"]["selected"],
        true
    );
    assert!(labels_of(&opened, "Button").contains(&"Send message".to_owned()));
    assert_named(&opened);

    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), &alice);
    // The way back to the composer is one Tab step on from the open row's
    // neighbours, and Escape from there again lands on the same row.
    key(handle, cx, "enter");
    assert_focus(&tree(handle, cx), "Message to Alice");
}

/// A message whose second half is a spoiler, a reply quoting it, and the
/// sidebar's preview of it as the newest message: none may carry the hidden
/// text (review A12).
fn send_spoilered_history(handle: WindowHandle<Root>, cx: &mut App, link: &str, secret: &str) {
    use abyss_core::rich_text::{EntityKind, RawEntity};

    let length = i32::try_from(link.len()).unwrap();
    let entities = [
        RawEntity {
            offset: 0,
            length,
            kind: EntityKind::Url,
        },
        RawEntity {
            offset: length + 1,
            length: i32::try_from(secret.len()).unwrap(),
            kind: EntityKind::Spoiler,
        },
    ];
    let rich = RichText::from_entities(&format!("{link} {secret}"), &entities);
    let spoilered = message(2, Some("Alice"), Body::Text(rich));
    let reply = Message {
        reply_to: Some(MsgId(2)),
        ..message(3, Some("Alice"), Body::Text(RichText::plain("noted")))
    };
    let bumped = ChatSummary {
        last: Some(Preview::of(&spoilered)),
        last_id: Some(MsgId(2)),
        last_date: spoilered.date,
        ..dialogs().swap_remove(0)
    };
    send(
        handle,
        cx,
        vec![
            Event::History {
                thread: ThreadKey::whole(ALICE),
                view: ViewGen(1),
                dir: HistoryDir::Around,
                messages: vec![spoilered, reply],
                removed: Vec::new(),
                has_more: false,
            },
            Event::ChatBumped(bumped),
        ],
    );
}

fn rich_actions(cx: &mut App) {
    let (started, _harness) = shell::tests::started_fixture();
    let handle = open_sized(cx, size(px(960.), px(640.)), |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    signed_in(handle, cx);
    let initial = tree(handle, cx);
    let alice = labels_of(&initial, "ListItem")
        .into_iter()
        .find(|label| label.starts_with("Alice,"))
        .unwrap();
    tab_to(handle, cx, &alice);
    key(handle, cx, "enter");
    let link = "https://example.org";
    let secret = "hidden word";
    send_spoilered_history(handle, cx, link, secret);
    let before = tree(handle, cx);
    assert!(!before.to_string().contains(secret));
    assert!(
        before.to_string().contains("In reply to: ")
            && before.to_string().contains("Hidden spoiler"),
        "the reply quotes the message with its spoiler hidden"
    );
    assert_native_role(handle, cx, &format!("Copy link, {link}"), "AXButton");
    assert_native_role(handle, cx, "Reveal spoiler", "AXButton");
    tab_to(handle, cx, &format!("Copy link, {link}"));
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), "Reveal spoiler");
    key(handle, cx, "space");
    let revealed = tree(handle, cx);
    assert!(
        labels_of(&revealed, "ListItem")
            .iter()
            .any(|label| label.contains(secret))
    );
    assert!(
        !labels_of(&revealed, "Button")
            .iter()
            .any(|label| label == "Reveal spoiler")
    );
    assert_excerpts_stay_hidden(&revealed, secret);
}

/// Revealing a message reveals only its own body: a reply's quote and the
/// sidebar row keep the excerpt.
fn assert_excerpts_stay_hidden(revealed: &Value, secret: &str) {
    let leaks: Vec<_> = nodes_of(revealed, "Label")
        .into_iter()
        .filter_map(|node| node["aria"]["value"].as_str())
        .filter(|value| value.starts_with("In reply to") && value.contains(secret))
        .collect();
    assert!(
        leaks.is_empty(),
        "a revealed message's quote leaks: {leaks:?}"
    );
    let rows: Vec<_> = labels_of(revealed, "ListItem")
        .into_iter()
        .filter(|label| label.starts_with("Alice,") && label.contains("last: "))
        .collect();
    assert_eq!(rows.len(), 1, "one sidebar row for Alice: {rows:?}");
    assert!(
        rows[0].contains("Hidden spoiler") && !rows[0].contains(secret),
        "the sidebar row leaks: {}",
        rows[0]
    );
}

fn message(id: i32, from: Option<&str>, body: Body) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey::whole(ALICE),
        sender: from.map(|name| Sender {
            id: ALICE,
            name: name.into(),
        }),
        date: OLD + i64::from(id) * 60,
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body,
    }
}

/// A message of the user's own.
fn own(id: i32, body: Body) -> Message {
    Message {
        outgoing: true,
        ..message(id, None, body)
    }
}

fn text(value: &str) -> Body {
    Body::Text(RichText::plain(value))
}

/// The history is a list of messages named by who, what, when and whether
/// edited; media says what it is; the divider, the jump button and the
/// day separators are there to be found.
fn history(handle: WindowHandle<Root>, cx: &mut App) {
    let mut edited = own(3, text("on my way"));
    edited.edit_date = Some(OLD + 400);
    let mut reply = message(4, Some("Alice"), text("great"));
    reply.reply_to = Some(MsgId(2));
    let messages = vec![
        message(1, Some("Alice"), text("hello there")),
        message(
            2,
            Some("Alice"),
            Body::Media {
                kind: MediaKind::Photo,
                caption: RichText::plain("the view"),
            },
        ),
        edited,
        reply,
        message(5, Some("Alice"), Body::Service("Alice joined".into())),
    ];
    send(
        handle,
        cx,
        vec![Event::History {
            thread: ThreadKey::whole(ALICE),
            view: ViewGen(1),
            dir: HistoryDir::Around,
            messages,
            removed: Vec::new(),
            has_more: true,
        }],
    );
    let tree = tree(handle, cx);
    assert_named(&tree);
    assert!(labels_of(&tree, "List").contains(&"Messages in Alice".to_owned()));
    let at = |id: i32| format::time_of_day(OLD + i64::from(id) * 60, &format::local_tz());
    let items = labels_of(&tree, "ListItem");
    for expected in [
        format!("Alice, hello there, {}", at(1)),
        format!("Alice, Photo, not shown, the view, {}", at(2)),
        format!("You, on my way, {}, edited", at(3)),
        format!("Alice, in reply to: [Photo], great, {}", at(4)),
        "Alice joined".to_owned(),
    ] {
        assert!(
            items.contains(&expected),
            "{expected:?} is not among {items:?}"
        );
    }
    // The placeholder says what it stands for and that nothing is shown.
    assert!(has_value(&tree, "Label", "Photo, not shown"));
    // The divider is a heading, where the read position is: after message 1.
    let headings = labels_of(&tree, "Heading");
    assert!(
        headings.contains(&chat_copy::NEW_SINCE_READ.to_owned()),
        "{headings:?}"
    );
    assert!(headings.contains(&"Alice".to_owned()), "{headings:?}");
    // Newer history exists, so the way to it is a named button.
    assert!(labels_of(&tree, "Button").contains(&"Jump to latest".to_owned()));
    // History is one tab stop that the keyboard scrolls, and the floating
    // button after it is a stop too.
    let history = find_node(&tree, "List", "Messages in Alice");
    assert!(
        history["aria"]["on_action"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "Focus")
    );
    tab_to(handle, cx, "Messages in Alice");
}

/// A message arriving in the open chat is announced politely, once per two
/// seconds: the next one, inside the gap, is not.
fn live_region(handle: WindowHandle<Root>, cx: &mut App) {
    let live = |tree: &Value| {
        nodes_of(tree, "Status")
            .iter()
            .find_map(|node| node["aria"]["value"].as_str().map(str::to_owned))
    };
    // The window is attached to its tail only once the newest page is in.
    send(
        handle,
        cx,
        vec![Event::History {
            thread: ThreadKey::whole(ALICE),
            view: ViewGen(1),
            dir: HistoryDir::Newer,
            messages: Vec::new(),
            removed: Vec::new(),
            has_more: false,
        }],
    );
    assert_eq!(live(&tree(handle, cx)), None, "nothing arrived yet");
    let arrive = |id: i32, who: &str| Event::History {
        thread: ThreadKey::whole(ALICE),
        view: ViewGen(1),
        dir: HistoryDir::Newer,
        messages: vec![message(id, Some(who), text("news"))],
        removed: Vec::new(),
        has_more: false,
    };
    send(handle, cx, vec![arrive(6, "Alice")]);
    assert_eq!(
        live(&tree(handle, cx)).as_deref(),
        Some("New message from Alice")
    );
    send(handle, cx, vec![arrive(7, "Bo")]);
    assert_eq!(
        live(&tree(handle, cx)).as_deref(),
        Some("New message from Alice"),
        "a second message inside two seconds is not announced"
    );
    std::thread::sleep(crate::chat::announce::MIN_GAP + Duration::from_millis(100));
    send(handle, cx, vec![arrive(8, "Bo")]);
    assert_eq!(
        live(&tree(handle, cx)).as_deref(),
        Some("New message from Bo")
    );
    // An own message is not news.
    std::thread::sleep(crate::chat::announce::MIN_GAP + Duration::from_millis(100));
    let own = own(9, text("ok"));
    send(
        handle,
        cx,
        vec![Event::History {
            thread: ThreadKey::whole(ALICE),
            view: ViewGen(1),
            dir: HistoryDir::Newer,
            messages: vec![own],
            removed: Vec::new(),
            has_more: false,
        }],
    );
    assert_eq!(
        live(&tree(handle, cx)).as_deref(),
        Some("New message from Bo")
    );
}

/// The numbers of the messages of Bo's history that are on screen: the list
/// holds only the rows it draws.
fn shown_messages(handle: WindowHandle<Root>, cx: &mut App) -> Vec<i32> {
    let items = labels_of(&tree(handle, cx), "ListItem");
    (1..=80)
        .filter(|&id| {
            let at = format::time_of_day(OLD + i64::from(id) * 60, &format::local_tz());
            items.contains(&format!("Bo, a line of history, {at}"))
        })
        .collect()
}

/// History is one tab stop, and Home, End and Page Up scroll it: a keyboard
/// reaches every loaded message.
fn keyboard_history(handle: WindowHandle<Root>, cx: &mut App) {
    key(handle, cx, "escape");
    let date = when(OLD);
    tab_to(handle, cx, &format!("Bo, last: [Photo], {date}"));
    key(handle, cx, "enter");
    let messages: Vec<Message> = (1..=80)
        .map(|id| {
            let mut m = message(id, Some("Bo"), text("a line of history"));
            m.thread = ThreadKey::whole(BO);
            m
        })
        .collect();
    send(
        handle,
        cx,
        vec![Event::History {
            thread: ThreadKey::whole(BO),
            view: ViewGen(2),
            dir: HistoryDir::Tail,
            messages,
            removed: Vec::new(),
            has_more: false,
        }],
    );
    tab_to(handle, cx, "Messages in Bo");
    let at_end = shown_messages(handle, cx);
    assert!(
        at_end.contains(&80) && !at_end.contains(&1),
        "a chat opens at its end: {at_end:?}"
    );
    key(handle, cx, "home");
    let top = shown_messages(handle, cx);
    assert!(
        top.contains(&1) && !top.contains(&80),
        "Home shows the start: {top:?}"
    );
    key(handle, cx, "pagedown");
    let paged = shown_messages(handle, cx);
    assert!(!paged.contains(&1), "Page Down moved on: {paged:?}");
    key(handle, cx, "pageup");
    assert!(
        shown_messages(handle, cx).contains(&1),
        "Page Up moved back"
    );
    key(handle, cx, "end");
    let end = shown_messages(handle, cx);
    assert!(
        end.contains(&80) && !end.contains(&1),
        "End shows the latest: {end:?}"
    );
    assert_named(&tree(handle, cx));
}

/// The composer reads as lines: a run for each, so a reader finds the line
/// the caret is on, and the caret is where `AppKit` says.
fn composer_lines(handle: WindowHandle<Root>, cx: &mut App) {
    tab_to(handle, cx, "Message to Bo");
    type_text(handle, cx, "ab");
    key(handle, cx, "shift-enter");
    type_text(handle, cx, "cd");
    let composer = tree(handle, cx);
    let field = find_node(&composer, "MultilineTextInput", "Message to Bo");
    let runs: Vec<_> = field["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| &composer["nodes"][id.as_str().unwrap()])
        .filter(|node| node["aria"]["role"] == "TextRun")
        .map(|node| node["aria"]["value"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(runs, ["ab\n", "cd"]);
    assert_selection(handle, cx, "Message to Bo", NSRange::new(5, 0));
    key(handle, cx, "up");
    let _ = tree(handle, cx);
    assert_selection(handle, cx, "Message to Bo", NSRange::new(2, 0));
}

fn topic(n: i32, title: &str) -> Topic {
    Topic {
        id: TopicId::try_from(n).unwrap(),
        title: title.into(),
        created: OLD,
        closed: false,
        pinned: false,
        unread: 0,
        unread_mentions: 0,
        freshness: Freshness::Current,
        read_inbox_max: MsgId(n),
        last: None,
        last_date: OLD,
        last_id: Some(MsgId(n)),
    }
}

/// Tab until the focus is on the element named `label`.
fn tab_to(handle: WindowHandle<Root>, cx: &mut App, label: &str) {
    for _ in 0..100 {
        key(handle, cx, "tab");
        let tree = tree(handle, cx);
        let focused = tree["gpui_focus"].as_str().unwrap();
        if tree["nodes"][focused]["aria"]["label"] == label {
            return;
        }
    }
    panic!("Tab never reached {label}");
}

/// A forum shows its topics as a list of rows with their marks; opening a
/// closed topic leaves a note where the box was, and Escape from there goes
/// back to the topic's row in the sidebar.
fn forum(handle: WindowHandle<Root>, cx: &mut App) {
    // The composer holds a draft of Bo's: leaving the chat keeps it.
    key(handle, cx, "escape");
    let date = when(OLD);
    tab_to(handle, cx, &format!("Bo, last: [Photo], {date}"));
    tab_to(
        handle,
        cx,
        &format!("Family, 2 unread, 1 mention, group, {date}"),
    );
    tab_to(handle, cx, &format!("Forum, forum, {date}"));
    key(handle, cx, "enter");
    let rules = Topic {
        closed: true,
        pinned: true,
        unread: 2,
        ..topic(14, "Rules")
    };
    send(
        handle,
        cx,
        vec![Event::TopicsReset {
            chat: FORUM,
            view: TopicsGen(1),
            order: TopicOrder::LastActivity,
            topics: vec![rules, topic(15, "Chat")],
            can_post_closed: false,
        }],
    );
    let topics = tree(handle, cx);
    assert_named(&topics);
    let list = labels_of(&topics, "List");
    assert!(list.contains(&"Topics in Forum".to_owned()), "{list:?}");
    let items = labels_of(&topics, "ListItem");
    let rules_row = format!("Rules, 2 unread, closed, pinned, {date}");
    for expected in [rules_row.clone(), format!("Chat, {date}")] {
        assert!(
            items.contains(&expected),
            "{expected:?} is not among {items:?}"
        );
    }
    assert!(labels_of(&topics, "Heading").contains(&"Forum, forum".to_owned()));
    // The forum's own row shows its disclosure, with its state.
    let disclosure = find_node(&topics, "Button", "Hide topics");
    assert_eq!(disclosure["aria"]["expanded"], true);

    // The inline topic row opens the topic.
    tab_to(handle, cx, &rules_row);
    key(handle, cx, "enter");
    let open = tree(handle, cx);
    assert!(labels_of(&open, "Button").contains(&"Back to topics".to_owned()));
    assert!(labels_of(&open, "Heading").contains(&"Forum, Rules, closed".to_owned()));
    assert_named(&open);
    // The box is replaced by a note; nothing is left to send with.
    assert!(has_value(&open, "Label", chat_copy::TOPIC_CLOSED));
    assert!(!labels_of(&open, "Button").contains(&"Send message".to_owned()));
    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), &rules_row);
}

/// A chat only admins post in: a note says why there is no box.
fn composer_blocks(handle: WindowHandle<Root>, cx: &mut App) {
    tab_to(handle, cx, &format!("News, group, {}", when(OLD)));
    key(handle, cx, "enter");
    let read_only = tree(handle, cx);
    assert!(has_value(&read_only, "Label", chat_copy::READ_ONLY));
    assert_named(&read_only);
}

/// The floating Jump button is reached by Tab, and when it is used, focus
/// stays in the messages instead of going with the button.
fn jump_button(handle: WindowHandle<Root>, cx: &mut App) {
    key(handle, cx, "escape");
    tab_to(
        handle,
        cx,
        &format!("Family, 2 unread, 1 mention, group, {}", when(OLD)),
    );
    key(handle, cx, "enter");
    let mut older = message(1, Some("Mo"), text("early"));
    older.thread = ThreadKey::whole(FAMILY);
    send(
        handle,
        cx,
        vec![Event::History {
            thread: ThreadKey::whole(FAMILY),
            view: ViewGen(3),
            dir: HistoryDir::Around,
            messages: vec![older],
            removed: Vec::new(),
            has_more: true,
        }],
    );
    tab_to(handle, cx, "Jump to latest");
    cx.set_reduce_motion(true);
    key(handle, cx, "enter");
    assert_focus(&tree(handle, cx), "Messages in Family");
    cx.set_reduce_motion(false);
}
