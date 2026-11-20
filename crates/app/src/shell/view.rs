//! Rendering for the main window, in the tonal theme (`docs/brand/brand.md`):
//! one hue, no accent color, emphasis by contrast only.

use std::time::Instant;

use abyss_core::api::UserError;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, FontWeight, IntoElement, Role, ScrollHandle,
    SharedString, Stateful, Window, div, prelude::*, px, text,
};

use super::appearance;
use super::auth::{AuthView, Back, back};
use super::chat_view::chat_shell;
use super::session::Session;
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::chat::copy as chat_copy;
use crate::chat::inline::Inline;
use crate::chat::list::Step;
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Mode, Side, Theme, Themes};
use crate::ui::{a11y, a11y::FocusDirection, copy};

mod onboarding;

type SubmitFn = fn(&mut Session, &mut Window, &mut Context<'_, Root>);

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if let State::Running(session) = &mut self.state {
            // The sidebar's rows are cached: a folder's expired mutes are
            // rechecked here, at most once a minute (plan section 17.7).
            session.shell.list.recheck(crate::chat::format::now());
            session.sync_composer_label(cx);
        }
        let theme = &Themes::of(window, cx);
        let (body, overlay) = match &self.state {
            State::Failed(failure) => (failure_screen(failure, theme).into_any_element(), None),
            State::Running(session) => (
                session_body(session, theme, window, cx),
                closing_overlay(session.closing, &self.closing_focus, theme, window, cx),
            ),
        };
        // Settings closes when shutdown starts (`request_quit`); it is never
        // drawn beside the closing overlay either (N43).
        let panel = (self.appearance_open && overlay.is_none()).then(|| {
            appearance::panel(
                theme,
                Mode::of(window),
                Themes::settings(cx),
                self,
                window,
                cx,
            )
        });
        let occluded = panel.is_some() || overlay.is_some();
        shell_frame(theme, window, cx)
            .child(behind_overlay("main-body", body, occluded))
            .children(panel)
            .children(overlay)
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "overlay presence controls native tree visibility"
)]
fn behind_overlay(id: &'static str, body: AnyElement, hidden: bool) -> AnyElement {
    div()
        .id(id)
        .relative()
        .size_full()
        .when(hidden, |element| {
            element
                .role(Role::Group)
                .a11y_synthetic_children(|builder| builder.parent_node().set_hidden())
        })
        .child(body)
        .into_any_element()
}

fn shell_frame(theme: &Theme, window: &Window, cx: &mut Context<'_, Root>) -> Div {
    div()
        .size_full()
        .relative()
        .flex()
        .flex_col()
        .bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .font_family(theme::BODY_FONT)
        .text_size(px(14.))
        // The content view extends under the transparent macOS title
        // bar. Paint it in the theme, but keep content below the native
        // controls. Fullscreen needs no permanent title-bar space.
        .pt(super::titlebar_inset(window))
        .on_action(cx.listener(|root, _: &super::FocusNext, window, cx| {
            root.step_focus(FocusDirection::Next, window, cx);
        }))
        .on_action(cx.listener(|root, _: &super::FocusPrevious, window, cx| {
            root.step_focus(FocusDirection::Previous, window, cx);
        }))
        .on_action(cx.listener(|root, _: &super::ForumBack, window, cx| {
            root.with_chat_shortcut(|s, cx| s.back_from_forum(window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::NextTab, window, cx| {
            root.with_chat_shortcut(|s, cx| s.step_tab(Step::Next, window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::PreviousTab, window, cx| {
            root.with_chat_shortcut(|s, cx| s.step_tab(Step::Previous, window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::ToggleUnread, window, cx| {
            root.with_chat_shortcut(|s, cx| s.toggle_unread(window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::NextRow, window, cx| {
            root.with_chat_shortcut(|s, cx| s.step_row(Step::Next, window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::PreviousRow, window, cx| {
            root.with_chat_shortcut(|s, cx| s.step_row(Step::Previous, window, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::ExpandForum, _, cx| {
            root.with_chat_shortcut(|s, cx| s.set_inline(Inline::Expanded, cx), cx);
        }))
        .on_action(cx.listener(|root, _: &super::CollapseForum, _, cx| {
            root.with_chat_shortcut(|s, cx| s.set_inline(Inline::Collapsed, cx), cx);
        }))
}

/// A card screen: scrolls when its content is taller than the window, and
/// is centered when it fits (task 12: the guide was cut off, buttons
/// unreachable, on a short window).
fn page(content: impl IntoElement) -> AnyElement {
    Page(content.into_any_element()).into_any_element()
}

#[derive(IntoElement)]
struct Page(AnyElement);

impl RenderOnce for Page {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_state(cx, |_, _| ScrollHandle::new());
        let scroll = state.read(cx).clone();
        div()
            .id("page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .on_action(move |action: &a11y::RevealFocus, window, _| {
                action.scroll_page(&scroll, window);
            })
            .child(
                div()
                    .w_full()
                    .min_h_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .py_4()
                    .child(self.0),
            )
    }
}

pub(super) fn centered() -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
}

pub(super) fn card() -> Div {
    div().w(px(420.)).flex().flex_col().gap_3().p_6()
}

fn title(text: impl Into<SharedString>) -> Stateful<Div> {
    heading_style(a11y::heading("heading", text, 1))
}

fn heading_style<E: Styled>(heading: E) -> E {
    heading.text_xl().font_weight(FontWeight::SEMIBOLD)
}

/// Quiet text with a stable identity for each accessible paragraph.
fn quiet_lines<'a>(
    id: &'static str,
    lines: impl IntoIterator<Item = &'a str>,
    theme: &Theme,
) -> impl Iterator<Item = Div> {
    let opacity = theme.quiet_opacity();
    lines
        .into_iter()
        .enumerate()
        .map(move |(i, line)| div().opacity(opacity).child(text!(id = (id, i), line)))
}

/// Quiet text: whatever color it inherits, at reduced strength.
pub(super) fn muted(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div().opacity(theme.quiet_opacity()).child(text.into())
}

/// Quiet text a screen reader announces when it arrives or changes: a
/// plain string child has no accessibility node of its own.
pub(super) fn quiet_status(
    id: &'static str,
    text: impl Into<SharedString>,
    theme: &Theme,
) -> Stateful<Div> {
    a11y::status(id, text).opacity(theme.quiet_opacity())
}

/// Quiet text inside a message bubble on `side`, at that bubble's quiet
/// strength.
pub(super) fn muted_in(text: impl Into<SharedString>, side: Side, theme: &Theme) -> Div {
    div().opacity(theme.quiet_in(side)).child(text.into())
}

fn failure_screen(failure: &Failure, theme: &Theme) -> impl IntoElement {
    let (heading, lines) = match failure {
        Failure::Launch(error) => copy::launch_error(error),
        Failure::Start(error) => copy::start_error(error),
    };
    page(
        card()
            .w(px(560.))
            .child(title(heading))
            .children(lines.into_iter().enumerate().map(|(i, message)| {
                a11y::alert(("startup-error", i), message).opacity(theme.quiet_opacity())
            })),
    )
}

/// What the session shows (task 6m).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Screen {
    /// The auth router (`SignedIn` is the chat shell).
    Auth,
    /// Core stopped with account state kept: the local recovery view (the
    /// chat shell, nothing asked of core) under an honest banner.
    Recovery,
    /// Core stopped with no account state kept: restart.
    Stopped,
}

impl Session {
    pub(super) fn screen(&self) -> Screen {
        if !self.shell.is_stopped() {
            Screen::Auth
        } else if self.auth.retains_account() {
            Screen::Recovery
        } else {
            Screen::Stopped
        }
    }

    /// The recovery view's banner: honest about a logout core stopped in.
    pub(super) fn stopped_banner(&self) -> &'static str {
        match self.auth {
            AuthView::LogoutStarting => chat_copy::STOPPED_SIGNING_OUT,
            AuthView::LogoutNotStarted { .. } => chat_copy::STOPPED_SIGN_OUT_NOT_STARTED,
            _ => chat_copy::CORE_STOPPED_COPY,
        }
    }
}

fn session_body(
    session: &Session,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let screen = match session.screen() {
        Screen::Auth => auth_screen(session, theme, cx),
        Screen::Recovery => recovery_view(session, theme, cx),
        Screen::Stopped => centered()
            .child(quiet_status("stopped", chat_copy::STOPPED_RESTART, theme))
            .into_any_element(),
    };
    let live = session.screen() == Screen::Auth;
    // The Logout control of the signed-in shell (plan section 17.3); none
    // once core stopped (it needs core).
    let logout = (live && session.auth == AuthView::SignedIn).then(|| {
        div().absolute().top_2().right_4().child(on(
            secondary("logout", copy::onboarding::LOGOUT, theme),
            Session::logout,
            cx,
        ))
    });
    let confirm =
        (live && session.confirming_logout()).then(|| logout_confirm(session, theme, window, cx));
    let menu = (live && session.auth == AuthView::SignedIn && confirm.is_none())
        .then(|| super::folder_menu::menu_overlay(session, theme, window, cx))
        .flatten();
    let credential = live
        .then(|| credential_banner(session, theme, cx))
        .flatten();
    let base = div()
        .size_full()
        .relative()
        .id("session")
        .when(live && session.auth != AuthView::SignedIn, |body| {
            body.role(Role::Group)
                .aria_label("Telegram sign-in")
                .track_focus(&session.auth_focus)
        })
        .child(screen)
        .children(credential)
        .children(logout)
        .children(
            (live && !session.discarded.is_empty())
                .then(|| discarded_notice(&session.discarded, theme, cx)),
        )
        .into_any_element();
    div()
        .size_full()
        .relative()
        .child(behind_overlay(
            "session-body",
            base,
            confirm.is_some() || menu.is_some(),
        ))
        .children(confirm)
        .children(menu)
        .children(session_notice(session, theme))
        .into_any_element()
}

/// Core stopped with account state kept (task 6m): the chat shell, local
/// only, under a banner. Replaces a logout overlay, whose actions need core.
fn recovery_view(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .w_full()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(theme.line(theme.text))
                .child(quiet_status(
                    "stopped-banner",
                    session.stopped_banner(),
                    theme,
                )),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(chat_shell(session, theme, cx)),
        )
        .into_any_element()
}

/// A raised panel over the base surface: outlined, never a second color.
fn raised(el: Div, theme: &Theme) -> Div {
    el.bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .border_1()
        .border_color(theme.line(theme.text))
        .rounded(px(theme::RADIUS))
}

/// The credential item's failed save or delete, with Retry (finding 6).
fn credential_banner(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Option<Div> {
    let text = copy::onboarding::credential_item(session.cred_item)?;
    let retry = super::onboarding::credential_retry(session.cred_item).map(|_| {
        on(
            secondary("credential-retry", copy::onboarding::RETRY, theme),
            Session::retry_credential_item,
            cx,
        )
    });
    let banner = div()
        .absolute()
        .top_2()
        .left_4()
        .flex()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .child(a11y::alert("credential-item-error", text))
        .children(retry);
    Some(raised(banner, theme))
}

/// Logout deletes local data, so it asks first.
fn logout_confirm(
    session: &Session,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<'_, Root>,
) -> Div {
    let cancel = on(
        secondary("logout-cancel", copy::CANCEL, theme),
        Session::cancel_logout,
        cx,
    );
    let confirm = on(
        button("logout-confirm", copy::onboarding::LOGOUT, theme),
        Session::confirm_logout,
        cx,
    );
    let group = &session.logout_focus.group;
    group.enter_after_draw(window, cx);
    let content = group
        .frame(a11y::labelled(
            raised(card(), theme),
            "logout-dialog",
            Role::Dialog,
            copy::LOGOUT_CONFIRM_TITLE,
        ))
        .key_context("Overlay")
        .a11y_synthetic_children(|builder| builder.parent_node().set_modal())
        .on_action(cx.listener(|root, _: &super::DismissOverlay, window, cx| {
            root.with_session(|s, cx| s.cancel_logout(window, cx), cx);
        }))
        .child(title(copy::LOGOUT_CONFIRM_TITLE))
        .child(
            div()
                .opacity(theme.quiet_opacity())
                .child(a11y::text("logout-detail", copy::LOGOUT_CONFIRM_DETAIL)),
        )
        .child(footer(None, vec![cancel, confirm]))
        .child(group.end());
    modal(content, theme)
}

/// "Logout incomplete": the local deletion failed; Retry runs it again.
fn logout_incomplete_screen(
    remote: abyss_core::api::RemoteLogout,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    use abyss_core::api::RemoteLogout;
    let remote = matches!(remote, RemoteLogout::Failed(_) | RemoteLogout::TimedOut).then(|| {
        div()
            .opacity(theme.quiet_opacity())
            .child(text!(copy::LOGOUT_REMOTE_UNCONFIRMED))
    });
    let retry = on(
        auth_button("retry-logout", copy::onboarding::RETRY, theme),
        Session::retry_logout,
        cx,
    );
    page(
        card()
            .child(title(copy::LOGOUT_INCOMPLETE_TITLE))
            .child(error_line(copy::LOGOUT_INCOMPLETE_DETAIL, theme))
            .children(remote)
            .child(footer(None, vec![retry])),
    )
    .into_any_element()
}

/// "Couldn't start logout": nothing changed. Retry writes the marker again;
/// the user's own logout may stay signed in instead (core's Back).
fn logout_not_started_screen(
    session: &Session,
    trigger: abyss_core::api::LogoutTrigger,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    use abyss_core::api::LogoutTrigger;
    let detail = match trigger {
        LogoutTrigger::User => copy::LOGOUT_NOT_STARTED_USER,
        LogoutTrigger::Server => copy::LOGOUT_NOT_STARTED_SERVER,
    };
    let stay = (back(&session.auth, session.source) == Back::Core).then(|| {
        on(
            secondary("stay-signed-in", copy::STAY_SIGNED_IN, theme),
            Session::back,
            cx,
        )
    });
    let retry = on(
        auth_button("retry-logout", copy::onboarding::RETRY, theme),
        Session::retry_logout,
        cx,
    );
    page(
        card()
            .child(title(copy::LOGOUT_NOT_STARTED_TITLE))
            .child(error_line(detail, theme))
            .child(footer(stay, vec![retry])),
    )
    .into_any_element()
}

/// "Couldn't read Keychain" at startup: Retry reads the marker again.
fn startup_blocked_screen(theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let retry = on(
        auth_button("retry-startup", copy::onboarding::RETRY, theme),
        Session::retry_startup,
        cx,
    );
    page(
        card()
            .child(title(copy::STARTUP_BLOCKED_TITLE))
            .child(error_line(copy::STARTUP_BLOCKED_DETAIL, theme))
            .child(footer(None, vec![retry])),
    )
    .into_any_element()
}

/// The auth router: one arm per state, each delegating to its own screen
/// function. The onboarding screens live in [`onboarding`].
fn auth_screen(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    use copy::onboarding as text;
    match &session.auth {
        AuthView::Connecting => connecting_screen(theme),
        AuthView::Onboarding(step) => onboarding::screen(session, step, theme, cx),
        AuthView::Phone { error } => phone_screen(session, *error, theme, cx),
        AuthView::Code {
            phone_hint,
            sent,
            error,
        } => onboarding::code_screen(session, phone_hint, sent.as_ref(), *error, theme, cx),
        AuthView::Password { hint, error } => {
            password_screen(session, hint.as_deref(), *error, theme, cx)
        }
        AuthView::FloodWait { until, .. } => flood_wait_screen(session, *until, theme, cx),
        AuthView::Verifying { error } => onboarding::verifying_screen(*error, theme),
        AuthView::SignUpRequired => onboarding::dead_end_screen(
            session,
            text::SIGN_UP_TITLE,
            text::SIGN_UP_DETAIL,
            None,
            theme,
            cx,
        ),
        AuthView::OfficialAppRequired { url } => onboarding::dead_end_screen(
            session,
            text::OFFICIAL_APP_TITLE,
            text::OFFICIAL_APP_DETAIL,
            url.as_deref().map(text::official_app_url),
            theme,
            cx,
        ),

        AuthView::LoggingOut | AuthView::LogoutStarting => onboarding::logging_out_screen(theme),
        AuthView::LogoutIncomplete { remote } => logout_incomplete_screen(*remote, theme, cx),
        AuthView::LogoutNotStarted { trigger } => {
            logout_not_started_screen(session, *trigger, theme, cx)
        }
        AuthView::StartupBlocked => startup_blocked_screen(theme, cx),
        AuthView::Done {
            has_password,
            saved,
        } => onboarding::done_screen(*has_password, *saved, theme, cx),
        AuthView::SignedIn => chat_shell(session, theme, cx),
    }
}

fn connecting_screen(theme: &Theme) -> AnyElement {
    centered()
        .child(title(copy::CONNECTING).opacity(theme.quiet_opacity()))
        .into_any_element()
}

fn phone_screen(
    session: &Session,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PHONE_TITLE,
        detail: copy::PHONE_DETAIL.into(),
        input: session.phone.clone().into_any_element(),
        extra: Vec::new(),
        error,
        submit: Session::submit_phone,
        back: back_button(session, theme, cx),
    };
    form.render(theme, cx)
}

fn password_screen(
    session: &Session,
    hint: Option<&str>,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PASSWORD_TITLE,
        detail: copy::password_detail(hint),
        input: session.password.clone().into_any_element(),
        extra: Vec::new(),
        error,
        submit: Session::submit_password,
        back: back_button(session, theme, cx),
    };
    form.render(theme, cx)
}

/// An input's frame: the same wash as an incoming message.
pub(super) fn field(input: AnyElement, border: gpui::Rgba, theme: &Theme) -> Stateful<Div> {
    div()
        .id("folder-name-frame")
        .w_full()
        .px_3()
        .py_2()
        .border_2()
        .border_color(border)
        .rounded(px(theme::RADIUS))
        .bg(theme.incoming)
        .text_color(theme.on_incoming)
        .child(input)
}

/// A primary action, in the theme's `accent`.
pub(super) fn button(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    auth_button(id, label, theme)
}

fn auth_button(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    let button = a11y::focus_ring(a11y::button(id, label), theme.on_accent);
    primary_style(button, theme)
}

pub(super) fn primary_style(el: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    el.px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .bg(theme.accent)
        .text_color(theme.on_accent)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
}

/// The lesser action beside a [`button`]: an outline, no fill.
fn secondary(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    let button = a11y::focus_ring(a11y::button(id, label), theme.text);
    secondary_style(button, theme)
}

fn secondary_style(el: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    el.px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .border_color(theme.line(theme.text))
        .cursor_pointer()
}

/// Wires an accessible control to a session action.
fn on(el: Stateful<Div>, action: SubmitFn, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let root = cx.weak_entity();
    a11y::on_activate(el, move |window, cx| {
        root.update(cx, |root, cx| {
            root.with_session(|s, cx| action(s, window, cx), cx)
        })
        .ok();
    })
}

/// Back, where the screen has one (plan section 17.3).
fn back_button(
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Option<Stateful<Div>> {
    match back(&session.auth, session.source) {
        Back::None => None,
        Back::Local(_) | Back::Core => Some(on(
            secondary("back", copy::onboarding::BACK, theme),
            Session::back,
            cx,
        )),
    }
}

/// A footer row: Back (if any) on the left, the actions on the right.
fn footer(back: Option<Stateful<Div>>, actions: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .justify_between()
        .gap_2()
        .child(div().children(back))
        .child(div().flex().gap_2().children(actions))
}

/// An inline error. No color marks it (the theme has none to spare): a rule
/// and a heavier weight do.
fn error_line(text: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    a11y::alert("inline-error", text)
        .pl_2()
        .border_l_2()
        .border_color(theme.text)
        .font_weight(FontWeight::MEDIUM)
}

/// The mark, the wordmark and the tagline, above each sign-in step.
pub(super) fn brand(theme: &Theme) -> Div {
    div()
        .pb_4()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(mark(theme, 34.))
                .child(
                    div()
                        .font_family(theme::NAME_FONT)
                        .text_3xl()
                        .child(chat_copy::WORDMARK),
                ),
        )
        .child(
            div()
                .opacity(theme.quiet_opacity())
                .child(text!(copy::TAGLINE)),
        )
}

/// A login step: heading, explanation, one input, extra lines, inline
/// error, Back and Continue.
struct LoginForm {
    heading: &'static str,
    detail: SharedString,
    input: AnyElement,
    extra: Vec<AnyElement>,
    error: Option<UserError>,
    submit: SubmitFn,
    back: Option<Stateful<Div>>,
}

impl LoginForm {
    fn render(self, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
        let error = self.error.map(|e| error_line(copy::user_error(e), theme));
        let submit = on(
            auth_button("continue", copy::CONTINUE, theme),
            self.submit,
            cx,
        );
        page(
            card()
                .child(brand(theme))
                .child(title(self.heading))
                .child(
                    div()
                        .opacity(theme.quiet_opacity())
                        .child(text!(self.detail)),
                )
                .child(self.input)
                .children(self.extra)
                .children(error)
                .child(footer(self.back, vec![submit]))
                .child(
                    div()
                        .opacity(theme.quiet_opacity())
                        .text_xs()
                        .pt_4()
                        .child(text!(copy::DESCRIPTOR)),
                ),
        )
        .into_any_element()
    }
}

fn flood_wait_screen(
    session: &Session,
    until: Instant,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
    page(
        card()
            .child(title(copy::FLOOD_TITLE))
            .child(
                div()
                    .opacity(theme.quiet_opacity())
                    .child(text!(copy::flood_detail(left))),
            )
            .child(footer(back_button(session, theme, cx), Vec::new())),
    )
    .into_any_element()
}

fn session_notice(session: &Session, theme: &Theme) -> Option<Div> {
    let text = session
        .writes_cancelled
        .then(|| copy::user_error(UserError::FolderWritesCancelled))
        .or_else(|| session.notice.clone())?;
    let content = if session.screen() == Screen::Auth && session.auth != AuthView::SignedIn {
        a11y::alert("notice", text)
    } else {
        a11y::status("notice", text)
    }
    .into_any_element();
    Some(toast(content, theme))
}

/// A passing notice: outlined, on the base surface, quieter than a message.
fn toast(content: AnyElement, theme: &Theme) -> Div {
    div()
        .absolute()
        .bottom_4()
        .left_0()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .px_4()
                .py_2()
                .rounded_full()
                .border_1()
                .border_color(theme.line(theme.text))
                .bg(theme.surface(Depth::NOW))
                .text_color(theme.text)
                .child(content),
        )
}

/// Batches of discarded drafts the notice lists.
const DISCARDED_LINES: usize = 3;

/// Discarded drafts (their threads can no longer be sent to), with Copy
/// and Dismiss: above the toast, and kept until one of them.
fn discarded_notice(
    discarded: &crate::chat::discarded::Discarded,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Div {
    // The newest few batches; Copy takes every text.
    let lines = discarded.lines().take(DISCARDED_LINES).cloned();
    let notice = div()
        .flex()
        .gap_3()
        .items_center()
        .px_4()
        .py_2()
        .occlude()
        .child(
            div().flex().flex_col().children(
                lines
                    .enumerate()
                    .map(|(i, line)| a11y::status(("discarded", i), line)),
            ),
        )
        .child(on(
            secondary("discarded-copy", chat_copy::COPY_TEXT, theme),
            Session::copy_discarded,
            cx,
        ))
        .child(on(
            secondary("discarded-dismiss", chat_copy::DISMISS, theme),
            Session::dismiss_discarded,
            cx,
        ));
    div()
        .absolute()
        .bottom_16()
        .left_0()
        .w_full()
        .flex()
        .justify_center()
        .child(raised(notice, theme))
}

/// The closing overlay's dialog. It hides the window's body from assistive
/// technology (`behind_overlay`), so it must be a node of its own, and the
/// one place focus can be (`Root::overlay_focus`).
fn closing_dialog(
    focus: &a11y::OverlayFocus,
    label: &'static str,
    content: Div,
    closing: Closing,
    window: &Window,
    cx: &mut App,
) -> Stateful<Div> {
    // Without a control, focus stays on the dialog; stepping into it would
    // redraw for ever.
    if closing != Closing::Waiting {
        focus.group.enter_after_draw(window, cx);
    }
    focus
        .group
        .frame(a11y::labelled(
            content,
            "closing-dialog",
            Role::Dialog,
            label,
        ))
        .a11y_synthetic_children(|builder| builder.parent_node().set_modal())
        .child(focus.group.end())
}

/// A blocking notice with a Quit button: a dialog named by its heading,
/// whose detail is an alert read on arrival.
fn quit_card(heading: &'static str, detail: &'static str, theme: &Theme) -> Div {
    card()
        .child(title(heading))
        .child(a11y::alert("closing-detail", detail).opacity(theme.quiet_opacity()))
        .child(
            div().flex().justify_end().child(
                button("quit", copy::QUIT, theme).on_click(|_: &ClickEvent, _, cx| cx.quit()),
            ),
        )
}

fn closing_overlay(
    closing: Closing,
    focus: &a11y::OverlayFocus,
    theme: &Theme,
    window: &Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let (label, content) = match closing {
        Closing::Open => return None,
        Closing::Waiting => (
            copy::CLOSING,
            card()
                .items_center()
                .child(heading_style(a11y::status("closing-status", copy::CLOSING))),
        ),
        Closing::Unconfirmed => (
            copy::UNCONFIRMED_TITLE,
            quit_card(copy::UNCONFIRMED_TITLE, copy::UNCONFIRMED_DETAIL, theme),
        ),
        Closing::LogoutUnfinished => (
            copy::LOGOUT_UNFINISHED_TITLE,
            quit_card(
                copy::LOGOUT_UNFINISHED_TITLE,
                copy::LOGOUT_UNFINISHED_DETAIL,
                theme,
            ),
        ),
    };
    let dialog = closing_dialog(focus, label, content, closing, window, cx);
    Some(modal(dialog, theme).into_any_element())
}

/// A layer over the window that blocks the content under it.
fn modal(content: impl IntoElement, theme: &Theme) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .bg(theme::faded(theme.surface(Depth::NOW), 0.9))
        .flex()
        .justify_center()
        .items_center()
        .child(content)
}
