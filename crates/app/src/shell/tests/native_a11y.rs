#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this harnessless target runs assertions in AppKit's main thread without libtest"
)]

use std::time::{Duration, SystemTime};

use abyss_core::api::{
    AuthState, CodeDelivery, CredentialError, CredentialSource, EnvVar, Event, LocalDeletion,
    LogoutTrigger, NewLogin, NextDelivery, RemoteLogout, SentCodeInfo, StorageError, UserError,
};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, KeyDownEvent, KeyUpEvent, Keystroke, PlatformInput,
    Window, WindowBounds, WindowHandle, WindowOptions, px, size,
};
use objc2::rc::Retained;
use objc2_app_kit::{NSAccessibility, NSAccessibilityElement, NSView, NSWindow};
use objc2_foundation::{NSArray, NSRange};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::Value;

use crate::launch::LaunchError;
use crate::shell::{self, EventSink, Root};
use crate::ui::{copy, text_input};

const HASH: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "fake tree regression secret";
/// A login code the tree must never carry: five digits, as the sign-in sent.
const CODE: &str = "90817";

#[path = "native_a11y/signed_in.rs"]
mod signed_in;

pub(super) fn run() {
    let platform = gpui_platform::current_platform(false);
    platform.set_activation_policy(gpui::ActivationPolicy::Accessory);
    gpui::Application::with_platform(platform).run(|cx| {
        cx.bind_keys(shell::key_bindings());
        cx.bind_keys(text_input::key_bindings());
        // Assertions must not unwind through AppKit's launch callback.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            startup_failure(cx);
            auth(cx);
            signed_in::run(cx);
        }));
        if result.is_err() {
            std::process::exit(1);
        }
        // The press scenes wait on the executor, and end the process.
        signed_in::run_async(cx);
    });
}

fn open(
    cx: &mut App,
    root: impl FnOnce(&mut Window, &mut App) -> gpui::Entity<Root>,
) -> WindowHandle<Root> {
    open_sized(cx, size(px(640.), px(360.)), root)
}

/// A window of the given size; the sign-in scenes use a short one on purpose.
fn open_sized(
    cx: &mut App,
    extent: gpui::Size<gpui::Pixels>,
    root: impl FnOnce(&mut Window, &mut App) -> gpui::Entity<Root>,
) -> WindowHandle<Root> {
    let bounds = Bounds::centered(None, extent, cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: false,
                show: false,
                ..WindowOptions::default()
            },
            root,
        )
        .unwrap();
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, cx| {
            activate_accesskit(window);
            draw(window, cx);
        })
        .unwrap();
    handle
}

fn activate_accesskit(window: &Window) {
    let _children = appkit_window(window).accessibilityChildren();
}

fn appkit_window(window: &Window) -> Retained<NSWindow> {
    let raw = HasWindowHandle::window_handle(window).unwrap();
    let RawWindowHandle::AppKit(handle) = raw.as_raw() else {
        panic!("expected an AppKit window");
    };
    // SAFETY: GPUI's AppKit window handle borrows its live native NSView.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window().unwrap()
}

fn descendant(
    children: Option<Retained<NSArray>>,
    label: &str,
) -> Option<Retained<NSAccessibilityElement>> {
    children?.iter().find_map(|child| {
        let element = child.downcast::<NSAccessibilityElement>().ok()?;
        if element
            .accessibilityTitle()
            .is_some_and(|title| title.to_string() == label)
        {
            Some(element)
        } else {
            descendant(element.accessibilityChildren(), label)
        }
    })
}

fn assert_native_role(handle: WindowHandle<Root>, cx: &mut App, label: &str, role: &str) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            let node = descendant(appkit_window(window).accessibilityChildren(), label)
                .unwrap_or_else(|| panic!("missing native accessibility element {label}"));
            assert_eq!(node.accessibilityRole().unwrap().to_string(), role);
        })
        .unwrap();
}

fn assert_visible(handle: WindowHandle<Root>, cx: &mut App, label: &str) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            let native = appkit_window(window);
            let frame = native.frame();
            let item = descendant(native.accessibilityChildren(), label)
                .unwrap()
                .accessibilityFrame();
            assert!(
                item.origin.y >= frame.origin.y
                    && item.origin.y + item.size.height <= frame.origin.y + frame.size.height,
                "{label} is outside the short window: {item:?} vs {frame:?}"
            );
        })
        .unwrap();
}

fn assert_selection(handle: WindowHandle<Root>, cx: &mut App, label: &str, expected: NSRange) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            let node = descendant(appkit_window(window).accessibilityChildren(), label).unwrap();
            assert_eq!(node.accessibilitySelectedTextRange(), expected);
        })
        .unwrap();
}

fn draw(window: &mut Window, cx: &mut App) {
    window.refresh();
    window.draw(cx).clear(cx);
}

fn tree(handle: WindowHandle<Root>, cx: &mut App) -> Value {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, cx| {
            draw(window, cx);
            serde_json::from_str(
                &window
                    .debug_a11y_tree_json()
                    .expect("AccessKit was activated"),
            )
            .unwrap()
        })
        .unwrap()
}

fn key(handle: WindowHandle<Root>, cx: &mut App, key: &'static str) {
    let keystroke = Keystroke::parse(key).unwrap();
    keystroke_events(handle, cx, keystroke);
}

fn keystroke_events(handle: WindowHandle<Root>, cx: &mut App, keystroke: Keystroke) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
            window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
        })
        .unwrap();
}

fn type_text(handle: WindowHandle<Root>, cx: &mut App, value: &str) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, cx| {
            for ch in value.chars() {
                let text = ch.to_string();
                let name = if ch == ' ' { "space" } else { &text };
                let mut keystroke = Keystroke::parse_with_os(name, cx.platform_info().os).unwrap();
                keystroke.key_char = Some(text);
                assert!(window.dispatch_keystroke(keystroke, cx));
            }
        })
        .unwrap();
}

fn assert_controls(tree: &Value, expected: &[(&str, &str)]) {
    let nodes = tree["nodes"].as_object().unwrap();
    assert_eq!(
        tree["frame"]["tab_stop_count"].as_u64().unwrap(),
        u64::try_from(expected.len()).unwrap()
    );
    let mut remaining = expected.to_vec();
    for node in nodes.values() {
        let aria = &node["aria"];
        let role = aria["role"].as_str().unwrap();
        if aria["on_action"].is_null() {
            continue;
        }
        let label = aria["label"].as_str().unwrap_or_default();
        assert!(!label.trim().is_empty(), "missing label for {role}");
        if role == "Group" {
            assert_eq!(label, "Telegram sign-in");
            continue;
        }
        let Some(index) = remaining.iter().position(|pair| *pair == (role, label)) else {
            panic!("unexpected interactive node role {role}");
        };
        remaining.remove(index);
        let action = match role {
            "Button" | "Link" => "Click",
            _ => "Focus",
        };
        assert!(
            aria["on_action"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == action)
        );
    }
    assert!(remaining.is_empty(), "missing controls: {remaining:?}");
}

fn find_node<'a>(tree: &'a Value, role: &str, label: &str) -> &'a Value {
    tree["nodes"]
        .as_object()
        .unwrap()
        .values()
        .find(|node| node["aria"]["role"] == role && node["aria"]["label"] == label)
        .expect("expected node")
}

fn assert_focus(tree: &Value, label: &str) {
    let focused = tree["gpui_focus"].as_str().unwrap();
    assert_eq!(tree["nodes"][focused]["aria"]["label"], label);
}

fn assert_secret(tree: &Value, label: &str, secret: &str) {
    let field = find_node(tree, "PasswordInput", label);
    assert!(field["aria"]["value"].is_null());
    let children = field["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    let run = &tree["nodes"][children[0].as_str().unwrap()];
    assert_eq!(run["aria"]["role"], "TextRun");
    assert_eq!(run["aria"]["value"], "•".repeat(secret.chars().count()));
    assert!(run["children"].is_null());
    assert!(
        !tree.to_string().contains(secret),
        "secret appeared in the tree"
    );
}

fn startup_failure(cx: &mut App) {
    let handle = open(cx, |window, cx| {
        cx.new(|cx| Root::new(Err(LaunchError::FakeNotBuilt), None, window, cx))
    });
    let tree = tree(handle, cx);
    find_node(&tree, "Heading", "This build has no demo backend");
    assert_controls(&tree, &[]);
    assert!(
        tree["nodes"]
            .as_object()
            .unwrap()
            .values()
            .any(|node| node["aria"]["role"] == "Alert")
    );
}

fn apply(handle: WindowHandle<Root>, cx: &mut App, state: AuthState) {
    handle
        .update(cx, |root, window, cx| {
            root.apply(vec![(1, Event::Auth(state))], window, cx);
        })
        .unwrap();
}

fn auth(cx: &mut App) {
    let (started, _harness) = shell::tests::started_fixture();
    let handle = open(cx, |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    onboarding(handle, cx);
    phone(handle, cx);
    code(handle, cx);
    password(handle, cx);
    onboarding_errors(handle, cx);
    auth_outcomes(handle, cx);
    logout_outcomes(handle, cx);
}

fn onboarding(handle: WindowHandle<Root>, cx: &mut App) {
    use copy::onboarding as text;

    apply(
        handle,
        cx,
        AuthState::NeedCredentials {
            error: None,
            source: None,
        },
    );
    let welcome = tree(handle, cx);
    find_node(&welcome, "Heading", text::WELCOME_TITLE);
    assert_native_role(handle, cx, text::WELCOME_TITLE, "Heading");
    assert!(
        welcome["nodes"]
            .as_object()
            .unwrap()
            .values()
            .any(|node| node["aria"]["value"] == copy::DESCRIPTOR)
    );
    assert_controls(&welcome, &[("Button", text::CONNECT)]);
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), text::CONNECT);
    key(handle, cx, "enter");

    let guide = tree(handle, cx);
    find_node(&guide, "Heading", text::GUIDE_TITLE);
    assert_controls(
        &guide,
        &[
            ("Link", text::OPEN_SITE),
            ("Button", text::BACK),
            ("Button", text::HAVE_CREDENTIALS),
        ],
    );
    for label in [text::OPEN_SITE, text::BACK, text::HAVE_CREDENTIALS] {
        key(handle, cx, "tab");
        assert_focus(&tree(handle, cx), label);
        assert_visible(handle, cx, label);
    }
    key(handle, cx, "shift-tab");
    assert_focus(&tree(handle, cx), text::BACK);
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), text::HAVE_CREDENTIALS);
    key(handle, cx, "space");
    credential_entry(handle, cx);
}

fn credential_entry(handle: WindowHandle<Root>, cx: &mut App) {
    use copy::onboarding as text;

    let entry = tree(handle, cx);
    find_node(&entry, "Heading", text::ENTRY_TITLE);
    assert_focus(&entry, text::API_ID_LABEL);
    assert_visible(handle, cx, text::API_ID_LABEL);
    assert_controls(
        &entry,
        &[
            ("TextInput", text::API_ID_LABEL),
            ("PasswordInput", text::API_HASH_LABEL),
            ("Button", "Show API hash on screen"),
            ("Button", text::BACK),
            ("Button", copy::CONTINUE),
        ],
    );
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), text::API_HASH_LABEL);
    type_text(handle, cx, HASH);
    assert_secret(&tree(handle, cx), text::API_HASH_LABEL, HASH);
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), "Show API hash on screen");
    key(handle, cx, "enter");
    let revealed = tree(handle, cx);
    assert_eq!(
        find_node(&revealed, "Button", "Show API hash on screen")["aria"]["toggled"],
        "True"
    );
    assert_secret(&revealed, text::API_HASH_LABEL, HASH);
}

fn phone(handle: WindowHandle<Root>, cx: &mut App) {
    apply(handle, cx, AuthState::Phone { error: None });
    let before = tree(handle, cx);
    find_node(&before, "Heading", copy::PHONE_TITLE);
    assert_focus(&before, copy::PHONE_LABEL);
    assert_visible(handle, cx, copy::PHONE_LABEL);
    assert_controls(
        &before,
        &[
            ("TextInput", copy::PHONE_LABEL),
            ("Button", copy::onboarding::BACK),
            ("Button", copy::CONTINUE),
        ],
    );
    type_text(handle, cx, "123");
    key(handle, cx, "shift-left");
    let _ = tree(handle, cx);
    assert_selection(handle, cx, copy::PHONE_LABEL, NSRange::new(2, 1));
    apply(
        handle,
        cx,
        AuthState::Phone {
            error: Some(UserError::PhoneInvalid),
        },
    );
    let after = tree(handle, cx);
    let alert = |tree: &Value| {
        tree["nodes"].as_object().unwrap().values().any(|node| {
            node["aria"]["role"] == "Alert"
                && node["aria"]["value"] == copy::user_error(UserError::PhoneInvalid).as_ref()
        })
    };
    assert!(!alert(&before));
    assert!(alert(&after));
    assert_focus(&after, copy::PHONE_LABEL);
}

fn code(handle: WindowHandle<Root>, cx: &mut App) {
    apply(
        handle,
        cx,
        AuthState::Code {
            phone_hint: "••42".into(),
            sent: Some(SentCodeInfo {
                delivery: CodeDelivery::App,
                length: Some(5),
                next: Some(NextDelivery::Sms),
                resend_at: None,
            }),
            error: None,
        },
    );
    let code = tree(handle, cx);
    find_node(&code, "Heading", copy::CODE_TITLE);
    assert_focus(&code, copy::CODE_LABEL);
    assert_controls(
        &code,
        &[
            ("PasswordInput", copy::CODE_LABEL),
            ("Button", copy::onboarding::RESEND),
            ("Button", copy::onboarding::BACK),
            ("Button", copy::CONTINUE),
        ],
    );
    // The live sign-in credential is bullets to accessibility (review A3),
    // in the AccessKit tree and in what AppKit hands a client.
    type_text(handle, cx, CODE);
    key(handle, cx, "shift-left");
    assert_secret(&tree(handle, cx), copy::CODE_LABEL, CODE);
    assert_selection(
        handle,
        cx,
        copy::CODE_LABEL,
        NSRange::new(CODE.chars().count() - 1, 1),
    );
    assert_native_secret(handle, cx, copy::CODE_LABEL, CODE);
    // Clear it, so nothing typed here leaks into the scenes after.
    key(handle, cx, "cmd-a");
    key(handle, cx, "backspace");
}

/// No native element under the field (nor the field) exposes the secret as
/// its value, title, description or help.
fn assert_native_secret(handle: WindowHandle<Root>, cx: &mut App, label: &str, secret: &str) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, window, _| {
            let field = descendant(appkit_window(window).accessibilityChildren(), label)
                .unwrap_or_else(|| panic!("missing native accessibility element {label}"));
            assert_eq!(
                field.accessibilityRole().unwrap().to_string(),
                "AXTextField"
            );
            check_native_tree(&field, secret, 0);
        })
        .unwrap();
}

fn check_native_tree(element: &NSAccessibilityElement, secret: &str, depth: usize) {
    assert!(depth < 8, "native subtree is deeper than expected");
    let mut seen = vec![
        format!("{:?}", element.accessibilityValue()),
        format!("{:?}", element.accessibilityLabel()),
        format!("{:?}", element.accessibilityHelp()),
        format!("{:?}", element.accessibilityTitle()),
    ];
    seen.extend(
        element
            .accessibilityPlaceholderValue()
            .map(|p| p.to_string()),
    );
    for text in &seen {
        assert!(!text.contains(secret), "secret exposed natively: {text}");
    }
    let children = element.accessibilityChildren();
    for child in children.iter().flat_map(|c| c.iter()) {
        if let Ok(child) = child.downcast::<NSAccessibilityElement>() {
            check_native_tree(&child, secret, depth + 1);
        }
    }
}

fn password(handle: WindowHandle<Root>, cx: &mut App) {
    apply(
        handle,
        cx,
        AuthState::Password {
            hint: None,
            error: None,
        },
    );
    let initial = tree(handle, cx);
    find_node(&initial, "Heading", copy::PASSWORD_TITLE);
    assert_focus(&initial, copy::PASSWORD_LABEL);
    assert_controls(
        &initial,
        &[
            ("PasswordInput", copy::PASSWORD_LABEL),
            ("Button", copy::onboarding::BACK),
            ("Button", copy::CONTINUE),
        ],
    );
    type_text(handle, cx, PASSWORD);
    key(handle, cx, "shift-left");
    assert_secret(&tree(handle, cx), copy::PASSWORD_LABEL, PASSWORD);
    assert_selection(
        handle,
        cx,
        copy::PASSWORD_LABEL,
        NSRange::new(PASSWORD.chars().count() - 1, 1),
    );
}

fn check_scene(
    handle: WindowHandle<Root>,
    cx: &mut App,
    state: AuthState,
    heading: &str,
    buttons: &[(&str, &str)],
) {
    apply(handle, cx, state);
    let tree = tree(handle, cx);
    find_node(&tree, "Heading", heading);
    assert_controls(&tree, buttons);
}

fn onboarding_errors(handle: WindowHandle<Root>, cx: &mut App) {
    use copy::onboarding as text;

    check_scene(
        handle,
        cx,
        AuthState::NeedCredentials {
            error: Some(CredentialError::EnvPartial {
                missing: EnvVar::ApiHash,
            }),
            source: Some(CredentialSource::Env),
        },
        text::ENV_TITLE,
        &[],
    );
    check_scene(
        handle,
        cx,
        AuthState::NeedCredentials {
            error: Some(CredentialError::KeychainUnavailable),
            source: Some(CredentialSource::Stored),
        },
        text::KEYCHAIN_TITLE,
        &[("Button", text::RETRY)],
    );
    check_scene(
        handle,
        cx,
        AuthState::NeedCredentials {
            error: None,
            source: Some(CredentialSource::Stored),
        },
        text::ENTRY_TITLE,
        &[
            ("TextInput", text::API_ID_LABEL),
            ("PasswordInput", text::API_HASH_LABEL),
            ("Button", "Show API hash on screen"),
            ("Button", text::USE_SAVED),
            ("Button", text::FORGET_SAVED),
            ("Button", text::BACK),
            ("Button", copy::CONTINUE),
        ],
    );
    key(handle, cx, "enter");
    let entry = tree(handle, cx);
    assert!(entry["nodes"].as_object().unwrap().values().any(|node| {
        node["aria"]["role"] == "Alert" && node["aria"]["value"] == "Enter the api_id."
    }));
}

fn auth_outcomes(handle: WindowHandle<Root>, cx: &mut App) {
    use copy::onboarding as text;

    let back = &[("Button", text::BACK)][..];
    let open = &[("Button", text::OPEN_CHATS)][..];
    let new_login = Some(NewLogin {
        has_password: Some(false),
    });
    for (state, heading, controls) in [
        (AuthState::Connecting, copy::CONNECTING, &[][..]),
        (
            AuthState::FloodWait {
                until: SystemTime::now() + Duration::from_secs(60),
                resume: Box::new(AuthState::Phone { error: None }),
            },
            copy::FLOOD_TITLE,
            back,
        ),
        (
            AuthState::Verifying {
                error: Some(UserError::InvalidCode),
            },
            text::VERIFYING_TITLE,
            &[],
        ),
        (AuthState::SignUpRequired, text::SIGN_UP_TITLE, back),
        (
            AuthState::OfficialAppRequired {
                url: Some("https://fragment.com/".into()),
            },
            text::OFFICIAL_APP_TITLE,
            back,
        ),
        (
            AuthState::SignedIn {
                display_name: "Demo User".into(),
                new_login,
            },
            text::DONE_TITLE,
            open,
        ),
        (
            AuthState::SignedInUnsaved {
                display_name: "Demo User".into(),
                new_login,
            },
            text::UNSAVED_TITLE,
            open,
        ),
    ] {
        check_scene(handle, cx, state, heading, controls);
    }
}

fn logout_outcomes(handle: WindowHandle<Root>, cx: &mut App) {
    use copy::onboarding as text;

    let retry = &[("Button", text::RETRY)][..];
    let stay = &[("Button", copy::STAY_SIGNED_IN), ("Button", text::RETRY)][..];
    for (state, heading, controls) in [
        (AuthState::LoggingOut, text::LOGGING_OUT_TITLE, &[][..]),
        (
            AuthState::LoggedOut {
                remote: RemoteLogout::TimedOut,
                local: LocalDeletion::Failed(StorageError::Local),
            },
            copy::LOGOUT_INCOMPLETE_TITLE,
            retry,
        ),
        (
            AuthState::LogoutNotStarted {
                error: StorageError::Local,
                trigger: LogoutTrigger::User,
            },
            copy::LOGOUT_NOT_STARTED_TITLE,
            stay,
        ),
        (
            AuthState::StartupBlocked {
                error: StorageError::Local,
            },
            copy::STARTUP_BLOCKED_TITLE,
            retry,
        ),
    ] {
        check_scene(handle, cx, state, heading, controls);
    }
}
