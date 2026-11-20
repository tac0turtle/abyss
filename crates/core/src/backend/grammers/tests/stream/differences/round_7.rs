use super::*;
use grammers_session::types::ChannelKind;

fn delete(pts: i32) -> tl::enums::Update {
    tl::types::UpdateDeleteMessages {
        messages: vec![6],
        pts,
        pts_count: 1,
    }
    .into()
}

async fn fresh(
    session: Arc<AbyssSession>,
    replies: Vec<Vec<u8>>,
) -> (Feed<Arc<Mock>>, mpsc::Sender<UpdatesLike>) {
    let mock = Mock::with(
        std::iter::once(state(10).to_bytes())
            .chain(replies)
            .map(Ok)
            .collect(),
    );
    let (socket, input) = mpsc::channel(1);
    let mut feed = Feed::new(mock, session, input, None);
    checkpoint_of(&mut feed).await;
    (feed, socket)
}

async fn next(feed: &mut Feed<Arc<Mock>>) -> StreamItem {
    tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn common_gap_deleted_messages_are_unscoped_after_an_empty_reply() {
    let (mut feed, socket) = fresh(session(), vec![empty_account_difference()]).await;
    socket.send(socket_batch(vec![delete(12)])).await.unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
    assert_stale(&next(&mut feed).await, None);
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
}

#[tokio::test]
async fn common_gap_edit_is_missing_even_if_the_reply_advances_pts() {
    let (mut feed, socket) = fresh(session(), vec![account_reply(12, vec![])]).await;
    socket
        .send(socket_batch(vec![common_edit(12, 1)]))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
    assert_stale(
        &next(&mut feed).await,
        Some(ChatId {
            kind: PeerKind::User,
            id: 7,
        }),
    );
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
}

#[tokio::test]
async fn common_gap_is_covered_by_the_edit_in_the_reply() {
    let (mut feed, socket) =
        fresh(session(), vec![account_reply(12, vec![common_edit(12, 1)])]).await;
    socket
        .send(socket_batch(vec![common_edit(12, 1)]))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::CatchUp(Update::Edited(_))
    ));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpEnded));
}

#[tokio::test]
async fn common_gap_from_affected_messages_is_detected() {
    let (mut feed, socket) = fresh(session(), vec![empty_account_difference()]).await;
    socket
        .send(UpdatesLike::AffectedMessages(
            tl::types::messages::AffectedMessages {
                pts: 12,
                pts_count: 1,
            },
        ))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
    assert_stale(&next(&mut feed).await, None);
}

fn envelope(seq: i32, updates: Vec<tl::enums::Update>, chats: Vec<tl::enums::Chat>) -> UpdatesLike {
    UpdatesLike::Updates(
        tl::types::Updates {
            updates,
            users: vec![],
            chats,
            date: 2,
            seq,
        }
        .into(),
    )
}

#[tokio::test]
async fn rejected_sequence_envelope_invalidates_closed_channel_history() {
    let (mut feed, socket) = fresh(channel_session().await, vec![empty_account_difference()]).await;
    socket
        .send(envelope(2, vec![channel_delete()], vec![]))
        .await
        .unwrap();
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
}

#[tokio::test]
async fn rejected_sequence_envelope_is_covered_by_the_account_reply() {
    let (mut feed, socket) = fresh(
        channel_session().await,
        vec![account_reply(11, vec![channel_delete()])],
    )
    .await;
    socket
        .send(envelope(2, vec![channel_delete()], vec![]))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::CatchUp(Update::Deleted { .. })
    ));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
}

#[tokio::test]
async fn channel_gap_offered_by_account_difference_is_detected() {
    let (mut feed, socket) = fresh(
        channel_session().await,
        vec![
            account_reply(
                11,
                vec![
                    tl::types::UpdateChannelWebPage {
                        channel_id: CHANNEL,
                        webpage: tl::types::WebPageEmpty { id: 1, url: None }.into(),
                        pts: 5,
                        pts_count: 1,
                    }
                    .into(),
                ],
            ),
            empty_channel_difference(5),
        ],
    )
    .await;
    socket
        .send(socket_batch(vec![channel_new(CHANNEL, 3)]))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
    socket.send(UpdatesLike::ConnectionClosed).await.unwrap();
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpEnded));
    assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
}

#[tokio::test]
async fn evicting_a_channel_with_an_unresolved_gap_invalidates_history() {
    let (mut feed, socket) = fresh(channel_session().await, vec![]).await;
    let cap = i64::try_from(crate::store::MAX_DIALOGS).unwrap();
    socket
        .send(socket_batch(vec![channel_new(CHANNEL, 3)]))
        .await
        .unwrap();
    checkpoint_of(&mut feed).await;
    socket
        .send(socket_batch(
            (100..100 + cap - 1).map(|id| channel_new(id, 1)).collect(),
        ))
        .await
        .unwrap();
    assert_eq!(
        checkpoint_of(&mut feed).await.channels.len(),
        usize::try_from(cap).unwrap()
    );
    socket
        .send(socket_batch(vec![channel_delete_at(6, 5)]))
        .await
        .unwrap();
    checkpoint_of(&mut feed).await;
    socket
        .send(socket_batch(vec![channel_new(100 + cap, 1)]))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::CatchUp(Update::NewMessage(_))
    ));
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
    assert!(
        !checkpoint_of(&mut feed)
            .await
            .channels
            .iter()
            .any(|c| c.id == CHANNEL)
    );
}

#[tokio::test]
async fn duplicate_sequenced_signals_have_no_side_effects() {
    let (mut feed, socket) =
        fresh(channel_session().await, vec![empty_channel_difference(10)]).await;
    let hinted = || envelope(1, vec![channel_hint(10), tl::enums::Update::Config], vec![]);
    socket.send(hinted()).await.unwrap();
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
    assert_eq!(checkpoint_of(&mut feed).await.seq, 1);
    for replay in [
        hinted(),
        envelope(1, vec![tl::enums::Update::PtsChanged], vec![]),
    ] {
        socket.send(replay).await.unwrap();
        assert!(
            matches!(next(&mut feed).await, StreamItem::Checkpoint(s) if s.0.seq == 1 && s.0.pts == 10)
        );
    }
}

#[tokio::test]
async fn a_channel_message_omitted_from_account_replay_invalidates_history() {
    let mock = Mock::with(vec![
        Ok(account_reply(11, vec![channel_new(CHANNEL, 4)])),
        Ok(empty_channel_difference(4)),
    ]);
    let (_socket, input) = mpsc::channel(1);
    let mut feed = Feed::new(
        mock,
        channel_session().await,
        input,
        Some(resumed_with_channel()),
    );
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
}

#[tokio::test]
async fn a_resumed_known_broadcast_channel_needs_no_follow_up_reconcile() {
    let session = session();
    session
        .cache_peer(PeerInfo::Channel {
            id: CHANNEL,
            auth: None,
            kind: Some(ChannelKind::Broadcast),
        })
        .await
        .unwrap();
    let mock = Mock::with(vec![Ok(empty_account_difference())]);
    let (_socket, input) = mpsc::channel(1);
    let mut feed = Feed::new(
        Arc::clone(&mock),
        session,
        input,
        Some(resumed_with_channel()),
    );
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpEnded));
    assert!(checkpoint_of(&mut feed).await.channels.is_empty());
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn a_live_broadcast_hint_never_invalidates_or_checkpoints_the_channel() {
    let (mut feed, socket) = fresh(session(), vec![]).await;
    let mut broadcast = super::super::super::channel(CHANNEL, "broadcast");
    broadcast.broadcast = true;
    broadcast.megagroup = false;
    broadcast.access_hash = None;
    socket
        .send(envelope(
            0,
            vec![channel_new(CHANNEL, 3), channel_hint(10)],
            vec![broadcast.into()],
        ))
        .await
        .unwrap();
    assert!(matches!(next(&mut feed).await, StreamItem::Checkpoint(s) if s.0.channels.is_empty()));
    assert_eq!(feed.tracked_channels(), 0);
    socket
        .send(socket_batch(vec![channel_hint(15), common_edit(11, 1)]))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::Update(Update::Edited(_))
    ));
    assert!(
        matches!(next(&mut feed).await, StreamItem::Checkpoint(s) if s.0.pts == 11 && s.0.channels.is_empty())
    );
    assert_eq!(feed.tracked_channels(), 0);
}

#[tokio::test]
async fn a_short_message_gap_uses_the_peer_when_available() {
    use tl::enums::Updates;
    let private = tl::types::UpdateShortMessage {
        out: false,
        mentioned: false,
        media_unread: false,
        silent: false,
        id: 11,
        user_id: 7,
        message: "short".into(),
        pts: 12,
        pts_count: 1,
        date: 1,
        fwd_from: None,
        via_bot_id: None,
        reply_to: None,
        entities: None,
        ttl_period: None,
    };
    let group = tl::types::UpdateShortChatMessage {
        out: false,
        mentioned: false,
        media_unread: false,
        silent: false,
        id: 11,
        from_id: 7,
        chat_id: 8,
        message: "short".into(),
        pts: 12,
        pts_count: 1,
        date: 1,
        fwd_from: None,
        via_bot_id: None,
        reply_to: None,
        entities: None,
        ttl_period: None,
    };
    let bare_sent = tl::types::UpdateShortSentMessage {
        out: true,
        id: 11,
        pts: 12,
        pts_count: 1,
        date: 1,
        media: None,
        entities: None,
        ttl_period: None,
    };
    for (update, scope) in [
        (
            Updates::UpdateShortMessage(private),
            Some(ChatId {
                kind: PeerKind::User,
                id: 7,
            }),
        ),
        (
            Updates::UpdateShortChatMessage(group),
            Some(ChatId {
                kind: PeerKind::Chat,
                id: 8,
            }),
        ),
        (Updates::UpdateShortSentMessage(bare_sent), None),
    ] {
        let (mut feed, socket) = fresh(session(), vec![empty_account_difference()]).await;
        socket.send(UpdatesLike::Updates(update)).await.unwrap();
        assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
        assert_stale(&next(&mut feed).await, scope);
    }
}

#[tokio::test]
async fn a_common_gap_filled_by_socket_reordering_needs_no_reconcile() {
    let (mut feed, socket) = fresh(session(), vec![]).await;
    socket
        .send(socket_batch(vec![common_edit(12, 1)]))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.pts, 10);
    socket.send(socket_batch(vec![delete(11)])).await.unwrap();
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::Update(Update::Deleted { .. })
    ));
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::Update(Update::Edited(_))
    ));
    assert_eq!(checkpoint_of(&mut feed).await.pts, 12);
}

#[tokio::test]
async fn later_messages_in_the_same_replay_cannot_reseed_stale_history() {
    let reply = ChannelDifference::Difference(tl::types::updates::ChannelDifference {
        r#final: true,
        pts: 11,
        timeout: None,
        new_messages: vec![message(peer_channel(CHANNEL), 11, "after hint").into()],
        other_updates: vec![
            tl::types::UpdateMessageId {
                id: 11,
                random_id: 7,
            }
            .into(),
        ],
        chats: vec![],
        users: vec![],
    });
    let (mut feed, socket) = fresh(channel_session().await, vec![reply.to_bytes()]).await;
    socket
        .send(socket_batch(vec![channel_new(CHANNEL, 3)]))
        .await
        .unwrap();
    assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 3);
    socket
        .send(socket_batch(vec![
            tl::types::UpdateNewChannelMessage {
                message: message(peer_channel(CHANNEL), 4, "before hint").into(),
                pts: 4,
                pts_count: 1,
            }
            .into(),
            channel_hint(10),
        ]))
        .await
        .unwrap();
    assert!(
        matches!(next(&mut feed).await, StreamItem::CatchUp(Update::NewMessage(new)) if new.message.id == MsgId(4))
    );
    assert_stale(&next(&mut feed).await, Some(CHANNEL_CHAT));
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::CatchUp(Update::MessageId { id: MsgId(11), .. })
    ));
    assert!(matches!(next(&mut feed).await, StreamItem::CatchUpGap));
    assert_eq!(checkpoint_of(&mut feed).await.channels[0].pts, 11);
    socket
        .send(socket_batch(vec![channel_new(CHANNEL, 12)]))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut feed).await,
        StreamItem::Update(Update::NewMessage(_))
    ));
}

#[tokio::test]
async fn a_broadcast_post_without_a_directory_cannot_seed_a_checkpoint() {
    let (mut feed, socket) = fresh(session(), vec![]).await;
    let mut post = message(peer_channel(CHANNEL), 1, "broadcast post");
    post.post = true;
    socket
        .send(socket_batch(vec![
            tl::types::UpdateNewChannelMessage {
                message: post.into(),
                pts: 3,
                pts_count: 1,
            }
            .into(),
        ]))
        .await
        .unwrap();
    assert!(matches!(next(&mut feed).await, StreamItem::Checkpoint(s) if s.0.channels.is_empty()));
    assert_eq!(feed.tracked_channels(), 0);
}
