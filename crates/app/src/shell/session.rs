//! A running core and the UI state built from its events.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use abyss_core::api::{
    AuthState, Command, ConnState, CredentialItem, CredentialSource, Event, MAX_CODE_DIGITS,
    PasswordBuf, UserError,
};
use abyss_core::{BackendChoice, CoreConfig, StartError};
use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
};

use super::Root;
use super::auth::{AuthView, project};
use super::bridge::{CoreLink, EventSource, SharedCore, spawn_pump};
use super::shutdown::{self, Closing};
use crate::chat::discarded::Discarded;
use crate::chat::sidebar_file::SidebarFile;
use crate::chat::{ChatShell, Refused};
use crate::launch::Launch;
use crate::ui::copy;
use crate::ui::password::{PasswordEvent, PasswordInput};
use crate::ui::text_input::{Limits, TextInput, TextInputEvent, text_input};

/// How long a notice (toast) stays up.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// A started core, split into its command and event halves.
pub(crate) struct Started {
    pub(super) commands: Box<dyn CoreLink>,
    pub(super) events: Box<dyn EventSource>,
    /// Keeps the fake's update channel open; dropping it ends core's update
    /// stream.
    #[cfg(feature = "fake")]
    pub(super) fake: Option<abyss_core::backend::fake::FakeHandle>,
}

impl Started {
    fn new(core: abyss_core::CoreHandle) -> Self {
        let (commands, events) = core.split();
        Self {
            commands: Box::new(commands),
            events: Box::new(events),
            #[cfg(feature = "fake")]
            fake: None,
        }
    }
}

pub(crate) fn start(
    launch: Launch,
    secret_storage: Result<abyss_core::secret::SecretStorageConfig, StartError>,
    notification_cleanup: abyss_core::events::NotificationCleanupPolicy,
) -> Result<Started, StartError> {
    match launch {
        #[cfg(feature = "fake")]
        Launch::Fake {
            faults,
            start,
            cache,
        } => {
            use abyss_core::backend::fake::{FakeBackend, FakeScript};
            let mut script = crate::chat::demo::script(crate::chat::format::now());
            if start == crate::launch::FakeStart::Onboarding {
                // No credentials: core starts from `NeedCredentials`.
                script.env = abyss_core::credentials::EnvResolution::Absent;
            }
            // The demo account's login: code `FAKE_CODE`, 2FA password with a hint.
            script.login = FakeScript::demo(0, 0).login;
            let (backend, handle) = FakeBackend::new(script);
            // Opt-in only (`ABYSS_FAKE_CACHE=1`), never the real cache.
            let config = CoreConfig {
                fake_faults: faults,
                secret_storage: None,
                notification_cleanup,
                cache: cache.and_then(|location| crate::storage::config(Ok(location))),
            };
            let core = abyss_core::start(config, BackendChoice::Fake(backend))?;
            Ok(Started {
                fake: Some(handle),
                ..Started::new(core)
            })
        }
        Launch::Telegram { env, cache } => {
            use abyss_core::events::NotificationCleanupPolicy::{Disabled, Unavailable};
            use abyss_core::secret::KeychainNamespace;

            let storage = secret_storage?;
            let notification_cleanup = match (notification_cleanup, storage.keychain_namespace) {
                (Disabled, KeychainNamespace::Bundle) => Unavailable,
                (policy, _) => policy,
            };
            // `core::start` initializes SQLCipher and the exit hook before
            // anything opens; a failed directory runs without a cache.
            let config = CoreConfig {
                cache: crate::storage::config(cache),
                secret_storage: Some(storage),
                notification_cleanup,
                #[cfg(feature = "fake")]
                fake_faults: abyss_core::FakeFaults::default(),
            };
            let core = abyss_core::start(config, BackendChoice::Grammers(env))?;
            Ok(Started::new(core))
        }
    }
}

pub(crate) struct Session {
    pub(super) core: SharedCore,
    pub(super) auth: AuthView,
    /// Core said the password token is gone: the next submission asks for new
    /// parameters first.
    needs_params: bool,
    pub(super) phone: Entity<TextInput>,
    pub(super) code: Entity<TextInput>,
    pub(super) password: Entity<PasswordInput>,
    /// Credential entry (plan section 17.3). The `api_hash` is a masked
    /// secret field, like the password.
    pub(super) api_id: Entity<TextInput>,
    pub(super) api_hash: Entity<PasswordInput>,
    /// Log out was pressed and waits for confirmation.
    logout_prompt: bool,
    /// The appearance panel is showing. `Root` owns the panel, which also
    /// opens on screens with no session; it mirrors the flag here
    /// (`Root::set_appearance`) so focus moves made by the session
    /// know the panel covers it.
    settings: SettingsPanel,
    /// An event or timer moved the screen on while an overlay covered it,
    /// and left focus where it was. The last overlay's close completes the
    /// move ([`Session::overlay_closed`]).
    focus: FocusDebt,
    pub(super) logout_focus: crate::ui::a11y::OverlayFocus,
    /// Inline validation of the credential entry.
    pub(super) entry_error: Option<SharedString>,
    /// Which credential source is in control (core's `CredentialSource`).
    pub(super) source: Option<CredentialSource>,
    /// The stored credential item's save or delete (core's
    /// `CredentialItem`): a failure shows Retry.
    pub(super) cred_item: CredentialItem,
    pub(super) composer: Entity<TextInput>,
    /// Drafts discarded because their thread can no longer be sent to,
    /// offered for copying until dismissed (`chat::discarded`).
    pub(super) discarded: Discarded,
    /// The chat shell's own focus, for a pane without a composer (a topic
    /// list): key bindings such as Cmd+[ need a focused element on screen.
    pub(super) pane_focus: FocusHandle,
    /// The start of the tab order on auth screens without an editor.
    pub(super) auth_focus: FocusHandle,
    /// A new chat folder's title (`folder_menu`).
    pub(super) folder_name: Entity<TextInput>,
    /// The open archive/folder menu, if any (`folder_menu`).
    pub(super) menu: Option<super::folder_menu::Menu>,
    pub(super) menu_focus: crate::ui::a11y::OverlayFocus,
    pub(super) tabs: super::tabs::Tabs,
    /// Focus and scrolling of the sidebar's rows (`shell::nav`).
    pub(super) nav: super::nav::Nav,
    /// Chat list, open chat and pending sends (`crate::chat`, `shell::chats`).
    pub(super) shell: ChatShell,
    pub(super) conn: Option<ConnState>,
    pub(super) notice: Option<SharedString>,
    notice_gen: u64,
    /// A logout cancelled waiting archive moves or folder edits: told for as
    /// long as any logout screen shows ("Logging out", "Couldn't start
    /// logout", the logout itself, "Logout incomplete"), then as a passing
    /// notice once the logout is over ("Stay signed in", or it finished).
    pub(super) writes_cancelled: bool,
    pub(super) closing: Closing,
    _pump: Task<()>,
    #[cfg(feature = "fake")]
    fake: Option<abyss_core::backend::fake::FakeHandle>,
    _subscriptions: [Subscription; 7],
    _chat_wiring: [Subscription; 2],
}

impl Session {
    pub(super) fn can_hide(&self) -> bool {
        self.closing == Closing::Open
            && !self.shell.is_stopped()
            && matches!(self.auth, AuthView::SignedIn | AuthView::Done { .. })
    }

    pub(crate) fn new(started: Started, window: &mut Window, cx: &mut Context<'_, Root>) -> Self {
        let shared: SharedCore = Rc::new(RefCell::new(started.commands));
        let phone = login_field(
            Limits::PHONE,
            copy::PHONE_LABEL,
            copy::PHONE_PLACEHOLDER,
            cx,
        );
        let code = code_field(cx);
        let composer = text_input(Limits::COMPOSER, copy::COMPOSER_PLACEHOLDER, cx);
        let (folder_name, folder_sub) = super::folder_menu::title_field(window, cx);
        let password = cx.new(|cx| PasswordInput::new(window, cx));
        let api_id = login_field(
            Limits::API_ID,
            copy::onboarding::API_ID_LABEL,
            copy::onboarding::API_ID_PLACEHOLDER,
            cx,
        );
        let api_hash = cx.new(|cx| PasswordInput::api_hash(window, cx));
        let [id_sub, hash_sub] = credential_subscriptions(&api_id, &api_hash, window, cx);
        let subscriptions = subscriptions(
            [id_sub, hash_sub, folder_sub],
            &phone,
            &code,
            &password,
            &composer,
            window,
            cx,
        );
        let mut shell = ChatShell::default();
        shell.list.restore(SidebarFile::load(cx));
        let chat_wiring = super::chats::wire(window, cx);
        let mut session = Self {
            core: shared,
            auth: AuthView::Connecting,
            needs_params: false,
            phone,
            code,
            password,
            api_id,
            api_hash,
            logout_prompt: false,
            settings: SettingsPanel::Closed,
            focus: FocusDebt::None,
            logout_focus: crate::ui::a11y::OverlayFocus::new(cx),
            entry_error: None,
            source: None,
            cred_item: CredentialItem::Settled,
            composer,
            discarded: Discarded::default(),
            pane_focus: cx.focus_handle(),
            auth_focus: cx.focus_handle().tab_index(-1),
            folder_name,
            menu: None,
            menu_focus: crate::ui::a11y::OverlayFocus::new(cx),
            tabs: super::tabs::Tabs::default(),
            nav: super::nav::Nav::default(),
            shell,
            conn: None,
            notice: None,
            notice_gen: 0,
            writes_cancelled: false,
            closing: Closing::Open,
            _pump: spawn_pump(started.events, window, cx),
            #[cfg(feature = "fake")]
            fake: started.fake,
            _subscriptions: subscriptions,
            _chat_wiring: chat_wiring,
        };
        session.focus_for_auth(window, cx);
        session
    }

    /// Queues a command. A full queue shows "busy"; the rejected command is
    /// dropped (a password in it is zeroized on drop). Chat commands go
    /// through `shell::chats`, which changes the UI only once one is admitted.
    pub(super) fn send(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<'_, Root>) {
        if let Err(refused) = super::chats::admit(&self.core, cmd) {
            self.show_refused(refused, window, cx);
        }
    }

    /// Queues an auth command; the caller changes the UI only on `Ok`, so a
    /// refused command changes nothing and the user can submit again (as
    /// the chat commands do, task 10b). Refused, it shows "busy".
    pub(super) fn admit_auth(
        &mut self,
        cmd: Command,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) -> Result<(), Refused> {
        let admitted = super::chats::admit(&self.core, cmd);
        if let Err(refused) = admitted {
            self.show_refused(refused, window, cx);
        }
        admitted
    }

    /// Whether the logout confirmation is up. It belongs to the signed-in
    /// screens: on any other it is not shown, owns no focus and covers
    /// nothing, whatever the flag says (review N55).
    pub(super) fn confirming_logout(&self) -> bool {
        self.logout_prompt && matches!(self.auth, AuthView::SignedIn | AuthView::Done { .. })
    }

    /// Closes the logout confirmation through the one close path: its focus
    /// is released and focus owed after it is paid. A no-op when none is open.
    pub(super) fn close_logout_prompt(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !self.logout_prompt {
            return;
        }
        self.logout_prompt = false;
        self.logout_focus.close(window, cx);
        self.overlay_closed(window, cx);
        cx.notify();
    }

    pub(super) fn open_logout_prompt(&mut self) {
        self.logout_prompt = true;
    }

    pub(super) fn show_notice(
        &mut self,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.notice = Some(text);
        self.notice_gen = self.notice_gen.wrapping_add(1);
        let generation = self.notice_gen;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR).await;
            this.update(cx, |root, cx| {
                root.with_session(|s, _| s.expire_notice(generation), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn expire_notice(&mut self, generation: u64) {
        if self.notice_gen == generation {
            self.notice = None;
        }
    }

    pub(super) fn set_auth(
        &mut self,
        next: AuthView,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.wipe_left_secrets(&next, window, cx);
        let ticking = next.ticking_until().is_some() && self.auth.ticking_until().is_none();
        if next.tears_down() {
            let kept = self.shell.list.kept_view();
            self.reset_account(window, cx);
            if next.ends_account() {
                // The next account starts on All.
                SidebarFile::remove(cx);
            } else {
                // A startup block proves no logout: the same account may
                // resume, on its kept tab (the file still holds it).
                self.shell.list.restore(kept);
            }
        } else if next.hides_account() {
            self.shell.hidden();
        }
        if let AuthView::Code { sent, .. } = &next {
            let max = sent.as_ref().map_or(MAX_CODE_DIGITS, |s| s.max_digits);
            self.code.update(cx, |input, cx| input.set_max_len(max, cx));
        }
        self.auth = next;
        self.close_overlays_of_left_screens(window, cx);
        if ticking {
            spawn_ticker(window, cx);
        }
        self.focus_for_auth(window, cx);
        cx.notify();
    }

    /// A secret never outlives its screen (review N59): the password when
    /// leaving the password screen, the `api_hash` (and its reveal) when
    /// leaving the credential entry, the login code when leaving the code
    /// screen.
    fn wipe_left_secrets(
        &mut self,
        next: &AuthView,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        use super::auth::Onboarding;
        if matches!(self.auth, AuthView::Password { .. })
            && !matches!(next, AuthView::Password { .. })
        {
            self.password.update(cx, PasswordInput::clear);
        }
        if matches!(self.auth, AuthView::Onboarding(Onboarding::Entry { .. }))
            && !matches!(next, AuthView::Onboarding(Onboarding::Entry { .. }))
        {
            self.api_hash.update(cx, PasswordInput::clear);
        }
        if matches!(self.auth, AuthView::Code { .. }) && !matches!(next, AuthView::Code { .. }) {
            self.code.update(cx, |input, cx| input.clear(window, cx));
        }
    }

    /// The context menu and the logout confirmation belong to the signed-in
    /// screens (the menu is drawn on `SignedIn` only, the confirmation on it
    /// and `Done`). When the screen moves on, as on a revoked session, they
    /// close through the normal paths: focus released, owed focus paid, and
    /// nothing left that owns Tab without being on screen (review N55).
    fn close_overlays_of_left_screens(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth != AuthView::SignedIn {
            self.close_menu(window, cx);
        }
        if !matches!(self.auth, AuthView::SignedIn | AuthView::Done { .. }) {
            self.close_logout_prompt(window, cx);
        }
    }

    /// The account's teardown began (review round 15, B): nothing of it
    /// stays on screen or in a field. The single entry point for the chat
    /// state is `ChatShell::reset`.
    fn reset_account(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.shell.reset();
        self.composer
            .update(cx, |input, cx| input.clear(window, cx));
        // The next account starts with no credential or code typed for this
        // one (N59). The menu and the confirmation close with the screen
        // (`close_overlays_of_left_screens`).
        self.api_hash.update(cx, PasswordInput::clear);
        self.code.update(cx, |input, cx| input.clear(window, cx));
        self.discarded.clear();
    }

    /// Called every second while a countdown runs (a flood wait, a code
    /// resend). Returns whether to keep ticking.
    pub(super) fn tick(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) -> bool {
        let Some(until) = self.auth.ticking_until() else {
            return false;
        };
        if Instant::now() < until {
            cx.notify();
            return true;
        }
        if let AuthView::FloodWait { resume, .. } = &self.auth {
            let resume = (**resume).clone();
            self.set_auth(resume, window, cx);
        }
        cx.notify();
        false
    }

    /// Whether an overlay owns keyboard focus. The same order as
    /// `Root::overlay_focus`, which cannot be asked from inside the session.
    pub(super) fn overlay_open(&self) -> bool {
        self.pane_covered() || self.menu.is_some()
    }

    /// Whether a modal overlay covers the chat pane: the closing overlay,
    /// Settings, the logout confirmation. Each is a near-opaque veil over
    /// the whole window, so what the pane paints behind it is not seen: the
    /// frame observer marks nothing read and reports no live tail, and a
    /// banner for the open chat is not suppressed (review A64, N57). A
    /// context menu covers a few rows and leaves the transcript readable, so
    /// it is not one.
    pub(super) fn pane_covered(&self) -> bool {
        self.closing != Closing::Open
            || self.settings == SettingsPanel::Open
            || self.confirming_logout()
    }

    pub(super) fn set_settings(&mut self, panel: SettingsPanel) {
        self.settings = panel;
    }

    /// Moves focus to `handle` for something an event or a timer did, not
    /// the user. An open overlay owns focus until it closes, and typing must
    /// not reach a field behind it (review A56): the move waits for
    /// [`Self::overlay_closed`].
    pub(super) fn move_focus(
        &mut self,
        handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if self.overlay_open() {
            self.focus = FocusDebt::Owed;
        } else {
            window.focus(handle, cx);
        }
    }

    /// An overlay closed. Once none is left, a screen change it covered
    /// takes focus now: the control that had focus before the overlay opened
    /// may no longer be on screen.
    pub(super) fn overlay_closed(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.focus == FocusDebt::Owed && !self.overlay_open() {
            self.focus = FocusDebt::None;
            self.focus_for_auth(window, cx);
        }
    }

    fn focus_for_auth(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        use super::auth::Onboarding;
        let handle = match self.auth {
            AuthView::Onboarding(Onboarding::Entry { .. }) => self.api_id.focus_handle(cx),
            AuthView::Phone { .. } => self.phone.focus_handle(cx),
            AuthView::Code { .. } => self.code.focus_handle(cx),
            AuthView::Password { .. } => self.password.focus_handle(cx),
            // The message box if the pane renders one, else the shell's own
            // focus, so its chords (Ctrl+Tab) work before any chat is open.
            AuthView::SignedIn => self.pane_focus_target(cx),
            _ => self.auth_focus.clone(),
        };
        self.move_focus(&handle, window, cx);
    }

    pub(super) fn submit_phone(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Phone { .. }) {
            return;
        }
        let phone = self.phone.read(cx).text().to_owned();
        if self
            .admit_auth(Command::SubmitPhone(phone), window, cx)
            .is_ok()
        {
            self.auth = self.auth.cleared();
        }
        cx.notify();
    }

    /// The code leaves the field only once core accepted it.
    pub(super) fn submit_code(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Code { .. }) {
            return;
        }
        let code = self.code.read(cx).text().to_owned();
        if self
            .admit_auth(Command::SubmitCode(code), window, cx)
            .is_ok()
        {
            self.code.update(cx, |input, cx| input.clear(window, cx));
            self.auth = self.auth.cleared();
        }
        cx.notify();
    }

    /// Sends the password, or, when core has no password token, asks for new
    /// parameters and leaves the typed password in place for the next Enter.
    pub(super) fn submit_password(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Password { .. }) {
            return;
        }
        if self.needs_params {
            if self
                .admit_auth(Command::RetryPasswordParams, window, cx)
                .is_ok()
            {
                self.needs_params = false;
                self.auth = self.auth.cleared();
            }
            cx.notify();
            return;
        }
        // A copy: the field keeps the password until core accepted it.
        let secret = self.password.read(cx).secret();
        let buf = PasswordBuf::from_slice(&secret);
        drop(secret);
        if let Ok(buf) = buf {
            // A refused command is dropped, zeroizing the copy.
            if self
                .admit_auth(Command::SubmitPassword(buf), window, cx)
                .is_ok()
            {
                self.password.update(cx, PasswordInput::clear);
                self.auth = self.auth.cleared();
            }
        } else if let Some(next) = self
            .auth
            .with_error(UserError::InvalidPassword, Instant::now())
        {
            // Longer than any password Telegram accepts.
            self.set_auth(next, window, cx);
        }
        cx.notify();
    }

    pub(super) fn apply_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        match event {
            Event::Auth(state) => self.on_auth(state, window, cx),
            Event::CredentialSource(source) => self.source = source,
            Event::CredentialItem(item) => self.cred_item = item,
            Event::Connection(state) => {
                self.conn = Some(state);
                // Reconnection restarts retries that gave up (`shell::chats`).
                self.apply_chat_event(Event::Connection(state), window, cx);
            }
            Event::Error(error) => self.on_error(error, window, cx),
            Event::Notify(_)
            | Event::ClearNotifications(_)
            | Event::NotifyDeleted { .. }
            | Event::InboxRead { .. } => {}
            // Chat list, message view and sends: `shell::chats`.
            chat => self.apply_chat_event(chat, window, cx),
        }
    }

    fn on_auth(&mut self, state: AuthState, window: &mut Window, cx: &mut Context<'_, Root>) {
        #[cfg(feature = "fake")]
        let fake_code = self.fake.is_some() && matches!(state, AuthState::Code { error: None, .. });
        let projection = project(state, &self.auth, Instant::now());
        self.needs_params = projection.needs_params;
        self.set_auth(projection.view, window, cx);
        // Kept up until the logout is over: it either finished (a login
        // screen) or did not happen ("Stay signed in").
        let logging_out = matches!(
            self.auth,
            AuthView::LogoutStarting
                | AuthView::LogoutNotStarted { .. }
                | AuthView::LoggingOut
                | AuthView::LogoutIncomplete { .. }
        );
        let mut notice = projection.notice;
        if self.writes_cancelled && !logging_out {
            // Told now, after the logout's own notice if it has one.
            self.writes_cancelled = false;
            let cancelled = copy::user_error(UserError::FolderWritesCancelled);
            notice = Some(match notice {
                Some(first) => format!("{first} {cancelled}").into(),
                None => cancelled,
            });
        }
        if let Some(notice) = notice {
            self.show_notice(notice, window, cx);
        }
        #[cfg(feature = "fake")]
        if fake_code {
            self.show_notice(copy::FAKE_CODE_HINT.into(), window, cx);
        }
    }

    fn on_error(&mut self, error: UserError, window: &mut Window, cx: &mut Context<'_, Root>) {
        // Errors never end a load: core says which ended (`LoadEnded`).
        tracing::info!(?error, "core reported an error");
        if error == UserError::FolderWritesCancelled {
            // Kept up through the logout screen (`on_auth`).
            self.writes_cancelled = true;
            cx.notify();
            return;
        }
        match self.auth.with_error(error, Instant::now()) {
            Some(next) => self.set_auth(next, window, cx),
            None => self.show_notice(copy::user_error(error), window, cx),
        }
    }

    /// Starts shutdown: asks core to stop and waits for its ack in the
    /// background. Idempotent.
    pub(super) fn begin_shutdown(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.closing != Closing::Open {
            return;
        }
        self.closing = Closing::Waiting;
        self.password.update(cx, PasswordInput::clear);
        self.api_hash.update(cx, PasswordInput::clear);
        self.code.update(cx, |input, cx| input.clear(window, cx));
        let ack = self
            .core
            .try_borrow_mut()
            .ok()
            .and_then(|mut core| core.begin_shutdown());
        let timer = cx.background_executor().timer(shutdown::UI_TIMEOUT);
        tracing::info!("shutdown requested");
        cx.spawn_in(window, async move |this, cx| {
            let outcome = shutdown::await_ack(ack, timer).await;
            tracing::info!(?outcome, "shutdown finished");
            this.update(cx, |root, cx| {
                root.finish_shutdown(shutdown::next(outcome), cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Backstop for OS-initiated termination: asks core to stop and returns its
    /// ack, `None` if shutdown already began (the UI holds that ack). Never
    /// waits.
    pub(super) fn cancel_core(&self) -> Option<super::bridge::ShutdownAck> {
        self.core.try_borrow_mut().ok()?.begin_shutdown()
    }
}

impl Session {
    /// Copies every discarded draft, each under its label, and closes
    /// their notice.
    pub(super) fn copy_discarded(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.discarded.is_empty() {
            return;
        }
        let text = self.discarded.copy_text();
        self.discarded.clear();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.show_notice(crate::chat::copy::TEXT_COPIED.into(), window, cx);
    }

    /// Closes the discarded drafts' notice; their texts are gone.
    pub(super) fn dismiss_discarded(&mut self, _: &mut Window, cx: &mut Context<'_, Root>) {
        self.discarded.clear();
        cx.notify();
    }
}

/// Whether the appearance panel is showing, as `Root` tells the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsPanel {
    Open,
    Closed,
}

/// Whether a focus move was left undone because an overlay covered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusDebt {
    None,
    Owed,
}

fn login_field(
    limits: Limits,
    label: &'static str,
    placeholder: &'static str,
    cx: &mut Context<'_, Root>,
) -> Entity<TextInput> {
    cx.new(|cx| TextInput::new(limits, placeholder, cx).labelled(label))
}

/// The login code is a live credential: accessibility reads bullets (review A3).
fn code_field(cx: &mut Context<'_, Root>) -> Entity<TextInput> {
    cx.new(|cx| {
        TextInput::new(Limits::CODE, copy::CODE_PLACEHOLDER, cx)
            .labelled(copy::CODE_LABEL)
            .secret()
    })
}

fn subscriptions(
    [id_sub, hash_sub, folder_sub]: [Subscription; 3],
    phone: &Entity<TextInput>,
    code: &Entity<TextInput>,
    password: &Entity<PasswordInput>,
    composer: &Entity<TextInput>,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> [Subscription; 7] {
    [
        id_sub,
        hash_sub,
        cx.subscribe_in(phone, window, |root, _, _: &TextInputEvent, window, cx| {
            root.with_session(|s, cx| s.submit_phone(window, cx), cx);
        }),
        cx.subscribe_in(code, window, |root, _, _: &TextInputEvent, window, cx| {
            root.with_session(|s, cx| s.submit_code(window, cx), cx);
        }),
        cx.subscribe_in(
            password,
            window,
            |root, _, _: &PasswordEvent, window, cx| {
                root.with_session(|s, cx| s.submit_password(window, cx), cx);
            },
        ),
        cx.subscribe_in(
            composer,
            window,
            |root, _, _: &TextInputEvent, window, cx| {
                root.with_session(|s, cx| s.submit_message(window, cx), cx);
            },
        ),
        folder_sub,
    ]
}

/// Enter in either credential field submits both.
fn credential_subscriptions(
    api_id: &Entity<TextInput>,
    api_hash: &Entity<PasswordInput>,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> [Subscription; 2] {
    let id_sub = cx.subscribe_in(api_id, window, |root, _, _: &TextInputEvent, window, cx| {
        root.with_session(|s, cx| s.submit_credentials(window, cx), cx);
    });
    let hash_sub = cx.subscribe_in(
        api_hash,
        window,
        |root, _, _: &PasswordEvent, window, cx| {
            root.with_session(|s, cx| s.submit_credentials(window, cx), cx);
        },
    );
    [id_sub, hash_sub]
}

/// Re-renders a countdown every second until it ends.
fn spawn_ticker(window: &mut Window, cx: &mut Context<'_, Root>) {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let more = this.update_in(cx, |root, window, cx| {
                root.with_session(|s, cx| s.tick(window, cx), cx)
            });
            if more.ok().flatten() != Some(true) {
                return;
            }
        }
    })
    .detach();
}
