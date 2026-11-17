//! Rendering for the main window, in the tonal theme (`docs/brand/brand.md`):
//! one hue, no accent color, emphasis by contrast only.

use std::time::Instant;

use abyss_core::api::UserError;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px,
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
use crate::ui::copy;
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Mode, Theme, Themes};

mod onboarding;

type SubmitFn = fn(&mut Session, &mut Window, &mut Context<'_, Root>);

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if let State::Running(session) = &mut self.state {
            // The sidebar's rows are cached: a folder's expired mutes are
            // rechecked here, at most once a minute (plan section 17.7).
            session.shell.list.recheck(crate::chat::format::now());
        }
        let theme = &Themes::of(window, cx);
        let (body, overlay) = match &self.state {
            State::Failed(failure) => (failure_screen(failure).into_any_element(), None),
            State::Running(session) => (
                session_body(session, theme, cx),
                closing_overlay(session.closing, theme),
            ),
        };
        let panel = self
            .appearance_open
            .then(|| appearance::panel(theme, Mode::of(window), Themes::settings(cx), cx));
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.surface(Depth::NOW))
            .text_color(theme.text)
            .font_family(theme::BODY_FONT)
            .text_size(px(14.))
            .on_action(cx.listener(|root, _: &super::ForumBack, window, cx| {
                root.with_session(|s, cx| s.back_from_forum(window, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::NextTab, window, cx| {
                root.with_session(|s, cx| s.step_tab(Step::Next, window, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::PreviousTab, window, cx| {
                root.with_session(|s, cx| s.step_tab(Step::Previous, window, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::NextRow, window, cx| {
                root.with_session(|s, cx| s.step_row(Step::Next, window, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::PreviousRow, window, cx| {
                root.with_session(|s, cx| s.step_row(Step::Previous, window, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::ExpandForum, _, cx| {
                root.with_session(|s, cx| s.set_inline(Inline::Expanded, cx), cx);
            }))
            .on_action(cx.listener(|root, _: &super::CollapseForum, _, cx| {
                root.with_session(|s, cx| s.set_inline(Inline::Collapsed, cx), cx);
            }))
            .child(body)
            .children(panel)
            .children(overlay)
    }
}

/// A card screen: scrolls when its content is taller than the window, and
/// is centered when it fits (task 12: the guide was cut off, buttons
/// unreachable, on a short window).
pub(super) fn page(content: impl IntoElement) -> AnyElement {
    div()
        .id("page")
        .size_full()
        .overflow_y_scroll()
        .child(
            div()
                .w_full()
                .min_h_full()
                .flex()
                .flex_col()
                .justify_center()
                .items_center()
                .py_4()
                .child(content),
        )
        .into_any_element()
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

pub(super) fn title(text: impl Into<SharedString>) -> Div {
    div()
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// Quiet text: whatever color it inherits, at reduced strength.
pub(super) fn muted(text: impl Into<SharedString>) -> Div {
    div().opacity(theme::QUIET).child(text.into())
}

fn failure_screen(failure: &Failure) -> impl IntoElement {
    let (heading, lines) = match failure {
        Failure::Launch(error) => copy::launch_error(error),
        Failure::Start(error) => copy::start_error(error),
    };
    page(
        card()
            .w(px(560.))
            .child(title(heading))
            .children(lines.into_iter().map(muted)),
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

fn session_body(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let screen = match session.screen() {
        Screen::Auth => auth_screen(session, theme, cx),
        Screen::Recovery => recovery_view(session, theme, cx),
        Screen::Stopped => centered()
            .child(muted(chat_copy::STOPPED_RESTART))
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
        (live && session.confirming_logout()).then(|| logout_confirm(theme, cx).into_any_element());
    let credential = live
        .then(|| credential_banner(session, theme, cx))
        .flatten();
    div()
        .size_full()
        .relative()
        .child(screen)
        .children(credential)
        .children(logout)
        .children(confirm)
        .children(
            (live && session.auth == AuthView::SignedIn)
                .then(|| super::folder_menu::menu_overlay(session, theme, cx))
                .flatten(),
        )
        .children(
            (live && !session.discarded.is_empty())
                .then(|| discarded_notice(&session.discarded, theme, cx)),
        )
        .children(
            session
                .writes_cancelled
                .then(|| copy::user_error(UserError::FolderWritesCancelled))
                .or_else(|| session.notice.clone())
                .map(|text| toast(text, theme)),
        )
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
                .border_color(theme::line(theme.text))
                .child(muted(session.stopped_banner())),
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
        .border_color(theme::line(theme.text))
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
        .child(div().child(text))
        .children(retry);
    Some(raised(banner, theme))
}

/// Logout deletes local data, so it asks first.
fn logout_confirm(theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
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
    let content = card()
        .child(title(copy::LOGOUT_CONFIRM_TITLE))
        .child(muted(copy::LOGOUT_CONFIRM_DETAIL))
        .child(footer(None, vec![cancel, confirm]));
    modal(raised(content, theme), theme)
}

/// "Logout incomplete": the local deletion failed; Retry runs it again.
fn logout_incomplete_screen(
    remote: abyss_core::api::RemoteLogout,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    use abyss_core::api::RemoteLogout;
    let remote = matches!(remote, RemoteLogout::Failed(_) | RemoteLogout::TimedOut)
        .then(|| muted(copy::LOGOUT_REMOTE_UNCONFIRMED));
    let retry = on(
        button("retry-logout", copy::onboarding::RETRY, theme),
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
        button("retry-logout", copy::onboarding::RETRY, theme),
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
        button("retry-startup", copy::onboarding::RETRY, theme),
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
        AuthView::Connecting => connecting_screen(),
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

        AuthView::LoggingOut | AuthView::LogoutStarting => onboarding::logging_out_screen(),
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

fn connecting_screen() -> AnyElement {
    centered().child(muted(copy::CONNECTING)).into_any_element()
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
pub(super) fn field(input: AnyElement, theme: &Theme) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2()
        .border_1()
        .border_color(theme::line(theme.text))
        .rounded(px(theme::RADIUS))
        .bg(theme.incoming)
        .text_color(theme.text)
        .child(input)
}

/// The strongest fill on screen, the same as the user's own messages.
pub(super) fn button(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .bg(theme.text)
        .text_color(theme.on_text)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label)
}

/// The lesser action beside a [`button`]: an outline, no fill.
fn secondary(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .border_1()
        .border_color(theme::line(theme.text))
        .cursor_pointer()
        .child(label)
}

/// Wires a button to a session action.
fn on(el: Stateful<Div>, action: SubmitFn, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    el.on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| action(s, window, cx), cx);
    }))
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
fn error_line(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .pl_2()
        .border_l_2()
        .border_color(theme.text)
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
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
        .child(muted(copy::TAGLINE))
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
        let submit = on(button("continue", copy::CONTINUE, theme), self.submit, cx);
        page(
            card()
                .child(brand(theme))
                .child(title(self.heading))
                .child(muted(self.detail))
                .child(field(self.input, theme))
                .children(self.extra)
                .children(error)
                .child(footer(self.back, vec![submit]))
                .child(muted(copy::DESCRIPTOR).text_xs().pt_4()),
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
            .child(muted(copy::flood_detail(left)))
            .child(footer(back_button(session, theme, cx), Vec::new())),
    )
    .into_any_element()
}

/// A passing notice: outlined, on the base surface, quieter than a message.
fn toast(text: SharedString, theme: &Theme) -> Div {
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
                .border_color(theme::line(theme.text))
                .bg(theme.surface(Depth::NOW))
                .text_color(theme.text)
                .child(text),
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
        .child(div().flex().flex_col().children(lines))
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

/// A blocking notice with a Quit button.
fn quit_card(heading: &'static str, detail: &'static str, theme: &Theme) -> Div {
    card().child(title(heading)).child(muted(detail)).child(
        div()
            .flex()
            .justify_end()
            .child(button("quit", copy::QUIT, theme).on_click(|_: &ClickEvent, _, cx| cx.quit())),
    )
}

fn closing_overlay(closing: Closing, theme: &Theme) -> Option<AnyElement> {
    let content = match closing {
        Closing::Open => return None,
        Closing::Waiting => card().items_center().child(title(copy::CLOSING)),
        Closing::Unconfirmed => quit_card(copy::UNCONFIRMED_TITLE, copy::UNCONFIRMED_DETAIL, theme),
        Closing::LogoutUnfinished => quit_card(
            copy::LOGOUT_UNFINISHED_TITLE,
            copy::LOGOUT_UNFINISHED_DETAIL,
            theme,
        ),
    };
    Some(modal(content, theme).into_any_element())
}

/// A layer over the window that blocks the content under it.
fn modal(content: Div, theme: &Theme) -> Div {
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
