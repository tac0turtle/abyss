//! Sandbox, Application Support root, keychain and `NSWorkspace` checks.

use std::ffi::{CStr, c_char, c_int};

use abyss_core::secret::{KeychainLocation, KeychainStore, SecretItem, SecretStore};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{
    NSBundle, NSFileManager, NSHomeDirectory, NSSearchPathDirectory, NSSearchPathDomainMask,
    NSString, NSTemporaryDirectory, NSURL,
};

use crate::say;

unsafe extern "C" {
    // libsandbox: non-zero when the process is sandboxed (filter type 0, no operation).
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter_type: c_int, ...) -> c_int;
}

pub(crate) fn run(namespace: &str) {
    say(&format!("== sandbox spike, pid {}", std::process::id()));
    environment();
    let support = application_support();
    write_checks(support.as_deref());
    tcp_check(None);
    keychain(namespace);
    open_url();
}

fn environment() {
    for name in [
        "HOME",
        "CFFIXED_USER_HOME",
        "APP_SANDBOX_CONTAINER_ID",
        "TMPDIR",
    ] {
        say(&format!("env {name} = {:?}", std::env::var(name).ok()));
    }
    // SAFETY: a null operation with filter type 0 is the documented "am I sandboxed" form.
    let sandboxed = unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) };
    say(&format!(
        "sandbox_check = {sandboxed} (non-zero means sandboxed)"
    ));
    let id = NSBundle::mainBundle().bundleIdentifier();
    say(&format!(
        "bundle identifier = {:?}",
        id.map(|s| s.to_string())
    ));
    say(&format!("NSHomeDirectory() = {}", NSHomeDirectory()));
    say(&format!(
        "NSTemporaryDirectory() = {}",
        NSTemporaryDirectory()
    ));
    say(&format!("getpwuid_r pw_dir = {:?}", passwd_home()));
}

/// The home directory the app's `storage.rs` uses today: the user database, not `$HOME`.
fn passwd_home() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: pwd, buf and out are valid for the call; buf's length is passed.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &raw mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut out,
        )
    };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: on success pw_dir points into buf, a NUL-terminated string.
    Some(
        unsafe { CStr::from_ptr(pwd.pw_dir) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// `NSFileManager URLsForDirectory:NSApplicationSupportDirectory inDomains:NSUserDomainMask`.
fn application_support() -> Option<String> {
    let fm = NSFileManager::defaultManager();
    let urls = fm.URLsForDirectory_inDomains(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        NSSearchPathDomainMask::UserDomainMask,
    );
    let path = urls
        .firstObject()
        .and_then(|u| u.path())
        .map(|p| p.to_string());
    say(&format!("URLsForDirectory(ApplicationSupport) = {path:?}"));
    path
}

/// Create, write, read and remove a file under a directory; report each step.
fn probe_dir(label: &str, dir: &str) {
    let path = std::path::Path::new(dir).join("dev.abyss.spike-probe");
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&path)?;
        let file = path.join("probe.txt");
        std::fs::write(&file, b"probe")?;
        let back = std::fs::read(&file)?;
        assert_eq!(back, b"probe");
        std::fs::remove_file(&file)?;
        std::fs::remove_dir(&path)
    })();
    say(&format!("write probe under {label}: {result:?}"));
}

fn write_checks(support: Option<&str>) {
    if let Some(dir) = support {
        probe_dir("URLsForDirectory root", dir);
    }
    if let Some(home) = passwd_home() {
        probe_dir(
            "getpwuid_r Library/Application Support",
            &format!("{home}/Library/Application Support"),
        );
    }
}

/// A TCP handshake to a Telegram data centre (DC2, the address abyss-core
/// also uses), with no payload: does `network.client` let MTProto out?
pub(crate) fn tcp_check(target: Option<&str>) {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = target
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([149, 154, 167, 50], 443)));
    say(&format!("TCP connect to {addr} ..."));
    let result = TcpStream::connect(addr);
    say(&format!(
        "TCP connect to {addr}: {:?}",
        result.map(|_| "connected")
    ));
}

fn keychain(namespace: &str) {
    let store = KeychainStore::with_namespace(namespace, KeychainLocation::File);
    let item = SecretItem::Session;
    say(&format!("keychain namespace {namespace}, location File"));
    say(&format!("  precondition delete: {:?}", store.delete(item)));
    say(&format!(
        "  get (absent): {:?}",
        store.get(item).map(|v| v.map(|b| b.len()))
    ));
    say(&format!(
        "  add (set #1): {:?}",
        store.set(item, b"spike-secret-one")
    ));
    let first = store
        .get(item)
        .map(|v| v.map(|b| b.to_vec() == b"spike-secret-one"));
    say(&format!("  read back matches #1: {first:?}"));
    say(&format!(
        "  update (set #2): {:?}",
        store.set(item, b"spike-secret-two-longer")
    ));
    let second = store
        .get(item)
        .map(|v| v.map(|b| b.to_vec() == b"spike-secret-two-longer"));
    say(&format!("  read back matches #2: {second:?}"));
    say(&format!("  has_items: {:?}", store.has_items()));
    say(&format!("  delete: {:?}", store.delete(item)));
    say(&format!(
        "  get after delete: {:?}",
        store.get(item).map(|v| v.map(|b| b.len()))
    ));
}

fn open_url() {
    let url = NSURL::URLWithString(&NSString::from_str("https://my.telegram.org/apps"));
    let ok = url.is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url));
    say(&format!(
        "NSWorkspace openURL(https://my.telegram.org/apps) returned {ok}"
    ));
}

/// Leaves an item behind, so a process outside the sandbox can look for it.
pub(crate) fn keychain_set(namespace: &str) {
    let store = KeychainStore::with_namespace(namespace, KeychainLocation::File);
    say(&format!(
        "kc-set {namespace}: {:?}",
        store.set(SecretItem::Session, b"spike-visible")
    ));
}

pub(crate) fn keychain_delete(namespace: &str) {
    let store = KeychainStore::with_namespace(namespace, KeychainLocation::File);
    say(&format!(
        "kc-delete {namespace}: {:?}",
        store.delete(SecretItem::Session)
    ));
}
