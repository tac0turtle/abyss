//! Census admission tests.

use std::path::Path;
use std::time::Duration;

use super::{Open, Refused, close_admission, wait_closed};
use crate::cache::{Cache, CacheError, CacheKey};

fn census_empty(dir: &Path) -> bool {
    wait_closed(Some(dir), Duration::ZERO)
}

/// Core shutdown closes admission to its directory: entries already in are
/// kept (and drained), new ones are refused, and the directory reopens once
/// every closer is gone.
#[test]
fn closed_admission_refuses_new_entries_until_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let held = Open::enter(dir.path()).unwrap();
    let first = close_admission(dir.path());
    let second = close_admission(dir.path());
    assert_eq!(Open::enter(dir.path()).err(), Some(Refused::Closing));
    let key = CacheKey::generate().unwrap();
    assert!(matches!(
        Cache::open(dir.path(), &key),
        Err(CacheError::Refused(Refused::Closing))
    ));
    drop(Open::enter(other.path()).unwrap());
    assert!(!census_empty(dir.path()), "the admitted entry stays");
    drop(held);
    assert!(census_empty(dir.path()));
    drop(first);
    assert!(
        Open::enter(dir.path()).is_err(),
        "still closed by the second"
    );
    drop(second);
    drop(Open::enter(dir.path()).unwrap());
    assert!(census_empty(dir.path()));
}
