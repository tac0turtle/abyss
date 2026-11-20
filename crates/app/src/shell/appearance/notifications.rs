//! Notification permission and device-local preferences in Settings.

use gpui::{Context, Div, FontWeight, div, prelude::*};

use super::super::Root;
use super::{SwitchState, button, setting_switch};
use crate::notifications::{BadgePreference, Notifications, SETTINGS_URL};
use crate::ui::a11y;
use crate::ui::theme::Theme;

#[derive(Clone, Copy)]
pub(in crate::shell) struct Options {
    authorization: &'static str,
    /// The system has not been asked: Settings offers to ask.
    can_enable: bool,
    badge: Option<bool>,
}

impl Options {
    pub(in crate::shell) fn of(root: &Root) -> Self {
        // A removal-only center (its epoch seed failed) never delivers and its
        // badge is always 0, so the switch would do nothing (review N49).
        let (authorization, badge) =
            root.notifications
                .as_ref()
                .map_or(("Available in the abyss app bundle", None), |n| {
                    (
                        n.authorization.label(),
                        n.can_deliver()
                            .then_some(n.badge_preference == BadgePreference::On),
                    )
                });
        Self {
            authorization,
            can_enable: root
                .notifications
                .as_ref()
                .is_some_and(Notifications::can_request_authorization),
            badge,
        }
    }

    /// Whether the Dock badge switch can be used.
    #[cfg(all(test, feature = "fake"))]
    pub(in crate::shell) fn badge_switch_available(self) -> bool {
        self.badge.is_some()
    }
}

pub(super) fn section(options: Options, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let settings = a11y::on_activate(
        button("macos-notifications", "Open System Settings", theme),
        |_, cx| cx.open_url(SETTINGS_URL),
    );
    let enable = options.can_enable.then(|| {
        let enable = button("enable-notifications", "Enable notifications", theme)
            .debug_selector(|| "enable-notifications".to_owned());
        a11y::on_activate_in(enable, cx, |root, _, cx| root.enable_notifications(cx))
    });
    let badge = options
        .badge
        .map_or(SwitchState::Unavailable, SwitchState::Ready);
    let badge = setting_switch("dock-badge", "Dock badge", badge, theme);
    let badge = if options.badge.is_some() {
        a11y::on_activate_in(badge, cx, |root, _, cx| root.toggle_dock_badge(cx))
    } else {
        badge
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            a11y::heading("notifications-heading", "Notifications", 2)
                .font_weight(FontWeight::MEDIUM),
        )
        .child(row(
            div()
                .opacity(theme.quiet_opacity())
                .text_sm()
                .child(a11y::text(
                    "notification-authorization",
                    options.authorization,
                )),
            div().flex().gap_2().children(enable).child(settings),
        ))
        .child(row(div().child("Dock badge"), badge))
}

fn row(label: impl IntoElement, control: impl IntoElement) -> Div {
    div()
        .flex()
        .justify_between()
        .items_center()
        .child(label)
        .child(control)
}
