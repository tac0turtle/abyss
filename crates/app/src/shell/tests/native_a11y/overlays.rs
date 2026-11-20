//! Settings, context menus and focus fallback through the native window.

use gpui::{
    App, AppContext, PlatformInput, ScrollDelta, ScrollEvent, Toggled, WindowHandle, point, px,
    size,
};

use super::super::{appkit_window, assert_native_role, assert_visible, descendant};
use super::{
    Root, assert_focus, find_node, key, nodes_of, open_sized, send, signed_in, summary, tab_to,
    tree,
};
use crate::chat::copy;
use crate::ui::display::DisplayPreferences;
use crate::ui::theme::{Colourway, Themes};

pub(super) fn run(cx: &mut App) {
    let (started, _harness) = crate::shell::tests::started_fixture();
    let handle = open_sized(cx, size(px(700.), px(365.)), |window, cx| {
        cx.new(|cx| Root::with_started(Ok(started), window, cx))
    });
    signed_in(handle, cx);
    chat_menu(handle, cx);
    folder_menu(handle, cx);
    settings(handle, cx);
    logout(handle, cx);
    virtualized_focus(handle, cx);
    display_preferences(handle, cx);
}

fn chat_menu(handle: WindowHandle<Root>, cx: &mut App) {
    let before = tree(handle, cx);
    let alice = nodes_of(&before, "ListItem")
        .iter()
        .filter_map(|node| node["aria"]["label"].as_str())
        .find(|label| label.starts_with("Alice,"))
        .unwrap();
    tab_to(handle, cx, alice);
    assert_native_role(handle, cx, "Open menu for Alice", "AXButton");
    key(handle, cx, "shift-f10");
    let _ = tree(handle, cx);
    let menu = tree(handle, cx);
    find_node(&menu, "Menu", "Actions for Alice");
    assert_native_role(handle, cx, "Actions for Alice", "AXMenu");
    assert_native_role(handle, cx, "Archive chat", "AXMenuItem");
    assert_focus(&menu, "Archive chat");
    let work = find_node(&menu, "Switch", "Include in Work");
    assert_eq!(work["aria"]["toggled"], "False");
    assert_switch(handle, cx, "Include in Work", Toggled::False);
    key(handle, cx, "down");
    assert_focus(&tree(handle, cx), "Include in Work");
    key(handle, cx, "down");
    assert_focus(&tree(handle, cx), copy::NEW_FOLDER);
    key(handle, cx, "down");
    assert_focus(&tree(handle, cx), "Archive chat");
    key(handle, cx, "up");
    key(handle, cx, "enter");
    assert_focus(&tree(handle, cx), "Folder name");
    assert_native_role(handle, cx, "Folder name", "AXTextField");
    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), alice);
}

fn folder_menu(handle: WindowHandle<Root>, cx: &mut App) {
    tab_to(handle, cx, "All, 2 unread");
    key(handle, cx, "ctrl-shift-tab");
    let rail = tree(handle, cx);
    let tabs = nodes_of(&rail, "Tab");
    let work = tabs
        .iter()
        .find_map(|node| {
            let label = node["aria"]["label"].as_str()?;
            label.starts_with("Work,").then_some(label)
        })
        .unwrap();
    assert_focus(&rail, work);
    assert_native_role(handle, cx, "Open menu for Work", "AXButton");
    key(handle, cx, "shift-f10");
    let _ = tree(handle, cx);
    let menu = tree(handle, cx);
    assert_native_role(handle, cx, "Actions for Work", "AXMenu");
    let delete = copy::delete_folder("Work");
    assert_focus(&menu, &delete);
    key(handle, cx, "enter");
    let confirm = copy::confirm_delete_folder("Work");
    assert_focus(&tree(handle, cx), &confirm);
    assert_native_role(handle, cx, &confirm, "AXMenuItem");
    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), work);
    key(handle, cx, "ctrl-tab");
    let _ = tree(handle, cx);
}

fn settings(handle: WindowHandle<Root>, cx: &mut App) {
    Themes::set_colourway(Colourway::Ultramarine, cx);
    tab_to(handle, cx, "Settings");
    key(handle, cx, "enter");
    let _ = tree(handle, cx);
    let before = tree(handle, cx);
    find_node(&before, "Dialog", "Settings");
    assert_native_role(handle, cx, "Appearance", "Heading");
    assert_radio(handle, cx, "Ultramarine", Toggled::True);
    assert_radio(handle, cx, "Sky", Toggled::False);
    assert_focus(&before, "Ultramarine");
    key(handle, cx, "right");
    assert_focus(&tree(handle, cx), "Sky");
    assert_eq!(Themes::colourway(cx), Colourway::Sky);
    assert_radio(handle, cx, "Sky", Toggled::True);
    find_node(&tree(handle, cx), "RadioGroup", "Accent hue");
    assert_radio(handle, cx, "Accent hue, Rose, 5 degrees", Toggled::False);
    Themes::set_colourway(Colourway::Abyss, cx);
    let abyss = tree(handle, cx);
    find_node(&abyss, "RadioGroup", "Night hue");
    find_node(&abyss, "RadioGroup", "Day tint");
    assert_radio(handle, cx, "Night hue, Cyan, 215 degrees", Toggled::True);
    assert_radio(handle, cx, "Day tint, 100 percent", Toggled::True);
    assert_native_role(handle, cx, "Open System Settings", "AXButton");
    assert_native_role(handle, cx, "Dock badge", "AXCheckBox");
    assert_switch(
        handle,
        cx,
        crate::ui::copy::KEYCHAIN_STORAGE,
        Toggled::False,
    );
    tab_to(handle, cx, "Done");
    assert_visible(handle, cx, "Done");
    key(handle, cx, "tab");
    assert_focus(&tree(handle, cx), "Ultramarine");
    key(handle, cx, "shift-tab");
    assert_focus(&tree(handle, cx), "Done");
    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), "Settings");
}

fn assert_switch(handle: WindowHandle<Root>, cx: &mut App, label: &str, on: Toggled) {
    use objc2_app_kit::NSAccessibility;

    assert_native_role(handle, cx, label, "AXCheckBox");
    handle
        .update(cx, |_, window, _| {
            let node = descendant(appkit_window(window).accessibilityChildren(), label).unwrap();
            assert_eq!(node.accessibilitySubrole().unwrap().to_string(), "AXSwitch");
        })
        .unwrap();
    assert_boolean_value(handle, cx, label, on);
}

fn assert_radio(handle: WindowHandle<Root>, cx: &mut App, label: &str, selected: Toggled) {
    assert_native_role(handle, cx, label, "AXRadioButton");
    assert_boolean_value(handle, cx, label, selected);
}

fn assert_boolean_value(handle: WindowHandle<Root>, cx: &mut App, label: &str, expected: Toggled) {
    use objc2_app_kit::NSAccessibility;
    use objc2_foundation::NSNumber;

    handle
        .update(cx, |_, window, _| {
            let element = descendant(appkit_window(window).accessibilityChildren(), label).unwrap();
            let value = element
                .accessibilityValue()
                .unwrap()
                .downcast::<NSNumber>()
                .unwrap();
            assert_eq!(value.boolValue(), expected == Toggled::True);
        })
        .unwrap();
}

fn logout(handle: WindowHandle<Root>, cx: &mut App) {
    let label = crate::ui::copy::onboarding::LOGOUT;
    tab_to(handle, cx, label);
    key(handle, cx, "enter");
    let _ = tree(handle, cx);
    let confirmation = tree(handle, cx);
    find_node(
        &confirmation,
        "Dialog",
        crate::ui::copy::LOGOUT_CONFIRM_TITLE,
    );
    assert_focus(&confirmation, crate::ui::copy::CANCEL);
    key(handle, cx, "escape");
    assert_focus(&tree(handle, cx), label);
}

fn virtualized_focus(handle: WindowHandle<Root>, cx: &mut App) {
    use abyss_core::api::Event;
    use abyss_core::model::{ChatId, ChatKind, PeerKind};
    let rows = (1..=90)
        .map(|n| {
            let id = ChatId {
                id: n,
                kind: PeerKind::User,
            };
            let mut chat = summary(
                id,
                &format!("Chat {n}"),
                ChatKind::Private { contact: true },
            );
            chat.last_date -= n;
            chat
        })
        .collect();
    send(handle, cx, vec![Event::DialogsReset(rows)]);
    let start = tree(handle, cx);
    let first = nodes_of(&start, "ListItem")
        .into_iter()
        .filter_map(|node| node["aria"]["label"].as_str())
        .find(|label| label.starts_with("Chat 1,"))
        .unwrap();
    tab_to(handle, cx, first);
    handle
        .update(cx, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: point(px(150.), px(200.)),
                    delta: ScrollDelta::Pixels(point(px(0.), px(-1_600.))),
                    ..ScrollEvent::default()
                }),
                cx,
            );
        })
        .unwrap();
    let _ = tree(handle, cx);
    let after = tree(handle, cx);
    let id = after["gpui_focus"].as_str().unwrap();
    let focused = &after["nodes"][id]["aria"];
    assert_eq!(focused["role"], "ListItem");
    assert!(focused["label"].as_str().unwrap().starts_with("Chat "));
    assert_ne!(focused["label"], first);
}

fn display_preferences(handle: WindowHandle<Root>, cx: &mut App) {
    let normal = handle
        .update(cx, |_, window, cx| Themes::of(window, cx))
        .unwrap();
    DisplayPreferences {
        increase_contrast: true,
        reduce_motion: true,
    }
    .apply(cx);
    assert!(cx.reduce_motion());
    let high = handle
        .update(cx, |_, window, cx| Themes::of(window, cx))
        .unwrap();
    assert_eq!(high, normal.increased_contrast());
    DisplayPreferences::default().apply(cx);
    assert!(!cx.reduce_motion());
}
