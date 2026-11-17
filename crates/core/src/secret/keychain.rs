//! macOS Keychain store: generic password items through `SecItem*`.
//!
//! Attributes on every item, and why:
//! - `kSecClassGenericPassword`, service `<namespace>.session` or
//!   `<namespace>.cachekey` (`dev.abyss.*` in production), account `abyss`:
//!   one fixed item per secret, so a write replaces and a delete removes
//!   exactly that item.
//! - `kSecAttrSynchronizable = false`, set on writes **and** queries: the item
//!   never enters iCloud Keychain, and queries never match a synced item that
//!   someone else planted under our service name.
//! - `kSecAttrAccessible = WhenUnlockedThisDeviceOnly`: readable only while
//!   the Mac is unlocked, and excluded from backups and device migration. abyss
//!   never needs the secret while the screen is locked (a failed write is
//!   reported, never hidden), so `AfterFirstUnlock` would only widen exposure.
//!   `ThisDeviceOnly` also matches the cache key: a restored backup without the
//!   key holds only undecryptable files.
//! - `kSecAttrLabel`: a human-readable name in Keychain Access, so the user can
//!   see and remove what abyss stored.
//!
//! The accessibility attribute is enforced by the data protection keychain.
//! That keychain needs a `keychain-access-groups` entitlement, which ad-hoc
//! signed dev builds don't have (`errSecMissingEntitlement`), so Stage 1 uses
//! [`KeychainLocation::File`] (the login keychain, which ignores the
//! accessibility class but is never synced and is locked with the user's login
//! keychain). Signed builds (Stage 3) switch to
//! [`KeychainLocation::DataProtection`]; the attributes are already set.
//!
//! Residual copies: Security.framework copies the secret into its own `CFData`
//! buffers on write and read; those are released, not wiped. Our side holds
//! secrets only in `Zeroizing` buffers.

use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::CFTypeRef;
use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound, errSecSuccess};
use security_framework_sys::keychain_item::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate,
};
use zeroize::Zeroizing;

use super::{SecretItem, SecretStore, StoreError};

// SAFETY: these are the documented immutable `CFStringRef` constants exported
// by Security.framework (macOS 10.15+, below our deployment target). The
// dynamic linker initializes them before `main`, they are never written, and
// they point to immortal CFStrings, so reading them is always safe.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    safe static kSecClass: CFStringRef;
    safe static kSecClassGenericPassword: CFStringRef;
    safe static kSecAttrService: CFStringRef;
    safe static kSecAttrAccount: CFStringRef;
    safe static kSecAttrLabel: CFStringRef;
    safe static kSecAttrSynchronizable: CFStringRef;
    safe static kSecAttrAccessible: CFStringRef;
    safe static kSecAttrAccessibleWhenUnlockedThisDeviceOnly: CFStringRef;
    safe static kSecUseDataProtectionKeychain: CFStringRef;
    safe static kSecValueData: CFStringRef;
    safe static kSecReturnData: CFStringRef;
    safe static kSecReturnAttributes: CFStringRef;
    safe static kSecUseAuthenticationUI: CFStringRef;
    safe static kSecUseAuthenticationUIFail: CFStringRef;
}

const PRODUCTION_NAMESPACE: &str = "dev.abyss";
const ACCOUNT: &str = "abyss";

/// Which macOS keychain holds the items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeychainLocation {
    /// The user's login keychain. Works for ad-hoc signed builds (Stage 1).
    File,
    /// The data protection keychain. Requires a signed build with a
    /// `keychain-access-groups` entitlement (Stage 3).
    DataProtection,
}

/// Keychain-backed [`SecretStore`]. Every call blocks (and may show a system
/// prompt), so it must only run on the persistence worker thread.
#[derive(Debug, Clone)]
pub struct KeychainStore {
    namespace: String,
    location: KeychainLocation,
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainStore {
    /// Production items `dev.abyss.session` and `dev.abyss.cachekey` in the
    /// login keychain.
    pub fn new() -> Self {
        Self::with_namespace(PRODUCTION_NAMESPACE, KeychainLocation::File)
    }

    /// Items `<namespace>.session` and `<namespace>.cachekey`. Tests use a
    /// unique namespace so they never touch production items.
    pub fn with_namespace(namespace: &str, location: KeychainLocation) -> Self {
        Self {
            namespace: namespace.to_owned(),
            location,
        }
    }

    fn service(&self, item: SecretItem) -> String {
        format!("{}.{}", self.namespace, item.suffix())
    }

    /// The attributes that identify our item: class, service, account,
    /// non-synchronizable and, if selected, the data protection keychain.
    fn identity(&self, item: SecretItem) -> Vec<(CFString, CFType)> {
        let mut pairs = vec![
            (key(kSecClass), key(kSecClassGenericPassword).into_CFType()),
            (
                key(kSecAttrService),
                CFString::new(&self.service(item)).into_CFType(),
            ),
            (key(kSecAttrAccount), CFString::new(ACCOUNT).into_CFType()),
            (
                key(kSecAttrSynchronizable),
                CFBoolean::false_value().into_CFType(),
            ),
        ];
        if self.location == KeychainLocation::DataProtection {
            pairs.push((
                key(kSecUseDataProtectionKeychain),
                CFBoolean::true_value().into_CFType(),
            ));
        }
        pairs
    }

    /// Attributes written on add and refreshed on update.
    fn protection(item: SecretItem) -> [(CFString, CFType); 2] {
        let label = match item {
            SecretItem::Session => "abyss Telegram session",
            SecretItem::CacheKey => "abyss message cache key",
            SecretItem::ApiCredentials => "abyss Telegram API credentials",
            SecretItem::LogoutPending => "abyss logout pending",
        };
        [
            (
                key(kSecAttrAccessible),
                key(kSecAttrAccessibleWhenUnlockedThisDeviceOnly).into_CFType(),
            ),
            (key(kSecAttrLabel), CFString::new(label).into_CFType()),
        ]
    }

    fn update(&self, item: SecretItem, data: &CFData) -> Result<(), StoreError> {
        let query = dict(&self.identity(item));
        let mut changes = Self::protection(item).to_vec();
        changes.push((key(kSecValueData), data.as_CFType()));
        let changes = dict(&changes);
        // SAFETY: both arguments are valid, retained CFDictionaries that live
        // until the call returns; SecItemUpdate does not retain them past it.
        let status =
            unsafe { SecItemUpdate(query.as_concrete_TypeRef(), changes.as_concrete_TypeRef()) };
        check(status)
    }
}

impl SecretStore for KeychainStore {
    fn has_items(&self) -> Result<bool, StoreError> {
        for item in [
            SecretItem::Session,
            SecretItem::CacheKey,
            SecretItem::ApiCredentials,
            SecretItem::LogoutPending,
        ] {
            let mut query = self.identity(item);
            query.push((
                key(kSecReturnAttributes),
                CFBoolean::true_value().into_CFType(),
            ));
            query.push((
                key(kSecUseAuthenticationUI),
                key(kSecUseAuthenticationUIFail).into_CFType(),
            ));
            let query = dict(&query);
            let mut out: CFTypeRef = std::ptr::null();
            // SAFETY: query and out live for the call. Any returned value
            // follows the Create rule; it is released without reading data.
            let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &raw mut out) };
            if !out.is_null() {
                // SAFETY: a non-null result is an owned CF object.
                drop(unsafe { CFType::wrap_under_create_rule(out) });
            }
            if status == errSecItemNotFound {
                continue;
            }
            check(status)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        let mut query = self.identity(item);
        query.push((key(kSecReturnData), CFBoolean::true_value().into_CFType()));
        let query = dict(&query);
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: `query` is a valid CFDictionary and `out` a valid out-pointer;
        // on success the result follows the Create rule (we own one reference).
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &raw mut out) };
        if status == errSecItemNotFound {
            return Ok(None);
        }
        check(status)?;
        if out.is_null() {
            return Err(StoreError::Malformed);
        }
        // SAFETY: `out` is non-null and owned by us (Create rule); wrapping it
        // transfers that reference to `CFType`, which releases it on drop.
        let value = unsafe { CFType::wrap_under_create_rule(out) };
        let data = value
            .downcast_into::<CFData>()
            .ok_or(StoreError::Malformed)?;
        let bytes = data.bytes();
        let mut secret = Zeroizing::new(Vec::with_capacity(bytes.len()));
        secret.extend_from_slice(bytes);
        Ok(Some(secret))
    }

    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError> {
        let data = CFData::from_buffer(secret);
        let mut attrs = self.identity(item);
        attrs.extend(Self::protection(item));
        attrs.push((key(kSecValueData), data.as_CFType()));
        let attrs = dict(&attrs);
        // SAFETY: `attrs` is a valid CFDictionary; a null result pointer is
        // allowed and means "don't return the added item".
        let status = unsafe { SecItemAdd(attrs.as_concrete_TypeRef(), std::ptr::null_mut()) };
        if status == errSecDuplicateItem {
            // Update in place: a delete-then-add would lose the session if the
            // process died in between.
            return self.update(item, &data);
        }
        check(status)
    }

    fn delete(&self, item: SecretItem) -> Result<(), StoreError> {
        let query = dict(&self.identity(item));
        // SAFETY: `query` is a valid CFDictionary for the duration of the call.
        let status = unsafe { SecItemDelete(query.as_concrete_TypeRef()) };
        if status == errSecItemNotFound {
            return Ok(());
        }
        check(status)
    }
}

/// Wraps a Security.framework `kSec*` constant.
fn key(raw: CFStringRef) -> CFString {
    // SAFETY: `raw` is one of the immortal constants declared above (the only
    // values passed here); the Get rule retains it, which is always valid.
    unsafe { CFString::wrap_under_get_rule(raw) }
}

fn dict(
    pairs: &[(CFString, CFType)],
) -> core_foundation::dictionary::CFDictionary<CFString, CFType> {
    core_foundation::dictionary::CFDictionary::from_CFType_pairs(pairs)
}

fn check(status: i32) -> Result<(), StoreError> {
    if status == errSecSuccess {
        return Ok(());
    }
    let message = security_framework::base::Error::from_code(status)
        .message()
        .unwrap_or_default();
    Err(StoreError::Os {
        code: status,
        message,
    })
}

/// Accessibility class and synchronizable flag of a stored item, for the
/// ignored real-Keychain test.
#[cfg(test)]
pub(super) struct ItemAttributes {
    pub accessible: Option<String>,
    pub synchronizable: Option<bool>,
}

#[cfg(test)]
impl KeychainStore {
    pub(super) fn attributes(&self, item: SecretItem) -> Result<ItemAttributes, StoreError> {
        use core_foundation::dictionary::CFDictionary;
        use std::ffi::c_void;

        let mut query = self.identity(item);
        query.push((
            key(kSecReturnAttributes),
            CFBoolean::true_value().into_CFType(),
        ));
        let query = dict(&query);
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: as in `get`.
        check(unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &raw mut out) })?;
        if out.is_null() {
            return Err(StoreError::Malformed);
        }
        // SAFETY: as in `get`.
        let value = unsafe { CFType::wrap_under_create_rule(out) };
        let attrs = value
            .downcast_into::<CFDictionary<*const c_void, *const c_void>>()
            .ok_or(StoreError::Malformed)?;
        let find = |k: CFStringRef| -> Option<CFType> {
            let v = attrs.find(key(k).as_CFTypeRef())?;
            // SAFETY: values of a live CFDictionary are valid CF objects; the
            // Get rule retains this one.
            Some(unsafe { CFType::wrap_under_get_rule(*v) })
        };
        Ok(ItemAttributes {
            accessible: find(kSecAttrAccessible)
                .and_then(CFType::downcast_into::<CFString>)
                .map(|s| s.to_string()),
            synchronizable: find(kSecAttrSynchronizable)
                .and_then(CFType::downcast_into::<CFBoolean>)
                .map(bool::from),
        })
    }
}
