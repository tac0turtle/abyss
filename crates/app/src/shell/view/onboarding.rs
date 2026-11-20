//! The onboarding and login-outcome screens (plan section 17.3). All text
//! comes from `ui::copy::onboarding`.

use std::time::Instant;

use abyss_core::api::{CredentialError, UserError};
use gpui::{AnyElement, Context, Div, Role, SharedString, Stateful, Toggled, div, prelude::*, px};

use super::{
    LoginForm, auth_button, back_button, brand, card, error_line, footer, heading_style, on, page,
    quiet_lines, secondary, secondary_style, title,
};
use crate::shell::Root;
use crate::shell::auth::{Onboarding, Saved, SentView, Stored};
use crate::shell::session::Session;
use crate::ui::password::Reveal;
use crate::ui::theme::Theme;
use crate::ui::{a11y, copy::onboarding as text};

/// The credential screens.
pub(super) fn screen(
    session: &Session,
    step: &Onboarding,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    match step {
        Onboarding::Welcome => welcome(theme, cx),
        Onboarding::Guide => guide(session, theme, cx),
        Onboarding::Entry { error, stored } => entry(session, *error, *stored, theme, cx),
        Onboarding::Env { error } => env(*error, theme),
        Onboarding::Keychain => keychain(theme, cx),
    }
}

fn welcome(theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let connect = on(
        auth_button("connect", text::CONNECT, theme),
        Session::connect,
        cx,
    );
    page(
        card()
            .child(brand(theme))
            .child(title(text::WELCOME_TITLE))
            .children(quiet_lines(
                "welcome",
                [crate::ui::copy::DESCRIPTOR]
                    .into_iter()
                    .chain(text::WELCOME_LINES),
                theme,
            ))
            .child(footer(None, vec![connect])),
    )
}

fn guide(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let link = a11y::control("open-site", Role::Link, text::OPEN_SITE).child(text::OPEN_SITE);
    let open = on(
        secondary_style(a11y::focus_ring(link, theme.text), theme),
        |_, _, cx| Session::open_site(cx),
        cx,
    );
    let next = on(
        auth_button("have-credentials", text::HAVE_CREDENTIALS, theme),
        Session::have_credentials,
        cx,
    );
    page(
        card()
            .w(px(560.))
            .child(title(text::GUIDE_TITLE))
            .children(quiet_lines("guide-intro", [text::GUIDE_INTRO], theme))
            .child(div().flex().child(open))
            .child(guide_steps())
            .child(div().pt_2().child(heading_style(a11y::heading(
                "troubleshooting",
                text::TROUBLE_TITLE,
                2,
            ))))
            .children(quiet_lines("troubleshooting-steps", text::TROUBLE, theme))
            .child(footer(back_button(session, theme, cx), vec![next])),
    )
}

fn guide_steps() -> Stateful<Div> {
    div()
        .id("guide-steps")
        .role(Role::List)
        .aria_label("Credential setup steps")
        .flex()
        .flex_col()
        .gap_3()
        .children(
            text::GUIDE_STEPS
                .into_iter()
                .enumerate()
                .map(|(i, step)| a11y::list_row(i, [step]).child(step)),
        )
}

fn entry(
    session: &Session,
    error: Option<CredentialError>,
    stored: Stored,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let (reveal_label, toggled) = match session.api_hash.read(cx).reveal() {
        Reveal::Hidden => (text::REVEAL, Toggled::False),
        Reveal::Shown => (text::HIDE, Toggled::True),
    };
    let reveal = on(
        secondary("reveal-hash", reveal_label, theme)
            .aria_label("Show API hash on screen")
            .aria_toggled(toggled),
        |s, _, cx| s.toggle_hash_mask(cx),
        cx,
    );
    let hash_row = div()
        .flex()
        .gap_2()
        .items_center()
        .child(div().flex_1().child(session.api_hash.clone()))
        .child(reveal);
    // A local check failure wins over core's last verdict.
    let problem = session
        .entry_error
        .clone()
        .or_else(|| error.map(|e| text::credential_error(e, session.source)));
    let submit = on(
        auth_button("continue", crate::ui::copy::CONTINUE, theme),
        Session::submit_credentials,
        cx,
    );
    page(
        card()
            .child(title(text::ENTRY_TITLE))
            .children(quiet_lines("entry-detail", [text::ENTRY_DETAIL], theme))
            .child(session.api_id.clone())
            .child(hash_row)
            .children(problem.map(|p| error_line(p, theme)))
            .children(stored_pair(stored, theme, cx))
            .child(footer(back_button(session, theme, cx), vec![submit])),
    )
}

/// A saved pair exists: use it or forget it.
fn stored_pair(stored: Stored, theme: &Theme, cx: &mut Context<'_, Root>) -> Option<Div> {
    if stored == Stored::No {
        return None;
    }
    let use_saved = on(
        secondary("use-saved", text::USE_SAVED, theme),
        Session::use_saved,
        cx,
    );
    let forget = on(
        secondary("forget-saved", text::FORGET_SAVED, theme),
        Session::forget_saved,
        cx,
    );
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(quiet_lines("stored-note", [text::STORED_NOTE], theme))
            .child(div().flex().gap_2().child(use_saved).child(forget)),
    )
}

/// The env override is unusable: fix it outside abyss. No entry, no Back.
fn env(error: CredentialError, theme: &Theme) -> AnyElement {
    let mut lines = text::env_detail(error).into_iter();
    page(
        card()
            .w(px(560.))
            .child(title(text::ENV_TITLE))
            .children(
                lines
                    .next()
                    .map(|line| a11y::alert("env-error", line).opacity(theme.quiet_opacity())),
            )
            .children(lines.enumerate().map(|(i, line)| {
                div()
                    .opacity(theme.quiet_opacity())
                    .child(gpui::text!(id = ("env-recovery", i), line))
            })),
    )
}

/// The Keychain could not be read: retry, never re-onboard.
fn keychain(theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let retry = on(
        auth_button("retry-keychain", text::RETRY, theme),
        Session::use_saved,
        cx,
    );
    page(
        card()
            .child(title(text::KEYCHAIN_TITLE))
            .child(error_line(text::KEYCHAIN_DETAIL, theme))
            .child(footer(None, vec![retry])),
    )
}

/// The code screen: where the code went, the fixed warning, and resend.
pub(super) fn code_screen(
    session: &Session,
    phone_hint: &str,
    sent: Option<&SentView>,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let mut extra = vec![
        div()
            .child(gpui::text!(text::CODE_WARNING))
            .into_any_element(),
        div()
            .opacity(theme.quiet_opacity())
            .child(gpui::text!(text::NEW_LOGIN_NOTICE))
            .into_any_element(),
    ];
    if let Some(resend) = resend_row(sent, theme, cx) {
        extra.push(resend.into_any_element());
    }
    let form = LoginForm {
        heading: crate::ui::copy::CODE_TITLE,
        detail: text::code_detail(
            phone_hint,
            sent.map(|s| &s.delivery),
            sent.and_then(|s| s.length),
        ),
        input: session.code.clone().into_any_element(),
        extra,
        error,
        submit: Session::submit_code,
        back: back_button(session, theme, cx),
    };
    form.render(theme, cx)
}

/// When Telegram offers another delivery: a countdown, then a button.
fn resend_row(sent: Option<&SentView>, theme: &Theme, cx: &mut Context<'_, Root>) -> Option<Div> {
    let sent = sent?;
    let next = sent.next?;
    let left = sent
        .resend_at
        .map(|at| at.saturating_duration_since(Instant::now()))
        .filter(|d| !d.is_zero())
        .map(|d| d.as_secs() + 1);
    let mut row = div().flex().gap_2().items_center().child(
        div()
            .opacity(theme.quiet_opacity())
            .child(gpui::text!(text::resend_detail(next, left))),
    );
    if left.is_none() {
        let resend: Stateful<Div> = on(
            secondary("resend", text::RESEND, theme),
            Session::resend_code,
            cx,
        );
        row = row.child(resend);
    }
    Some(row)
}

pub(super) fn verifying_screen(error: Option<UserError>, theme: &Theme) -> AnyElement {
    page(
        card()
            .child(title(text::VERIFYING_TITLE))
            .children(quiet_lines(
                "verifying-detail",
                [text::VERIFYING_DETAIL],
                theme,
            ))
            .children(error.map(|e| error_line(crate::ui::copy::user_error(e), theme))),
    )
}

/// Sign-up required, or a step only an official app can do: explain, and
/// offer Back to the phone screen.
pub(super) fn dead_end_screen(
    session: &Session,
    heading: &'static str,
    detail: &'static str,
    note: Option<SharedString>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    page(
        card()
            .child(title(heading))
            .children(quiet_lines("dead-end-detail", [detail], theme))
            .children(note.map(|note| {
                div()
                    .opacity(theme.quiet_opacity())
                    .child(gpui::text!(note))
            }))
            .child(footer(back_button(session, theme, cx), Vec::new())),
    )
}

pub(super) fn logging_out_screen(theme: &Theme) -> AnyElement {
    page(
        card()
            .child(title(text::LOGGING_OUT_TITLE))
            .children(quiet_lines(
                "logout-detail",
                [text::LOGGING_OUT_DETAIL],
                theme,
            )),
    )
}

/// After a login in this run: tips, the 2FA recommendation, then the chats.
pub(super) fn done_screen(
    has_password: Option<bool>,
    saved: Saved,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let (heading, unsaved) = match saved {
        Saved::Yes => (text::DONE_TITLE, None),
        Saved::No => (
            text::UNSAVED_TITLE,
            Some(error_line(crate::ui::copy::SIGNED_IN_UNSAVED, theme)),
        ),
    };
    let no_2fa =
        (has_password == Some(false)).then(|| div().child(gpui::text!(text::RECOMMEND_2FA)));
    let open = on(
        auth_button("open-chats", text::OPEN_CHATS, theme),
        Session::open_chats,
        cx,
    );
    page(
        card()
            .child(title(heading))
            .children(unsaved)
            .children(quiet_lines("done-tips", text::DONE_TIPS, theme))
            .children(no_2fa)
            .child(footer(None, vec![open])),
    )
}
