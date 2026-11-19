//! Stage 3 task 0 spike: sandbox, keychain, `NSWorkspace` and notifications.
//!
//!   abyss-spike sandbox [namespace]   Application Support root, writes, keychain, openURL
//!   abyss-spike notify [wait-seconds] UNUserNotificationCenter authorization and a banner
//!
//! Every line goes to stdout and to `<NSTemporaryDirectory>/abyss-spike.log`,
//! because a bundle started with `open` has no terminal. Throwaway code: it
//! prints a lot, so lints about output are off.
#![allow(clippy::print_stdout, reason = "a spike reports on stdout")]

mod notify;
mod sandbox;

use std::io::Write as _;
use std::sync::OnceLock;

use objc2_foundation::NSTemporaryDirectory;

static LOG_PATH: OnceLock<String> = OnceLock::new();

/// Print a line and append it to the log file next to the process's temp dir.
pub(crate) fn say(line: &str) {
    println!("{line}");
    let path = LOG_PATH.get_or_init(|| format!("{}abyss-spike.log", NSTemporaryDirectory()));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sandbox") => sandbox::run(args.get(2).map_or("dev.abyss.spike", String::as_str)),
        Some("tcp") => sandbox::tcp_check(args.get(2).map(String::as_str)),
        Some("kc-set") => {
            sandbox::keychain_set(args.get(2).map_or("dev.abyss.spike", String::as_str))
        }
        Some("kc-delete") => {
            sandbox::keychain_delete(args.get(2).map_or("dev.abyss.spike", String::as_str))
        }
        Some("notify") => notify::run(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60)),
        _ => say("usage: abyss-spike sandbox|kc-set|kc-delete [namespace] | notify [wait-seconds]"),
    }
}
