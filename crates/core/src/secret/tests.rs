use super::*;

#[test]
fn cache_key_create_then_load_returns_same_key() {
    let store = MemoryStore::new();
    let created = load_or_create_cache_key(&store).unwrap();
    assert!(store.contains(SecretItem::CacheKey));
    let loaded = load_or_create_cache_key(&store).unwrap();
    assert_eq!(created.as_bytes(), loaded.as_bytes());
    assert_ne!(created.as_bytes(), &[0u8; 32]);
}

#[test]
fn malformed_cache_key_is_a_distinct_error_and_recreate_replaces_it() {
    let store = MemoryStore::new();
    store.set(SecretItem::CacheKey, &[7u8; 31]).unwrap();
    assert_eq!(
        load_or_create_cache_key(&store).unwrap_err(),
        CacheKeyError::Malformed
    );
    // The malformed key is left for the caller to replace after destroying the
    // cache, never silently overwritten.
    assert_eq!(store.get(SecretItem::CacheKey).unwrap().unwrap().len(), 31);
    let fresh = create_cache_key(&store).unwrap();
    let loaded = load_or_create_cache_key(&store).unwrap();
    assert_eq!(fresh.as_bytes(), loaded.as_bytes());
}

#[test]
fn cache_key_store_failure_is_not_malformed() {
    let (store, control) = FaultyStore::new(MemoryStore::new());
    control.fail(StoreOp::Get);
    assert_eq!(
        load_or_create_cache_key(&store).unwrap_err(),
        CacheKeyError::Store(StoreError::Injected)
    );
    control.heal(StoreOp::Get);
    control.fail(StoreOp::Set);
    // A key that could not be stored is never handed out.
    assert_eq!(
        load_or_create_cache_key(&store).unwrap_err(),
        CacheKeyError::Store(StoreError::Injected)
    );
}

#[test]
fn delete_cache_key_removes_it() {
    let store = MemoryStore::new();
    load_or_create_cache_key(&store).unwrap();
    delete_cache_key(&store).unwrap();
    assert!(!store.contains(SecretItem::CacheKey));
    delete_cache_key(&store).unwrap();
}

#[test]
fn debug_output_is_redacted() {
    let store = MemoryStore::new();
    store.set(SecretItem::Session, b"secret-bytes").unwrap();
    assert!(!format!("{store:?}").contains("secret"));
}

/// Deletes the test items even when an assertion fails.
#[cfg(target_os = "macos")]
struct Cleanup<'a>(&'a KeychainStore);

#[cfg(target_os = "macos")]
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.delete(SecretItem::Session);
        let _ = self.0.delete(SecretItem::CacheKey);
    }
}

#[cfg(target_os = "macos")]
fn unique_namespace() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("dev.abyss.test.{}.{nanos}", std::process::id())
}

/// Writes, reads and deletes a uniquely named item in the real login keychain
/// (the Stage 1 location) and checks its attributes. Ignored: it touches the
/// user's Keychain and may prompt. Run with
/// `cargo test -p abyss-core keychain -- --ignored`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "touches the real Keychain and may prompt"]
fn keychain_roundtrip_with_attributes() {
    let store = KeychainStore::with_namespace(&unique_namespace(), KeychainLocation::File);
    let _cleanup = Cleanup(&store);

    assert_eq!(store.get(SecretItem::Session).unwrap(), None);
    store.set(SecretItem::Session, b"first").unwrap();
    // The second set takes the errSecDuplicateItem -> update path.
    store.set(SecretItem::Session, b"second").unwrap();
    let read = store.get(SecretItem::Session).unwrap().unwrap();
    assert_eq!(read.as_slice(), b"second");
    assert_eq!(store.get(SecretItem::CacheKey).unwrap(), None);

    // The login keychain has no accessibility class and cannot sync, so it
    // reports neither attribute. It must never report a synced item.
    let attrs = store.attributes(SecretItem::Session).unwrap();
    assert_ne!(attrs.synchronizable, Some(true));

    store.delete(SecretItem::Session).unwrap();
    assert_eq!(store.get(SecretItem::Session).unwrap(), None);
    store.delete(SecretItem::Session).unwrap();
    assert!(store.attributes(SecretItem::Session).is_err());
}

/// The data protection keychain enforces the accessibility class. Unsigned
/// test binaries lack the entitlement (-34018), which is why Stage 1 uses the
/// login keychain; a signed build must store `akpu`
/// (`WhenUnlockedThisDeviceOnly`), non-synchronizable.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "touches the real Keychain and may prompt"]
fn keychain_data_protection_attributes() {
    const MISSING_ENTITLEMENT: i32 = -34018;
    let store =
        KeychainStore::with_namespace(&unique_namespace(), KeychainLocation::DataProtection);
    let _cleanup = Cleanup(&store);
    match store.set(SecretItem::CacheKey, &[1u8; 32]) {
        Err(StoreError::Os { code, .. }) if code == MISSING_ENTITLEMENT => return,
        other => other.unwrap(),
    }
    let attrs = store.attributes(SecretItem::CacheKey).unwrap();
    assert_eq!(attrs.accessible.as_deref(), Some("akpu"));
    assert_eq!(attrs.synchronizable, Some(false));
}
