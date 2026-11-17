//! The baseline race of the update stream, over a scripted source.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use grammers_client::InvocationError;
use tokio::sync::{mpsc, watch};

use crate::backend::grammers::stream::{Pull, Source, Updates};
use crate::backend::{StreamError, StreamItem, Update};
use crate::model::{ChatId, PeerKind};

type Item = Result<Option<Update>, InvocationError>;

/// Like grammers: the first pull records the baseline (`getState`), then
/// every pull waits for the next scripted item.
struct Script {
    items: mpsc::Receiver<Item>,
    baseline: Option<Arc<watch::Sender<bool>>>,
    pulls: Arc<AtomicUsize>,
}

impl Source for Script {
    type Raw = Option<Update>;

    fn pull(mut self) -> Pull<Self> {
        self.pulls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(flag) = self.baseline.take() {
                flag.send_replace(true);
                // Keep the sender alive, as the session does.
                self.baseline = Some(flag);
            }
            let res = match self.items.recv().await {
                Some(item) => item,
                None => Err(InvocationError::Dropped("script ended")),
            };
            (self, res)
        })
    }

    fn map(raw: &Self::Raw) -> Option<Update> {
        raw.clone()
    }
}

struct Harness {
    updates: Updates<Script>,
    items: mpsc::Sender<Item>,
    pulls: Arc<AtomicUsize>,
    _flag: Arc<watch::Sender<bool>>,
}

/// Whether the source records a baseline (`getState` succeeded).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Baseline {
    Recorded,
    Missing,
}

fn harness(baseline: Baseline) -> Harness {
    let (tx, rx) = mpsc::channel(8);
    let flag = Arc::new(watch::channel(false).0);
    let pulls = Arc::new(AtomicUsize::new(0));
    let script = Script {
        items: rx,
        baseline: (baseline == Baseline::Recorded).then(|| Arc::clone(&flag)),
        pulls: Arc::clone(&pulls),
    };
    Harness {
        updates: Updates::new(script, flag.subscribe()),
        items: tx,
        pulls,
        _flag: flag,
    }
}

fn read(still_unread: u32) -> Update {
    Update::ReadInbox {
        chat: ChatId {
            kind: PeerKind::User,
            id: 7,
        },
        topic: None,
        still_unread,
    }
}

async fn next(h: &mut Harness) -> Result<StreamItem, StreamError> {
    tokio::time::timeout(Duration::from_secs(5), h.updates.next())
        .await
        .expect("next must not hang")
}

fn unread(item: Result<StreamItem, StreamError>) -> u32 {
    match item {
        Ok(StreamItem::Update(Update::ReadInbox { still_unread, .. })) => still_unread,
        other => panic!("expected an update, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn baseline_is_reported_while_the_first_pull_waits() {
    let mut h = harness(Baseline::Recorded);
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline)));
    h.items.send(Ok(Some(read(1)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 1);
    assert_eq!(
        h.pulls.load(Ordering::SeqCst),
        1,
        "the pull interrupted by the baseline was resumed, not restarted"
    );
}

#[tokio::test(start_paused = true)]
async fn baseline_comes_before_an_update_that_raced_it() {
    let mut h = harness(Baseline::Recorded);
    h.items.send(Ok(Some(read(2)))).await.unwrap();
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline)));
    assert_eq!(unread(next(&mut h).await), 2);
}

#[tokio::test(start_paused = true)]
async fn errors_are_classified_and_the_stream_is_reused() {
    let mut h = harness(Baseline::Recorded);
    assert!(matches!(next(&mut h).await, Ok(StreamItem::Baseline)));
    let io = InvocationError::Io(std::io::Error::other("reset"));
    h.items.send(Err(io)).await.unwrap();
    assert_eq!(next(&mut h).await.unwrap_err(), StreamError::Transient);
    h.items.send(Ok(None)).await.unwrap();
    h.items.send(Ok(Some(read(3)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 3, "ignored updates are skipped");
    assert_eq!(h.pulls.load(Ordering::SeqCst), 3);
    h.items
        .send(Err(InvocationError::Dropped("runner gone")))
        .await
        .unwrap();
    assert_eq!(next(&mut h).await.unwrap_err(), StreamError::Fatal);
}

#[tokio::test(start_paused = true)]
async fn without_a_baseline_updates_still_flow() {
    // `getState` failed: core's bootstrap times out and rebuilds; the stream
    // itself keeps delivering.
    let mut h = harness(Baseline::Missing);
    h.items.send(Ok(Some(read(4)))).await.unwrap();
    assert_eq!(unread(next(&mut h).await), 4);
}
