//! Round 23 (review of the exit SIGSEGV fix): shutdown closes core's
//! caches alongside network retirement, not after it, and admits no new
//! cache open while it runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeroize::Zeroizing;

use super::{CacheDir, Setup, chat, start};
use crate::actor::logout::{erase_cache, open_cache};
use crate::api::{Command, HistoryDir};
use crate::backend::fake::{FakeBackend, FakeScript, QuitBehavior};
use crate::cache::{Cache, CacheError, CacheKey, Refused, wait_closed};
use crate::model::ThreadKey;
use crate::secret::{MemoryStore, SecretItem, SecretStore, StoreError};
use crate::session::PersistHandle;

/// Finding 3: with a runner that ignores quit, retirement runs to its 2 s
/// cap. The cache must be closed well before that, while admission to its
/// directory is closed; once the actor is done the directory reopens.
/// With the caches closed only after retirement, the census is still
/// non-empty after 1 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_the_cache_while_retirement_still_runs() {
    let dir = CacheDir::new();
    let (backend, fake) = FakeBackend::new(FakeScript::demo(1, 50));
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.send(Command::OpenThread(ThreadKey::whole(chat(1)))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.fake.set_quit(QuitBehavior::Ignore);

    h.cancel.cancel();
    let path = dir.path().to_path_buf();
    let drained = tokio::task::spawn_blocking(move || {
        let drained = wait_closed(Some(&path), Duration::from_secs(1));
        let key = CacheKey::generate().unwrap();
        (drained, Cache::open(&path, &key))
    });
    let (drained, reopened) = drained.await.unwrap();
    assert!(drained, "the cache closed while retirement ran");
    assert!(!h.join.is_finished(), "retirement is still running");
    assert!(matches!(
        reopened,
        Err(CacheError::Refused(Refused::Closing))
    ));
    h.join.await.unwrap();

    let path = dir.path().to_path_buf();
    let key = CacheKey::generate().unwrap();
    let after = tokio::task::spawn_blocking(move || Cache::open(&path, &key));
    // Admitted again (the key is not this cache's, so it cannot read it).
    assert!(matches!(after.await.unwrap(), Err(CacheError::WrongKey)));
}

/// A store whose cache-key reads can be paused: once armed, the next read
/// of `SecretItem::CacheKey` reports on `entered` and waits for `release`.
struct GatedStore {
    inner: MemoryStore,
    gate: Arc<KeyGate>,
}

struct KeyGate {
    armed: AtomicBool,
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SecretStore for GatedStore {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        if item == SecretItem::CacheKey && self.gate.armed.swap(false, Ordering::SeqCst) {
            self.gate.entered.lock().unwrap().send(()).unwrap();
            self.gate.release.lock().unwrap().recv().unwrap();
        }
        self.inner.get(item)
    }

    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError> {
        self.inner.set(item, secret)
    }

    fn delete(&self, item: SecretItem) -> Result<(), StoreError> {
        self.inner.delete(item)
    }
}

/// Round 24's test, made deterministic in round 27: `erase_cache` keeps
/// the cache's directory in the census from before the old cache closes
/// until the new one is open, so a drain cannot finish in between. The real
/// `open_cache` and `erase_cache` run over a store that pauses the reopen's
/// cache-key read, which comes after the old worker has closed its database
/// and been joined. While paused, the census must still list the
/// directory. Without the hold it is empty there, every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn erase_and_reopen_never_leave_the_census_empty() {
    let dir = CacheDir::new();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let gate = Arc::new(KeyGate {
        armed: AtomicBool::new(false),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let store = GatedStore {
        inner: MemoryStore::new(),
        gate: Arc::clone(&gate),
    };
    let persist = PersistHandle::spawn(store).unwrap();
    let (handle, key) = open_cache(&persist, dir.path().to_path_buf())
        .await
        .unwrap();

    gate.armed.store(true, Ordering::SeqCst);
    let path = dir.path().to_path_buf();
    let erasing =
        tokio::spawn(async move { erase_cache(&persist, Some(handle), path, Some(key)).await });
    let path = dir.path().to_path_buf();
    let drained_while_paused = tokio::task::spawn_blocking(move || {
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        let drained = wait_closed(Some(&path), Duration::ZERO);
        release.send(()).unwrap();
        drained
    })
    .await
    .unwrap();
    let (erased, reopened) = erasing.await.unwrap();
    assert!(
        !drained_while_paused,
        "the census drained between the old cache's close and the reopen"
    );
    assert!(erased.is_ok());
    let (handle, _key) = reopened.expect("a fresh cache for the next account");
    handle.close().await.unwrap();
}
