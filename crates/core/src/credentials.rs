//! API credential provenance (plan sections 17.1 and 17.2a).
//!
//! Resolution order, atomic per source: the env pair (a dev override, never
//! persisted), then saved credentials (owned by the persistence worker),
//! then a pair embedded at build time, else onboarding.
//! A source that is present but unusable stops the resolution with an error;
//! it never falls through to the next source.
//!
//! [`CredentialSlot`] is how core hands the pair in use to the backend: a
//! build waits until the slot holds a pair, so with no credentials no client
//! is built. Each pair set gets a version, and the slot remembers the version
//! the last build took, so core can tell an instance built for superseded
//! credentials.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::api::{ApiCredentials, ApiHash, ApiId, EnvVar};

/// What the environment says about credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvResolution {
    /// Neither variable is set (or both are empty).
    Absent,
    Pair(ApiCredentials),
    /// One variable is set, the other is not.
    Partial {
        missing: EnvVar,
    },
    /// A variable is set but malformed (the first one found).
    Malformed {
        var: EnvVar,
    },
}

/// Reads the env pair through `lookup` (the process environment in
/// production). Empty values count as unset.
pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> EnvResolution {
    let get = |var: EnvVar| lookup(var.name()).filter(|v| !v.trim().is_empty());
    match (get(EnvVar::ApiId), get(EnvVar::ApiHash)) {
        (None, None) => EnvResolution::Absent,
        (Some(_), None) => EnvResolution::Partial {
            missing: EnvVar::ApiHash,
        },
        (None, Some(_)) => EnvResolution::Partial {
            missing: EnvVar::ApiId,
        },
        (Some(id), Some(hash)) => {
            let hash = Zeroizing::new(hash);
            let Ok(api_id) = ApiId::parse(&id) else {
                return EnvResolution::Malformed { var: EnvVar::ApiId };
            };
            let Ok(api_hash) = ApiHash::parse(&hash) else {
                return EnvResolution::Malformed {
                    var: EnvVar::ApiHash,
                };
            };
            EnvResolution::Pair(ApiCredentials { api_id, api_hash })
        }
    }
}

/// A pair compiled in with `ABYSS_EMBEDDED_API_ID` / `ABYSS_EMBEDDED_API_HASH`
/// (off by default; not part of Stage 1 releases). A malformed embedded pair
/// counts as absent.
pub fn embedded() -> Option<ApiCredentials> {
    let id = option_env!("ABYSS_EMBEDDED_API_ID")?;
    let hash = option_env!("ABYSS_EMBEDDED_API_HASH")?;
    Some(ApiCredentials {
        api_id: ApiId::parse(id).ok()?,
        api_hash: ApiHash::parse(hash).ok()?,
    })
}

/// Stored format: a version byte, the id (little endian), the 32 hex digits.
const STORED_VERSION: u8 = 1;
const STORED_LEN: usize = 1 + 4 + ApiHash::LEN;

/// The Keychain item's bytes for `creds`.
pub(crate) fn encode(creds: &ApiCredentials) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(STORED_LEN));
    out.push(STORED_VERSION);
    out.extend_from_slice(&creds.api_id.get().to_le_bytes());
    out.extend_from_slice(creds.api_hash.expose().as_bytes());
    out
}

/// Parses a stored item; `None` for anything malformed.
pub(crate) fn decode(bytes: &[u8]) -> Option<ApiCredentials> {
    if bytes.len() != STORED_LEN {
        return None;
    }
    let (version, rest) = bytes.split_first()?;
    if *version != STORED_VERSION {
        return None;
    }
    let (id, hash) = rest.split_at_checked(4)?;
    let id = i32::from_le_bytes(id.try_into().ok()?);
    let hash = Zeroizing::new(std::str::from_utf8(hash).ok()?.to_owned());
    Some(ApiCredentials {
        api_id: ApiId::new(id).ok()?,
        api_hash: ApiHash::parse(&hash).ok()?,
    })
}

/// The pair in use, shared by core and the backend.
#[derive(Clone)]
pub struct CredentialSlot {
    inner: Arc<SlotInner>,
}

struct SlotInner {
    tx: watch::Sender<(u64, Option<Arc<ApiCredentials>>)>,
    /// The version the last build took.
    taken: AtomicU64,
    env: EnvResolution,
}

impl std::fmt::Debug for CredentialSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSlot")
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

impl CredentialSlot {
    /// An empty slot. `env` is what core resolves first.
    pub fn new(env: EnvResolution) -> Self {
        Self {
            inner: Arc::new(SlotInner {
                tx: watch::Sender::new((0, None)),
                taken: AtomicU64::new(0),
                env,
            }),
        }
    }

    pub fn env(&self) -> &EnvResolution {
        &self.inner.env
    }

    /// Replaces the pair (or clears it); returns the new version.
    pub fn set(&self, creds: Option<ApiCredentials>) -> u64 {
        let mut version = 0;
        self.inner.tx.send_modify(|(v, slot)| {
            *v = v.saturating_add(1);
            version = *v;
            *slot = creds.map(Arc::new);
        });
        version
    }

    pub fn current(&self) -> Option<Arc<ApiCredentials>> {
        self.inner.tx.borrow().1.clone()
    }

    pub fn version(&self) -> u64 {
        self.inner.tx.borrow().0
    }

    /// The version the last build took.
    pub fn taken(&self) -> u64 {
        self.inner.taken.load(Ordering::Acquire)
    }

    /// Waits until a pair is set, then takes it for a build. Cancel-safe.
    pub async fn wait(&self) -> Arc<ApiCredentials> {
        let mut rx = self.inner.tx.subscribe();
        loop {
            {
                let (version, creds) = &*rx.borrow_and_update();
                if let Some(creds) = creds {
                    self.inner.taken.store(*version, Ordering::Release);
                    return Arc::clone(creds);
                }
            }
            // The sender lives as long as `self`, so this cannot fail; if it
            // ever did, waiting forever is the safe answer (no build).
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789ABCDEF";

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn env_pair_partial_and_malformed() {
        assert_eq!(from_env(env(&[])), EnvResolution::Absent);
        assert_eq!(
            from_env(env(&[("ABYSS_API_ID", "1"), ("ABYSS_API_HASH", " ")])),
            EnvResolution::Partial {
                missing: EnvVar::ApiHash
            }
        );
        assert_eq!(
            from_env(env(&[("ABYSS_API_HASH", HASH)])),
            EnvResolution::Partial {
                missing: EnvVar::ApiId
            }
        );
        assert_eq!(
            from_env(env(&[("ABYSS_API_ID", "x"), ("ABYSS_API_HASH", HASH)])),
            EnvResolution::Malformed { var: EnvVar::ApiId }
        );
        assert_eq!(
            from_env(env(&[("ABYSS_API_ID", "7"), ("ABYSS_API_HASH", "zz")])),
            EnvResolution::Malformed {
                var: EnvVar::ApiHash
            }
        );
        let EnvResolution::Pair(c) =
            from_env(env(&[("ABYSS_API_ID", " 42 "), ("ABYSS_API_HASH", HASH)]))
        else {
            panic!("expected a pair");
        };
        assert_eq!(c.api_id.get(), 42);
        assert_eq!(c.api_hash.expose(), HASH.to_lowercase());
    }

    #[test]
    fn stored_round_trip_and_rejects() {
        let creds = ApiCredentials {
            api_id: ApiId::new(99).unwrap(),
            api_hash: ApiHash::parse(HASH).unwrap(),
        };
        let bytes = encode(&creds);
        assert_eq!(bytes.len(), STORED_LEN);
        assert_eq!(decode(&bytes), Some(creds));
        assert_eq!(decode(&bytes[..10]), None);
        let mut wrong = bytes.to_vec();
        wrong[0] = 2;
        assert_eq!(decode(&wrong), None);
        let mut zero = bytes.to_vec();
        zero[1..5].copy_from_slice(&0i32.to_le_bytes());
        assert_eq!(decode(&zero), None);
    }

    #[tokio::test]
    async fn slot_versions_and_waits() {
        let slot = CredentialSlot::new(EnvResolution::Absent);
        let waiter = tokio::spawn({
            let slot = slot.clone();
            async move { slot.wait().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        let creds = ApiCredentials {
            api_id: ApiId::new(5).unwrap(),
            api_hash: ApiHash::parse(HASH).unwrap(),
        };
        let v = slot.set(Some(creds.clone()));
        assert_eq!(*waiter.await.unwrap(), creds);
        assert_eq!(slot.taken(), v);
        let v2 = slot.set(None);
        assert!(v2 > slot.taken());
        assert!(slot.current().is_none());
    }
}
