//! Binding the message cache to a verified account (plan section 16; review
//! round 8, task 6c).
//!
//! The cache records its owner (the self user id) in its `meta` table. Rows
//! are served only while the cache's owner is the signed-in account:
//! - **Startup, stored session:** the session's self user is the account its
//!   auth keys belong to. A cache owned by it (or empty and unowned) is
//!   served; anything else is crypto-erased and recreated first.
//! - **Startup, signed out** (no session item, or an unreadable or unknown
//!   one): nobody can vouch for the cache's rows. An owned or non-empty cache
//!   is crypto-erased, which also retries a logout whose key deletion failed.
//! - **Startup, session store unreadable:** the cache may be the user's, so it
//!   is kept but quarantined (open, never read) until a login says whose it is.
//! - **Login:** the cache is bound to the new self user. A match serves it;
//!   a mismatch crypto-erases and recreates it in the background (the
//!   Keychain may prompt), and the actor serves the new cache once it is up.
//!
//! A cache whose erase fails is not served this run; the next startup
//! retries the erase.

use std::path::{Path, PathBuf};

use tokio::task::JoinHandle;

use super::Actor;
use super::auth::AuthMsg;
use super::lifecycle::Phase;
use super::logout::{CacheParts, erase_cache, open_cache};
use crate::backend::Backend;
use crate::cache::{Binding, CacheHandle, CacheKey};
use crate::session::PersistHandle;

/// Who the startup session says the account is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    /// A stored, signed-in session with this self user id.
    Verified(i64),
    /// No usable session: no item, an unknown version, a corrupt item, or a
    /// session without a self user.
    SignedOut,
    /// The session store could not be read: the account is unknown.
    Unknown,
}

/// Whether the actor may read the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Served,
    Quarantined,
}

/// An open cache and what it may be used for.
pub(crate) struct Bound {
    handle: CacheHandle,
    key: CacheKey,
    gate: Gate,
    /// The account it was bound to (`None`: unbound).
    owner: Option<i64>,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound")
            .field("gate", &self.gate)
            .finish_non_exhaustive()
    }
}

/// A background re-binding (erase and recreate); its result is adopted when
/// it reports, or by a logout that needs the cache.
pub(crate) type Rebinding = JoinHandle<Option<Bound>>;

/// The startup cache: the served handle (if any) and the parts the actor
/// keeps for binding and erasing it.
pub(crate) struct Provisioned {
    pub(crate) served: Option<CacheHandle>,
    pub(crate) parts: CacheParts,
}

impl std::fmt::Debug for Provisioned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provisioned")
            .field("served", &self.served.is_some())
            .finish_non_exhaustive()
    }
}

/// Opens the cache in `dir` for the startup `identity` (see the module docs).
pub(crate) async fn provision_cache(
    persist: &PersistHandle,
    dir: PathBuf,
    identity: Identity,
) -> Provisioned {
    let bound = match identity {
        Identity::Verified(id) => settle(persist, &dir, None, Some(id)).await,
        Identity::SignedOut => settle(persist, &dir, None, None).await,
        Identity::Unknown => open_cache(persist, dir.clone())
            .await
            .map(|(handle, key)| Bound {
                handle,
                key,
                gate: Gate::Quarantined,
                owner: None,
            }),
    };
    let mut parts = CacheParts::closed(dir);
    let served = bound.and_then(|b| parts.keep(b));
    Provisioned { served, parts }
}

/// Binds `open` (or the cache in `dir`, opened now) to `owner`. A foreign
/// cache is crypto-erased and recreated, then bound. `None`: no usable cache
/// (it could not be opened, or its erase failed and its files remain).
async fn settle(
    persist: &PersistHandle,
    dir: &Path,
    open: Option<(CacheHandle, CacheKey)>,
    owner: Option<i64>,
) -> Option<Bound> {
    let (handle, key) = match open {
        Some(open) => open,
        None => open_cache(persist, dir.to_path_buf()).await?,
    };
    match handle.bind(owner).await {
        Ok(binding @ (Binding::Same | Binding::Claimed | Binding::Blank)) => {
            return Some(bound(handle, key, binding, owner));
        }
        Ok(Binding::Foreign) | Err(_) => {}
    }
    tracing::info!("cache belongs to another account; erasing it");
    let (erased, reopened) = erase_cache(persist, Some(handle), dir.to_path_buf(), Some(key)).await;
    if erased.is_err() {
        tracing::warn!("cache erase failed; running without a cache");
    }
    let (handle, key) = reopened?;
    let binding = handle.bind(owner).await.unwrap_or(Binding::Foreign);
    Some(bound(handle, key, binding, owner))
}

fn bound(handle: CacheHandle, key: CacheKey, binding: Binding, owner: Option<i64>) -> Bound {
    let gate = match binding {
        Binding::Same | Binding::Claimed => Gate::Served,
        Binding::Blank | Binding::Foreign => Gate::Quarantined,
    };
    Bound {
        handle,
        key,
        gate,
        owner: owner.filter(|_| gate == Gate::Served),
    }
}

impl CacheParts {
    /// Keeps `bound`'s key and, unless it is served, its handle (quarantined).
    /// Returns the handle to serve.
    pub(super) fn keep(&mut self, bound: Bound) -> Option<CacheHandle> {
        self.key = Some(bound.key);
        match bound.gate {
            Gate::Served => Some(bound.handle),
            Gate::Quarantined => {
                self.quarantined = Some(bound.handle);
                None
            }
        }
    }

    /// The open cache and its key, served or not, for binding or erasing.
    pub(super) fn take_open(
        &mut self,
        served: Option<CacheHandle>,
    ) -> Option<(CacheHandle, CacheKey)> {
        let handle = served.or_else(|| self.quarantined.take());
        match (handle, self.key.take()) {
            (Some(handle), Some(key)) => Some((handle, key)),
            _ => None,
        }
    }
}

/// Waits for a background re-binding a logout interrupted, so the logout
/// erases the cache it produced.
pub(super) async fn finish_rebinding(
    rebinding: Option<Rebinding>,
) -> Option<(CacheHandle, CacheKey)> {
    let bound = rebinding?.await.ok().flatten()?;
    Some((bound.handle, bound.key))
}

impl<B: Backend> Actor<B> {
    /// Startup was blocked on an unreadable marker, so the cache was never
    /// opened, and the session was loaded since (`marker::retry_startup`):
    /// provision the cache for that `identity` as startup would have.
    pub(super) async fn reprovision_cache(&mut self, identity: Identity) {
        let Some(parts) = self.cache_parts.as_ref() else {
            return;
        };
        debug_assert!(
            self.cache.is_none() && parts.key.is_none() && parts.quarantined.is_none(),
            "no cache is opened while startup is blocked"
        );
        let provisioned = provision_cache(&self.persist, parts.dir.clone(), identity).await;
        self.cache = provisioned.served;
        if let Some(parts) = self.cache_parts.as_mut() {
            parts.key = provisioned.parts.key;
            parts.quarantined = provisioned.parts.quarantined;
        }
    }

    /// A login completed: bind the cache to the account it signed in to.
    /// Without a self user to verify against, the cache stays quarantined.
    pub(super) async fn bind_cache(&mut self) {
        let Some(parts) = self.cache_parts.as_mut() else {
            // A cache handed in without parts (tests) is not managed here.
            return;
        };
        let served = self.cache.take();
        let Ok(Some(owner)) = self.account.self_id() else {
            if let Some(handle) = served {
                parts.quarantined = Some(handle);
            }
            return;
        };
        let open = parts.take_open(served);
        if let Some((handle, key)) = open {
            match handle.bind(Some(owner)).await {
                Ok(binding @ (Binding::Same | Binding::Claimed)) => {
                    self.cache = parts.keep(bound(handle, key, binding, Some(owner)));
                    return;
                }
                _ => return self.rebind(Some((handle, key)), owner),
            }
        }
        self.rebind(None, owner);
    }

    /// Erases and recreates the cache in the background, then reports.
    fn rebind(&mut self, open: Option<(CacheHandle, CacheKey)>, owner: i64) {
        let Some(parts) = self.cache_parts.as_mut() else {
            return;
        };
        let persist = self.persist.clone();
        let dir = parts.dir.clone();
        let tx = self.auth.sender();
        parts.rebind_gen = parts.rebind_gen.wrapping_add(1);
        let generation = parts.rebind_gen;
        parts.rebinding = Some(tokio::spawn(async move {
            let bound = settle(&persist, &dir, open, Some(owner)).await;
            // A closed channel means the actor is gone.
            let _ = tx.send(AuthMsg::CacheSettled(generation)).await;
            bound
        }));
    }

    /// A background re-binding reported: serve its cache if it is still for
    /// the signed-in account. A logout may have taken it already, and a
    /// later re-binding may be the one running now (its own report adopts
    /// it). The report is sent just before the task returns its result, so
    /// the join handle is awaited rather than polled: a report that arrives
    /// before the task has finished is never dropped (review round 15, D).
    pub(super) async fn on_cache_settled(&mut self, generation: u64) {
        let Some(parts) = self.cache_parts.as_mut() else {
            return;
        };
        if parts.rebind_gen != generation {
            return;
        }
        let Some(task) = parts.rebinding.take() else {
            return;
        };
        let Some(mut bound) = task.await.ok().flatten() else {
            return;
        };
        let current = self.account.self_id().ok().flatten();
        let tearing_down = matches!(self.life.phase, Phase::LoggingOut(_));
        if tearing_down || !self.auth.signed_in || bound.owner.is_none() || bound.owner != current {
            bound.gate = Gate::Quarantined;
        }
        if let Some(handle) = parts.keep(bound) {
            self.cache = Some(handle);
        }
    }
}

impl<B: Backend> Actor<B> {
    /// Shutdown: takes every cache core holds (served, quarantined, or
    /// still re-binding) and returns their flush and close, bounded by
    /// `CACHE_FLUSH`, so no worker thread is closing SQLCipher while the
    /// process exits. The future owns what it closes, so it can run
    /// alongside retirement. A cache handed in without parts (tests) is not
    /// core's: it is only flushed, and its owner keeps using it.
    pub(super) fn close_caches(&mut self) -> impl Future<Output = ()> + Send + 'static {
        let (flush_only, served, quarantined, rebinding) = match self.cache_parts.as_mut() {
            None => (self.cache.clone(), None, None, None),
            Some(parts) => (
                None,
                self.cache.take(),
                parts.quarantined.take(),
                parts.rebinding.take(),
            ),
        };
        let all = async move {
            if let Some(cache) = flush_only {
                let _ = cache.barrier().await;
            }
            let rebound = finish_rebinding(rebinding).await.map(|(handle, _)| handle);
            for handle in [served, quarantined, rebound].into_iter().flatten() {
                let _ = handle.close().await;
            }
        };
        async move {
            let _ = tokio::time::timeout(super::CACHE_FLUSH, all).await;
        }
    }
}
