//! Review round 15, B: account A's chat state must not survive a logout
//! into account B, even when B has a chat with the same peer id.

use abyss_core::api::{Command, HistoryDir, SendState};
use abyss_core::model::{Body, ChatKind, MsgId, PeerFolder, PeerKind, ReadState, ThreadKey};
use abyss_core::rich_text::RichText;
use abyss_core::store::ViewGen;

use super::*;

/// The same peer id in both accounts.
const SHARED: ChatId = ChatId {
    kind: PeerKind::User,
    id: 7,
};
const SHARED_THREAD: ThreadKey = ThreadKey::whole(SHARED);

fn summary(title: &str) -> ChatSummary {
    ChatSummary {
        id: SHARED,
        title: title.into(),
        kind: ChatKind::Private { contact: false },
        pinned: false,
        read: ReadState::default(),
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
        muted_until: 0,
        folder: PeerFolder::Main,
        reading: None,
        topic_unread: None,
    }
}

fn tail(ids: std::ops::RangeInclusive<i32>) -> Event {
    let messages = ids
        .map(|id| Message {
            id: MsgId(id),
            thread: ThreadKey {
                chat: SHARED,
                topic: None,
            },
            sender: None,
            date: i64::from(id),
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body: Body::Text(RichText::plain("from A")),
        })
        .collect();
    Event::History {
        thread: SHARED_THREAD,
        view: ViewGen(1),
        dir: HistoryDir::Tail,
        messages,
        removed: Vec::new(),
        has_more: true,
    }
}

#[test]
fn logout_forgets_account_a_before_account_b_opens_the_same_peer() {
    let mut sent = Vec::new();
    let mut admit = |cmd: Command| -> Result<(), Refused> {
        sent.push(cmd);
        Ok(())
    };
    let mut shell = ChatShell::default();
    // Account A: the chat is open, messages shown, a send pending.
    shell.apply(Event::DialogsReset(vec![summary("A's chat")]));
    shell.select(SHARED_THREAD.chat, &mut admit).unwrap();
    shell.apply(tail(1..=20));
    let target = shell.send_target().unwrap().unwrap();
    let local = shell
        .submit(target, "draft for A", 0, &mut admit)
        .unwrap()
        .unwrap();
    shell.apply(Event::SendState {
        local_id: local,
        state: SendState::Sending,
    });
    assert!(shell.row_count() > 20);

    // Teardown begins, then account B signs in with the same peer id.
    shell.reset();
    assert_eq!(shell.selected(), None);
    assert_eq!(shell.row_count(), 0);
    assert!(shell.window().is_none());
    shell.apply(Event::DialogsReset(vec![summary("B's chat")]));
    assert_eq!(shell.row_count(), 0, "A's messages or echoes shown to B");
    sent.clear();
    let mut admit = |cmd: Command| -> Result<(), Refused> {
        sent.push(cmd);
        Ok(())
    };
    shell.select(SHARED_THREAD.chat, &mut admit).unwrap();
    assert!(
        matches!(sent.as_slice(), [Command::OpenThread(SHARED_THREAD), ..]),
        "B's chat is opened afresh: {sent:?}"
    );
    assert_eq!(shell.row_count(), 0);
    // A's old send state never lands on a new send.
    let target = shell.send_target().unwrap().unwrap();
    let mut admit = |_: Command| -> Result<(), Refused> { Ok(()) };
    let next = shell.submit(target, "hi", 0, &mut admit).unwrap().unwrap();
    assert_ne!(next, local);
}
