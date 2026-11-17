use super::*;
use crate::secret::{FaultyStore, MemoryStore, StoreOp};

#[derive(Clone, Copy, PartialEq)]
enum Protection {
    Local,
    Keychain,
}

fn store<S: SecretStore>(dir: &Path, keychain: S, protection: Protection) -> SwitchingStore<S> {
    SwitchingStore::new(
        SecretStorageConfig {
            dir: dir.to_owned(),
            use_keychain: protection == Protection::Keychain,
        },
        keychain,
    )
}

fn seed(keychain: &MemoryStore) {
    for item in ITEMS {
        keychain.set(item, item.suffix().as_bytes()).unwrap();
    }
}

#[test]
fn legacy_import_preserves_every_item_and_local_restarts_never_access_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = MemoryStore::new();
    seed(&keychain);
    let local = store(dir.path(), keychain.clone(), Protection::Local);
    for item in ITEMS {
        assert_eq!(
            local.get(item).unwrap().unwrap().as_slice(),
            item.suffix().as_bytes()
        );
        assert!(!keychain.contains(item));
    }
    drop(local);
    let (keychain, control) = FaultyStore::new(keychain);
    control.fail(StoreOp::Get);
    control.fail(StoreOp::Set);
    control.fail(StoreOp::Delete);
    let local = store(dir.path(), keychain, Protection::Local);
    for item in ITEMS {
        assert_eq!(
            local.get(item).unwrap().unwrap().as_slice(),
            item.suffix().as_bytes()
        );
        local.delete(item).unwrap();
        local.delete(item).unwrap();
    }
    assert_eq!(control.calls(), 0);
    local.set(SecretItem::Session, b"new session").unwrap();
    assert_eq!(
        local.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"new session"
    );
}

#[test]
fn switching_both_ways_preserves_session_and_key_and_removes_local_copies() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = MemoryStore::new();
    let local = store(dir.path(), keychain.clone(), Protection::Local);
    local
        .set(SecretItem::Session, b"sensitive session")
        .unwrap();
    local.set(SecretItem::CacheKey, &[17; 32]).unwrap();
    let abandoned = dir.path().join(format!(".secrets-{}", "a".repeat(32)));
    fs::write(&abandoned, b"abandoned plaintext").unwrap();
    drop(local);
    let protected = store(dir.path(), keychain.clone(), Protection::Keychain);
    assert_eq!(
        protected
            .get(SecretItem::Session)
            .unwrap()
            .unwrap()
            .as_slice(),
        b"sensitive session"
    );
    assert_eq!(
        protected
            .get(SecretItem::CacheKey)
            .unwrap()
            .unwrap()
            .as_slice(),
        &[17; 32]
    );
    assert!(!abandoned.exists());
    let bytes = read_private(&dir.path().join(FILE)).unwrap().unwrap();
    let state = decode(&bytes).unwrap();
    assert!(state.keychain);
    assert!(state.values.iter().all(Option::is_none));
    assert!(
        !bytes
            .windows(b"sensitive session".len())
            .any(|w| w == b"sensitive session")
    );
    drop(protected);
    let local = store(dir.path(), keychain.clone(), Protection::Local);
    assert_eq!(
        local.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"sensitive session"
    );
    assert!(!keychain.contains(SecretItem::Session));
    assert!(!keychain.contains(SecretItem::CacheKey));
}

#[test]
fn failed_destination_write_keeps_the_local_source_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = MemoryStore::new();
    let local = store(dir.path(), keychain.clone(), Protection::Local);
    local.set(SecretItem::Session, b"session").unwrap();
    local.set(SecretItem::CacheKey, &[9; 32]).unwrap();
    drop(local);
    let (keychain, control) = FaultyStore::new(keychain);
    control.fail_set_of(SecretItem::CacheKey);
    let protected = store(dir.path(), keychain, Protection::Keychain);
    assert_eq!(
        protected.get(SecretItem::Session),
        Err(StoreError::Injected)
    );
    let bytes = read_private(&dir.path().join(FILE)).unwrap().unwrap();
    let state = decode(&bytes).unwrap();
    assert!(!state.keychain);
    assert_eq!(state.values[0].as_deref(), Some(b"session".as_slice()));
    control.heal_item(SecretItem::CacheKey);
    assert_eq!(
        protected
            .get(SecretItem::CacheKey)
            .unwrap()
            .unwrap()
            .as_slice(),
        &[9; 32]
    );
}

#[test]
fn failed_cleanup_blocks_then_resumes_from_the_committed_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let original = MemoryStore::new();
    seed(&original);
    let (keychain, control) = FaultyStore::new(original.clone());
    control.fail_delete_of(SecretItem::CacheKey);
    let local = store(dir.path(), keychain, Protection::Local);
    assert_eq!(local.get(SecretItem::Session), Err(StoreError::Injected));
    assert!(
        !original.contains(SecretItem::Session),
        "cleanup already deleted this item"
    );
    drop(local);
    let local = store(dir.path(), original.clone(), Protection::Local);
    for item in ITEMS {
        assert_eq!(
            local.get(item).unwrap().unwrap().as_slice(),
            item.suffix().as_bytes()
        );
        assert!(!original.contains(item));
    }
}

#[test]
fn unreadable_legacy_item_does_not_commit_a_partial_import() {
    let dir = tempfile::tempdir().unwrap();
    let original = MemoryStore::new();
    seed(&original);
    let (keychain, control) = FaultyStore::new(original.clone());
    control.fail_get_of(SecretItem::LogoutPending);
    let local = store(dir.path(), keychain, Protection::Local);
    assert_eq!(local.get(SecretItem::Session), Err(StoreError::Injected));
    assert!(!dir.path().join(FILE).exists());
    for item in ITEMS {
        assert!(original.contains(item));
    }
}

#[test]
fn deleted_local_session_does_not_fall_back_to_a_stale_keychain_item() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = MemoryStore::new();
    let local = store(dir.path(), keychain.clone(), Protection::Local);
    local.set(SecretItem::Session, b"session").unwrap();
    local.delete(SecretItem::Session).unwrap();
    drop(local);
    keychain.set(SecretItem::Session, b"stale").unwrap();
    let local = store(dir.path(), keychain, Protection::Local);
    assert_eq!(local.get(SecretItem::Session).unwrap(), None);
}

#[test]
fn malformed_or_oversized_manifest_never_falls_back_to_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = MemoryStore::new();
    seed(&keychain);
    for bytes in [b"damaged".to_vec(), vec![0; 65 * 1024]] {
        fs::write(dir.path().join(FILE), bytes).unwrap();
        let local = store(dir.path(), keychain.clone(), Protection::Local);
        assert!(local.get(SecretItem::Session).is_err());
        assert!(keychain.contains(SecretItem::Session));
    }
}

#[test]
fn files_are_private_and_symlinks_are_refused() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let local = store(dir.path(), MemoryStore::new(), Protection::Local);
    local.set(SecretItem::Session, b"private").unwrap();
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.path().join(FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(local);
    let victim = dir.path().join("victim");
    fs::rename(dir.path().join(FILE), &victim).unwrap();
    symlink(&victim, dir.path().join(FILE)).unwrap();
    let local = store(dir.path(), MemoryStore::new(), Protection::Local);
    assert!(local.get(SecretItem::Session).is_err());
    assert!(
        fs::read(&victim)
            .unwrap()
            .windows(7)
            .any(|w| w == b"private")
    );
}

#[test]
fn a_second_process_owner_is_refused_and_debug_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let first = store(dir.path(), MemoryStore::new(), Protection::Local);
    first.set(SecretItem::Session, b"secret-bytes").unwrap();
    let second = store(dir.path(), MemoryStore::new(), Protection::Local);
    assert!(second.get(SecretItem::Session).is_err());
    assert!(!format!("{first:?}").contains("secret-bytes"));
    drop(first);
    assert_eq!(
        second.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"secret-bytes"
    );
}

#[test]
fn cancelling_a_failed_keychain_switch_cleans_up_partial_copies() {
    let dir = tempfile::tempdir().unwrap();
    let original = MemoryStore::new();
    let local = store(dir.path(), original.clone(), Protection::Local);
    local.set(SecretItem::Session, b"session").unwrap();
    local.set(SecretItem::CacheKey, &[9; 32]).unwrap();
    drop(local);
    let (keychain, control) = FaultyStore::new(original.clone());
    control.fail_set_of(SecretItem::CacheKey);
    let protected = store(dir.path(), keychain, Protection::Keychain);
    assert_eq!(
        protected.get(SecretItem::Session),
        Err(StoreError::Injected)
    );
    assert!(
        original.contains(SecretItem::Session),
        "a partial destination exists"
    );
    drop(protected);
    let local = store(dir.path(), original.clone(), Protection::Local);
    assert_eq!(
        local.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"session"
    );
    assert!(!original.contains(SecretItem::Session));
    local.delete(SecretItem::Session).unwrap();
}

#[test]
fn failed_local_write_preserves_the_previous_value_on_disk_and_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let local = store(dir.path(), MemoryStore::new(), Protection::Local);
    local.set(SecretItem::Session, b"saved").unwrap();
    assert_eq!(
        local.set(SecretItem::Session, &vec![7; 64 * 1024]),
        Err(StoreError::Malformed)
    );
    assert_eq!(
        local.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"saved"
    );
}

#[test]
fn a_fresh_local_install_never_requests_keychain_secret_access() {
    struct EmptyKeychain;
    impl SecretStore for EmptyKeychain {
        fn has_items(&self) -> Result<bool, StoreError> {
            Ok(false)
        }
        fn get(&self, _: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
            Err(StoreError::Injected)
        }
        fn set(&self, _: SecretItem, _: &[u8]) -> Result<(), StoreError> {
            Err(StoreError::Injected)
        }
        fn delete(&self, _: SecretItem) -> Result<(), StoreError> {
            Err(StoreError::Injected)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let local = store(dir.path(), EmptyKeychain, Protection::Local);
    assert_eq!(local.get(SecretItem::Session).unwrap(), None);
    local.set(SecretItem::Session, b"session").unwrap();
    assert_eq!(
        local.get(SecretItem::Session).unwrap().unwrap().as_slice(),
        b"session"
    );
    local.delete(SecretItem::Session).unwrap();
}
