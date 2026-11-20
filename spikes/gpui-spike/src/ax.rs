//! An in-process accessibility client: walks the window's NSAccessibility tree
//! with the same selectors VoiceOver uses, via the AccessKit adapter GPUI installs.

use gpui::{App, Window};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSRange, NSString};

use crate::log;

fn content_view() -> Option<Retained<AnyObject>> {
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let window = app.windows().firstObject()?;
    window.contentView().map(|v| {
        // SAFETY: an NSView is an NSObject; treat it dynamically.
        unsafe { Retained::cast_unchecked::<AnyObject>(v) }
    })
}

fn string(obj: &AnyObject, selector: objc2::runtime::Sel) -> Option<String> {
    // SAFETY: the NSAccessibility getters return an NSString or nil.
    let s: Option<Retained<NSString>> = unsafe { msg_send![obj, performSelector: selector] };
    s.map(|s| s.to_string())
}

fn responds(obj: &AnyObject, selector: objc2::runtime::Sel) -> bool {
    // SAFETY: respondsToSelector: takes a selector and returns BOOL.
    unsafe { msg_send![obj, respondsToSelector: selector] }
}

fn children(obj: &AnyObject) -> Vec<Retained<AnyObject>> {
    if !responds(obj, sel!(accessibilityChildren)) {
        return Vec::new();
    }
    // SAFETY: accessibilityChildren returns an NSArray of objects or nil.
    let array: Option<Retained<NSArray<AnyObject>>> =
        unsafe { msg_send![obj, accessibilityChildren] };
    array.map(|a| a.iter().collect()).unwrap_or_default()
}

/// First query: wakes the adapter (an AX client's first call does the same).
pub(crate) fn poke() {
    let Some(view) = content_view() else {
        log("ax poke: no content view");
        return;
    };
    log(&format!(
        "ax poke: contentView has {} accessibility children",
        children(&view).len()
    ));
}

pub(crate) fn dump(window: &mut Window, _cx: &mut App) {
    let Some(view) = content_view() else {
        log("ax dump: no content view");
        return;
    };
    log("ax dump: NSAccessibility tree (depth role | title | label | value)");
    walk(&view, 0);
    match window.debug_a11y_tree_json() {
        Some(json) => {
            if let Ok(dir) = std::env::var("SPIKE_CMD_DIR") {
                let _ = std::fs::write(std::path::Path::new(&dir).join("a11y-tree.json"), &json);
            }
            log(&format!(
                "debug_a11y_tree_json: Some({} bytes), saved to a11y-tree.json",
                json.len()
            ));
        }
        None => log("debug_a11y_tree_json: None"),
    }
}

fn walk(obj: &AnyObject, depth: usize) {
    if depth > 8 {
        return;
    }
    let role = string(obj, sel!(accessibilityRole));
    let title = string(obj, sel!(accessibilityTitle));
    let label = string(obj, sel!(accessibilityLabel));
    let subrole = string(obj, sel!(accessibilitySubrole));
    // SAFETY: accessibilityValue returns an object or nil; Debug prints its description.
    let value: Option<Retained<AnyObject>> = unsafe { msg_send![obj, accessibilityValue] };
    let value = value.map(|v| {
        // SAFETY: every NSObject answers description.
        let d: Option<Retained<NSString>> = unsafe { msg_send![&*v, description] };
        d.map(|d| d.to_string())
    });
    let indent = "  ".repeat(depth);
    log(&format!(
        "{indent}{depth} {role:?}/{subrole:?} | title {title:?} | label {label:?} | value {value:?}"
    ));
    if responds(obj, sel!(accessibilitySelectedTextRange)) && role.as_deref() == Some("AXTextField")
    {
        text_attributes(obj, &indent);
    }
    for child in children(obj) {
        walk(&child, depth + 1);
    }
}

fn text_attributes(obj: &AnyObject, indent: &str) {
    // SAFETY: documented NSAccessibility text attributes; nil or zero when unsupported.
    let (count, range, selected): (isize, NSRange, Option<Retained<NSString>>) = unsafe {
        (
            msg_send![obj, accessibilityNumberOfCharacters],
            msg_send![obj, accessibilitySelectedTextRange],
            msg_send![obj, accessibilitySelectedText],
        )
    };
    log(&format!(
        "{indent}  text: numberOfCharacters={count} selectedTextRange=({}, {}) selectedText={:?}",
        range.location,
        range.length,
        selected.map(|s| s.to_string())
    ));
}

/// What VoiceOver's "press" does: `accessibilityPerformPress` on the first AXButton.
pub(crate) fn press_button() {
    fn find(obj: &AnyObject, depth: usize) -> Option<Retained<AnyObject>> {
        if depth > 8 {
            return None;
        }
        for child in children(obj) {
            if string(&child, sel!(accessibilityRole)).as_deref() == Some("AXButton") {
                return Some(child);
            }
            if let Some(found) = find(&child, depth + 1) {
                return Some(found);
            }
        }
        None
    }
    let Some(view) = content_view() else { return };
    match find(&view, 0) {
        Some(button) => {
            // SAFETY: accessibilityPerformPress returns BOOL.
            let ok: bool = unsafe { msg_send![&*button, accessibilityPerformPress] };
            log(&format!(
                "accessibilityPerformPress on the AXButton returned {ok}"
            ));
        }
        None => log("press: no AXButton found"),
    }
}
