# Draft grammers message-box issues

All examples use grammers `e61cf8585e1e9969ab81eda9fb4725a1500e2d86` (0.12.0). They use synthetic TL values and the public `grammers_session::updates::MessageBoxes` API. No Telegram account or RPC connection is needed.

Each example uses these imports and helpers. The assertions capture the observed behavior at this revision.

```rust
use grammers_client::tl;
use grammers_session::updates::{MessageBoxes, UpdatesLike};

fn state(pts: i32, qts: i32) -> tl::types::updates::State {
    tl::types::updates::State { pts, qts, date: 1, seq: 0, unread_count: 0 }
}

fn boxes() -> MessageBoxes {
    let mut boxes = MessageBoxes::new();
    boxes.set_state(state(10, 1));
    boxes
}

fn short(update: tl::enums::Update) -> UpdatesLike {
    UpdatesLike::Updates(tl::types::UpdateShort { update, date: 1 }.into())
}

fn channel_delete(pts: i32) -> tl::enums::Update {
    tl::types::UpdateDeleteChannelMessages {
        channel_id: 5,
        messages: vec![6],
        pts,
        pts_count: 1,
    }.into()
}
```

## Channel hint replaces the stored channel PTS (A43/A65)

A channel is stored at PTS 3. Processing an `updateChannelTooLong` carrying PTS 10 starts the next difference at 10. The expected request starts at 3, so updates in the intervening range can be retrieved. Telegram specifies using the hint's PTS when the client lacks a stored position. [Telegram's updateChannelTooLong reference](https://core.telegram.org/constructor/updateChannelTooLong)

```rust
let mut boxes = boxes();
boxes.try_set_channel_state(5, 3);
boxes.process_updates(short(tl::types::UpdateChannelTooLong {
    channel_id: 5,
    pts: Some(10),
}.into())).unwrap();

assert_eq!(boxes.get_channel_difference().unwrap().pts, 10);
```

## Final channel difference skips an ordered update in its body (A44)

A final channel difference advances PTS from 3 to 4 and supplies a deletion at PTS 4. `apply_channel_difference` installs PTS 4 and leaves the pending difference before processing the body. Its result contains zero updates. The expected result delivers the deletion and advances the position to 4. [Telegram's channel difference reference](https://core.telegram.org/constructor/updates.channelDifference), [Telegram's gap recovery documentation](https://core.telegram.org/api/updates#recovering-gaps)

```rust
let mut boxes = boxes();
boxes.try_set_channel_state(5, 3);
boxes.process_updates(short(tl::types::UpdateChannelTooLong {
    channel_id: 5,
    pts: None,
}.into())).unwrap();
assert_eq!(boxes.get_channel_difference().unwrap().pts, 3);

let (updates, _, _) = boxes.apply_channel_difference(
    tl::types::updates::ChannelDifference {
        r#final: true,
        pts: 4,
        timeout: None,
        new_messages: vec![],
        other_updates: vec![channel_delete(4)],
        chats: vec![],
        users: vec![],
    }.into()
);
assert!(updates.is_empty());
assert_eq!(boxes.session_state().channels[0].pts, 4);
```

## Secondary-only deadline skips ordered Common updates (N50)

A Secondary QTS gap starts an account difference using Common's PTS 10. A reply containing a Common deletion at PTS 11 advances the Common position to 11. Since Common has no pending difference, the deletion is treated as a duplicate and omitted. The expected result delivers it. The request includes both Common and Secondary positions. [Telegram's account difference reference](https://core.telegram.org/method/updates.getDifference), [Telegram's gap recovery documentation](https://core.telegram.org/api/updates#recovering-gaps)

```rust
let mut boxes = boxes();
boxes.process_updates(short(tl::types::UpdateBotStopped {
    user_id: 7,
    date: 1,
    stopped: true,
    qts: 3,
}.into())).unwrap();
assert!(boxes.get_difference().is_none());
std::thread::sleep(std::time::Duration::from_millis(600));
boxes.check_deadlines();
let request = boxes.get_difference().unwrap();
assert_eq!((request.pts, request.qts), (10, 1));

let (updates, _, _) = boxes.apply_difference(tl::types::updates::Difference {
    new_messages: vec![],
    new_encrypted_messages: vec![],
    other_updates: vec![tl::types::UpdateDeleteMessages {
        messages: vec![6], pts: 11, pts_count: 1,
    }.into()],
    chats: vec![],
    users: vec![],
    state: state(11, 3).into(),
}.into());
assert!(updates.is_empty());
assert_eq!(boxes.session_state().pts, 11);
```

## Account PTS reset has no dedicated message-box transition (A47/A67)

Processing `updatePtsChanged` returns the unsequenced update and leaves Common's PTS and pending requests unchanged. Telegram instructs clients to fetch `updates.getState`. A public reset transition would let callers install the replacement account state while keeping independent channel positions and pending channel requests. [Telegram's updatePtsChanged reference](https://core.telegram.org/constructor/updatePtsChanged)

```rust
let mut boxes = boxes();
let (updates, _, _) = boxes.process_updates(short(tl::enums::Update::PtsChanged)).unwrap();
assert!(matches!(updates[0].0, tl::enums::Update::PtsChanged));
assert_eq!(boxes.session_state().pts, 10);
assert!(boxes.get_difference().is_none());
```

## A channel deadline loses the distinction between a gap and idle polling (A62)

The 500 ms reorder deadline discards the buffered socket update as it starts the channel difference. Both a reorder deadline and an idle deadline expose the same request shape. If recovery omits an update, a caller with a persistent cache cannot identify the unresolved reorder gap through the public API. A recovery-cause signal would let callers invalidate the affected history. The synthetic empty reply below isolates the API behavior when the reply supplies no update. [Telegram's gap recovery documentation](https://core.telegram.org/api/updates#recovering-gaps)

```rust
let mut boxes = boxes();
boxes.try_set_channel_state(5, 3);
let (updates, _, _) = boxes.process_updates(short(channel_delete(5))).unwrap();
assert!(updates.is_empty());
std::thread::sleep(std::time::Duration::from_millis(600));
boxes.check_deadlines();
assert_eq!(boxes.get_channel_difference().unwrap().pts, 3);

let (updates, _, _) = boxes.apply_channel_difference(
    tl::types::updates::ChannelDifferenceEmpty { r#final: true, pts: 5, timeout: None }.into()
);
assert!(updates.is_empty());
assert_eq!(boxes.session_state().channels[0].pts, 5);
```
