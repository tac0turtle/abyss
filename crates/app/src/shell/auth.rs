//! The onboarding and login screens' view of core's auth state (plan
//! sections 4 and 17.3, UI side).
//!
//! Core's [`AuthState`] is the source of truth; [`project`] maps it onto the
//! screens the view draws. The first-run screens before credential entry
//! (welcome, the my.telegram.org guide) are UI-only steps of
//! `NeedCredentials`; [`back`] says what Back does on each screen.

use std::time::{Duration, Instant, SystemTime};

use abyss_core::api::{
    AuthState, CodeDelivery, CredentialError, CredentialSource, LocalDeletion, LogoutTrigger,
    NewLogin, NextDelivery, RemoteLogout, SentCodeInfo, UserError,
};
use gpui::SharedString;

use crate::ui::copy;

/// The credential screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Onboarding {
    Welcome,
    /// Getting `api_id`/`api_hash` on my.telegram.org.
    Guide,
    /// Entering the pair. `stored`: a pair is saved in the Keychain.
    Entry {
        error: Option<CredentialError>,
        stored: Stored,
    },
    /// The env override is in control and unusable; entry is disabled.
    Env {
        error: CredentialError,
    },
    /// The Keychain could not be read: retry, never re-onboard.
    Keychain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Yes,
    No,
}

/// Where the code went, for the code screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SentView {
    pub(crate) delivery: CodeDelivery,
    /// Digits in the code, when the server said (a missed call: how many
    /// trailing digits of the calling number).
    pub(crate) length: Option<u8>,
    /// The most digits the code field takes for this delivery.
    pub(crate) max_digits: usize,
    pub(crate) next: Option<NextDelivery>,
    pub(crate) resend_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthView {
    Connecting,
    Onboarding(Onboarding),
    Phone {
        error: Option<UserError>,
    },
    Code {
        phone_hint: SharedString,
        sent: Option<SentView>,
        error: Option<UserError>,
    },
    Password {
        hint: Option<SharedString>,
        error: Option<UserError>,
    },
    FloodWait {
        until: Instant,
        resume: Box<AuthView>,
    },
    /// An ambiguous sign-in is being checked.
    Verifying {
        error: Option<UserError>,
    },
    SignUpRequired,
    /// Finish in an official app. A Fragment number's address is shown as
    /// text only; abyss never opens it.
    OfficialAppRequired {
        url: Option<SharedString>,
    },
    LoggingOut,
    /// A logout's marker is being written: "Logging out" over the account's
    /// UI, which is kept (nothing has changed yet) but not shown, so no
    /// send, retry or selection is made meanwhile (review round 25).
    LogoutStarting,
    /// The logout's local deletion failed: "Logout incomplete" with Retry.
    /// Core accepts no login until the retry succeeds.
    LogoutIncomplete {
        remote: RemoteLogout,
    },
    /// The logout could not start (its marker was not written), so nothing
    /// changed: Retry, or, for the user's own logout, stay signed in.
    LogoutNotStarted {
        trigger: LogoutTrigger,
    },
    /// Startup couldn't read the logout marker: Retry reads it again. Never
    /// "Logout incomplete": nothing is pending as far as anyone knows.
    StartupBlocked,
    /// A login finished in this run: tips, then the chats.
    Done {
        has_password: Option<bool>,
        saved: Saved,
    },
    SignedIn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Saved {
    Yes,
    No,
}

/// One core auth state, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Projection {
    pub(crate) view: AuthView,
    /// Shown as a toast, for states without a screen of their own.
    pub(crate) notice: Option<SharedString>,
    /// The password token is gone: the next submission asks core for new
    /// parameters instead of sending the password.
    pub(crate) needs_params: bool,
}

impl Projection {
    fn view(view: AuthView) -> Self {
        Self {
            view,
            notice: None,
            needs_params: false,
        }
    }

    fn with_notice(view: AuthView, notice: SharedString) -> Self {
        Self {
            view,
            notice: Some(notice),
            needs_params: false,
        }
    }
}

fn instant_of(at: SystemTime, now: Instant) -> Instant {
    now + at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// `NeedCredentials` onto a credential screen. Without an error or a saved
/// pair, the UI-only step is kept (or, coming back from the phone screen,
/// entry; on first launch, the welcome).
fn credentials(
    error: Option<CredentialError>,
    source: Option<CredentialSource>,
    prev: &AuthView,
) -> Onboarding {
    let stored = if source == Some(CredentialSource::Stored) {
        Stored::Yes
    } else {
        Stored::No
    };
    match error {
        Some(
            error @ (CredentialError::EnvPartial { .. } | CredentialError::EnvMalformed { .. }),
        ) => Onboarding::Env { error },
        Some(error) if source == Some(CredentialSource::Env) => Onboarding::Env { error },
        Some(CredentialError::KeychainUnavailable) => Onboarding::Keychain,
        Some(error) => Onboarding::Entry {
            error: Some(error),
            stored,
        },
        None => match prev {
            AuthView::Onboarding(step @ (Onboarding::Welcome | Onboarding::Guide))
                if stored == Stored::No =>
            {
                step.clone()
            }
            AuthView::Connecting if stored == Stored::No => Onboarding::Welcome,
            _ => Onboarding::Entry {
                error: None,
                stored,
            },
        },
    }
}

fn signed_in(display: Option<NewLogin>, saved: Saved) -> Projection {
    match (display, saved) {
        (Some(login), _) => Projection::view(AuthView::Done {
            has_password: login.has_password,
            saved,
        }),
        (None, Saved::Yes) => Projection::view(AuthView::SignedIn),
        (None, Saved::No) => {
            Projection::with_notice(AuthView::SignedIn, copy::SIGNED_IN_UNSAVED.into())
        }
    }
}

/// Maps core's state onto a screen. `prev` is the screen shown now: it
/// keeps the password hint for `PasswordTokenUnavailable` (which carries
/// none) and the UI-only onboarding step.
pub(crate) fn project(state: AuthState, prev: &AuthView, now: Instant) -> Projection {
    match state {
        AuthState::Connecting => Projection::view(AuthView::Connecting),
        AuthState::NeedCredentials { error, source } => {
            Projection::view(AuthView::Onboarding(credentials(error, source, prev)))
        }
        AuthState::Phone { error } => Projection::view(AuthView::Phone { error }),
        AuthState::Code {
            phone_hint,
            sent,
            error,
        } => Projection::view(AuthView::Code {
            phone_hint: phone_hint.into(),
            sent: sent.map(|s: SentCodeInfo| SentView {
                max_digits: s.max_code_digits(),
                length: s.length,
                delivery: s.delivery,
                next: s.next,
                resend_at: s.resend_at.map(|at| instant_of(at, now)),
            }),
            error,
        }),
        AuthState::Password { hint, error } => Projection::view(AuthView::Password {
            hint: hint.map(Into::into),
            error,
        }),
        AuthState::PasswordTokenUnavailable { error } => {
            let hint = match prev {
                AuthView::Password { hint, .. } => hint.clone(),
                _ => None,
            };
            Projection {
                view: AuthView::Password {
                    hint,
                    error: Some(error),
                },
                notice: Some(copy::PASSWORD_PARAMS.into()),
                needs_params: true,
            }
        }
        AuthState::FloodWait { until, resume } => {
            let resume = project(*resume, prev, now).view;
            Projection::view(AuthView::FloodWait {
                until: instant_of(until, now),
                resume: Box::new(resume),
            })
        }
        AuthState::Verifying { error } => Projection::view(AuthView::Verifying { error }),
        AuthState::SignUpRequired => Projection::view(AuthView::SignUpRequired),
        AuthState::OfficialAppRequired { url } => Projection::view(AuthView::OfficialAppRequired {
            url: url.map(Into::into),
        }),
        AuthState::SignedIn { new_login, .. } => signed_in(new_login, Saved::Yes),
        AuthState::SignedInUnsaved { new_login, .. } => signed_in(new_login, Saved::No),
        AuthState::LoggingOut => Projection::view(AuthView::LoggingOut),
        AuthState::LogoutStarting => Projection::view(AuthView::LogoutStarting),
        AuthState::LoggedOut {
            remote,
            local: LocalDeletion::Failed(_),
        } => Projection::view(AuthView::LogoutIncomplete { remote }),
        AuthState::LoggedOut { remote, local } => {
            Projection::with_notice(AuthView::Connecting, copy::logged_out(remote, local))
        }
        AuthState::LogoutNotStarted { trigger, .. } => {
            Projection::view(AuthView::LogoutNotStarted { trigger })
        }
        AuthState::StartupBlocked { .. } => Projection::view(AuthView::StartupBlocked),
    }
}

/// What Back does on a screen (plan section 17.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Back {
    /// No Back on this screen.
    None,
    /// A UI-only step back.
    Local(AuthView),
    /// Core's `Command::Back`: it supersedes the attempt.
    Core,
}

/// Back is allowed only before the session is authorized, and never away
/// from an env or embedded pair.
pub(crate) fn back(view: &AuthView, source: Option<CredentialSource>) -> Back {
    let fixed_pair = matches!(
        source,
        Some(CredentialSource::Env | CredentialSource::Embedded)
    );
    match view {
        AuthView::Onboarding(Onboarding::Guide) => {
            Back::Local(AuthView::Onboarding(Onboarding::Welcome))
        }
        AuthView::Onboarding(Onboarding::Entry { .. }) => {
            Back::Local(AuthView::Onboarding(Onboarding::Guide))
        }
        AuthView::Phone { .. } if fixed_pair => Back::None,
        AuthView::FloodWait { resume, .. } if matches!(**resume, AuthView::Verifying { .. }) => {
            Back::None
        }
        AuthView::Phone { .. }
        | AuthView::Code { .. }
        | AuthView::Password { .. }
        | AuthView::FloodWait { .. }
        | AuthView::SignUpRequired
        | AuthView::OfficialAppRequired { .. }
        // "Stay signed in" (core's Back).
        | AuthView::LogoutNotStarted {
            trigger: LogoutTrigger::User,
        } => Back::Core,
        AuthView::Onboarding(
            Onboarding::Welcome | Onboarding::Env { .. } | Onboarding::Keychain,
        )
        | AuthView::Connecting
        | AuthView::Verifying { .. }
        | AuthView::LoggingOut
        | AuthView::LogoutStarting
        | AuthView::LogoutIncomplete { .. }
        | AuthView::LogoutNotStarted {
            trigger: LogoutTrigger::Server,
        }
        | AuthView::StartupBlocked
        | AuthView::Done { .. }
        | AuthView::SignedIn => Back::None,
    }
}

impl AuthView {
    /// Shows `error` on the current login screen, if it belongs there. A
    /// flood wait becomes a countdown that resumes this screen.
    pub(crate) fn with_error(&self, error: UserError, now: Instant) -> Option<AuthView> {
        if let UserError::FloodWait { seconds } = error
            && self.is_login_form()
        {
            return Some(AuthView::FloodWait {
                until: now + Duration::from_secs(u64::from(seconds)),
                resume: Box::new(self.clone()),
            });
        }
        let mut next = self.clone();
        match &mut next {
            AuthView::Phone { error: slot }
            | AuthView::Code { error: slot, .. }
            | AuthView::Password { error: slot, .. } => *slot = Some(error),
            _ => return None,
        }
        Some(next)
    }

    fn is_login_form(&self) -> bool {
        matches!(
            self,
            AuthView::Phone { .. } | AuthView::Code { .. } | AuthView::Password { .. }
        )
    }

    /// The same screen without its error, as shown again after a retry.
    pub(crate) fn cleared(&self) -> AuthView {
        let mut next = self.clone();
        if let AuthView::Phone { error }
        | AuthView::Code { error, .. }
        | AuthView::Password { error, .. } = &mut next
        {
            *error = None;
        }
        next
    }

    /// The account is being torn down (a logout began, or one is stuck):
    /// the UI forgets everything of it (review round 15, B). A logout that
    /// could not start changed nothing, so it keeps the account's UI; a
    /// blocked startup has loaded nothing, so it has nothing to keep.
    pub(crate) fn tears_down(&self) -> bool {
        matches!(
            self,
            AuthView::LoggingOut | AuthView::LogoutIncomplete { .. } | AuthView::StartupBlocked
        )
    }

    /// A teardown that ends the account: a logout, not a startup block,
    /// after which the same account may resume.
    pub(crate) fn ends_account(&self) -> bool {
        matches!(
            self,
            AuthView::LoggingOut | AuthView::LogoutIncomplete { .. }
        )
    }

    /// Whether the account's chat state (echoes, drafts) is kept behind
    /// this screen: signed in, a login's tips, or a logout overlay over a
    /// kept account. Torn-down and signed-out screens keep none (task 6m).
    pub(crate) fn retains_account(&self) -> bool {
        matches!(
            self,
            AuthView::SignedIn
                | AuthView::Done { .. }
                | AuthView::LogoutStarting
                | AuthView::LogoutNotStarted { .. }
        )
    }

    /// A logout overlay hides the account's UI, which is kept: its live
    /// tail is not on screen (review round 28).
    pub(crate) fn hides_account(&self) -> bool {
        matches!(
            self,
            AuthView::LogoutStarting | AuthView::LogoutNotStarted { .. }
        )
    }

    /// A countdown is running on this screen (flood wait, code resend).
    pub(crate) fn ticking_until(&self) -> Option<Instant> {
        match self {
            AuthView::FloodWait { until, .. } => Some(*until),
            AuthView::Code {
                sent:
                    Some(SentView {
                        resend_at: Some(at),
                        ..
                    }),
                ..
            } => Some(*at),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::api::{EnvVar, LocalDeletion, RemoteLogout};

    use super::*;

    fn need(error: Option<CredentialError>, source: Option<CredentialSource>) -> AuthState {
        AuthState::NeedCredentials { error, source }
    }

    fn onboarding(state: AuthState, prev: &AuthView) -> Onboarding {
        match project(state, prev, Instant::now()).view {
            AuthView::Onboarding(step) => step,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors_land_on_the_current_form() {
        let now = Instant::now();
        let phone = AuthView::Phone { error: None };
        assert_eq!(
            phone.with_error(UserError::Rejected, now),
            Some(AuthView::Phone {
                error: Some(UserError::Rejected)
            })
        );
        match phone.with_error(UserError::FloodWait { seconds: 30 }, now) {
            Some(AuthView::FloodWait { until, resume }) => {
                assert_eq!(until - now, Duration::from_secs(30));
                assert_eq!(*resume, phone);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(AuthView::SignedIn.with_error(UserError::Busy, now), None);
    }

    /// The router follows each `AuthState` (plan section 17.5, app).
    #[test]
    fn the_router_follows_each_state() {
        let now = Instant::now();
        let at = |s: AuthState| project(s, &AuthView::Connecting, now).view;
        assert_eq!(at(AuthState::Connecting), AuthView::Connecting);
        assert_eq!(
            at(AuthState::Phone { error: None }),
            AuthView::Phone { error: None }
        );
        assert_eq!(
            at(AuthState::Verifying { error: None }),
            AuthView::Verifying { error: None }
        );
        assert_eq!(at(AuthState::SignUpRequired), AuthView::SignUpRequired);
        assert_eq!(
            at(AuthState::OfficialAppRequired { url: None }),
            AuthView::OfficialAppRequired { url: None }
        );
        assert_eq!(
            at(AuthState::OfficialAppRequired {
                url: Some("https://fragment.com/x".into())
            }),
            AuthView::OfficialAppRequired {
                url: Some("https://fragment.com/x".into())
            }
        );
        assert_eq!(at(AuthState::LoggingOut), AuthView::LoggingOut);
        let first = NewLogin {
            has_password: Some(false),
        };
        assert_eq!(
            at(AuthState::SignedIn {
                display_name: "A".into(),
                new_login: Some(first),
            }),
            AuthView::Done {
                has_password: Some(false),
                saved: Saved::Yes
            }
        );
        assert_eq!(
            at(AuthState::SignedIn {
                display_name: "A".into(),
                new_login: None,
            }),
            AuthView::SignedIn
        );
        let unsaved = project(
            AuthState::SignedInUnsaved {
                display_name: "A".into(),
                new_login: None,
            },
            &AuthView::Connecting,
            now,
        );
        assert_eq!(unsaved.view, AuthView::SignedIn);
        assert!(unsaved.notice.is_some());
        let gone = AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            local: LocalDeletion::Deleted,
        };
        assert!(project(gone, &AuthView::LoggingOut, now).notice.is_some());
    }

    /// Task 6c: a failed local deletion is never shown as "Signed out".
    #[test]
    fn failed_local_deletion_is_logout_incomplete() {
        let failed = AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            local: LocalDeletion::Failed(abyss_core::api::StorageError::Internal),
        };
        let p = project(failed, &AuthView::LoggingOut, Instant::now());
        assert_eq!(
            p.view,
            AuthView::LogoutIncomplete {
                remote: RemoteLogout::Revoked
            }
        );
        assert_eq!(p.notice, None);
        let text = crate::ui::copy::logged_out(
            RemoteLogout::Revoked,
            LocalDeletion::Failed(abyss_core::api::StorageError::Internal),
        );
        assert!(text.starts_with("Logout incomplete"), "{text}");
        assert!(!text.contains("Signed out"), "{text}");
    }

    #[test]
    fn credential_states_pick_their_screen() {
        let start = AuthView::Connecting;
        assert_eq!(onboarding(need(None, None), &start), Onboarding::Welcome);
        let guide = AuthView::Onboarding(Onboarding::Guide);
        assert_eq!(onboarding(need(None, None), &guide), Onboarding::Guide);
        // Back from the phone screen lands on entry.
        let phone = AuthView::Phone { error: None };
        assert_eq!(
            onboarding(need(None, None), &phone),
            Onboarding::Entry {
                error: None,
                stored: Stored::No
            }
        );
        assert_eq!(
            onboarding(need(None, Some(CredentialSource::Stored)), &start),
            Onboarding::Entry {
                error: None,
                stored: Stored::Yes
            }
        );
        let partial = CredentialError::EnvPartial {
            missing: EnvVar::ApiHash,
        };
        assert_eq!(
            onboarding(need(Some(partial), Some(CredentialSource::Env)), &start),
            Onboarding::Env { error: partial }
        );
        let invalid = CredentialError::ApiIdInvalid;
        assert_eq!(
            onboarding(need(Some(invalid), Some(CredentialSource::Env)), &start),
            Onboarding::Env { error: invalid }
        );
        assert_eq!(
            onboarding(need(Some(invalid), Some(CredentialSource::Entered)), &start),
            Onboarding::Entry {
                error: Some(invalid),
                stored: Stored::No
            }
        );
        assert_eq!(
            onboarding(
                need(
                    Some(CredentialError::KeychainUnavailable),
                    Some(CredentialSource::Stored)
                ),
                &start
            ),
            Onboarding::Keychain
        );
    }

    #[test]
    fn back_rules() {
        let entered = Some(CredentialSource::Entered);
        let env = Some(CredentialSource::Env);
        assert_eq!(
            back(&AuthView::Onboarding(Onboarding::Welcome), None),
            Back::None
        );
        assert_eq!(
            back(&AuthView::Onboarding(Onboarding::Guide), None),
            Back::Local(AuthView::Onboarding(Onboarding::Welcome))
        );
        let entry = AuthView::Onboarding(Onboarding::Entry {
            error: None,
            stored: Stored::No,
        });
        assert_eq!(
            back(&entry, None),
            Back::Local(AuthView::Onboarding(Onboarding::Guide))
        );
        let phone = AuthView::Phone { error: None };
        assert_eq!(back(&phone, entered), Back::Core);
        assert_eq!(back(&phone, env), Back::None, "an env pair stays");
        let code = AuthView::Code {
            phone_hint: "x".into(),
            sent: None,
            error: None,
        };
        assert_eq!(back(&code, env), Back::Core);
        let official = AuthView::OfficialAppRequired { url: None };
        assert_eq!(back(&official, entered), Back::Core);
        // Only before the session is authorized.
        for view in [
            AuthView::SignedIn,
            AuthView::Done {
                has_password: None,
                saved: Saved::Yes,
            },
            AuthView::LoggingOut,
            AuthView::Verifying { error: None },
        ] {
            assert_eq!(back(&view, entered), Back::None, "{view:?}");
        }
        let verifying_wait = AuthView::FloodWait {
            until: Instant::now(),
            resume: Box::new(AuthView::Verifying { error: None }),
        };
        assert_eq!(back(&verifying_wait, entered), Back::None);
    }

    #[test]
    fn token_unavailable_keeps_the_hint_and_asks_for_params() {
        let state = AuthState::PasswordTokenUnavailable {
            error: UserError::Network,
        };
        let prev = AuthView::Password {
            hint: Some("h".into()),
            error: None,
        };
        let p = project(state, &prev, Instant::now());
        assert!(p.needs_params);
        assert_eq!(
            p.view,
            AuthView::Password {
                hint: Some("h".into()),
                error: Some(UserError::Network)
            }
        );
    }

    #[test]
    fn code_screen_keeps_delivery_and_resend_countdown() {
        let now = Instant::now();
        let state = AuthState::Code {
            phone_hint: "+44 •••• ••12".into(),
            sent: Some(SentCodeInfo {
                delivery: CodeDelivery::Sms,
                length: Some(5),
                next: Some(NextDelivery::Call),
                resend_at: Some(SystemTime::now() + Duration::from_secs(60)),
            }),
            error: None,
        };
        let view = project(state, &AuthView::Connecting, now).view;
        let until = view.ticking_until().unwrap();
        assert!(until - now <= Duration::from_secs(60));
        assert!(until - now > Duration::from_secs(50));
        let AuthView::Code {
            sent: Some(sent), ..
        } = view
        else {
            panic!("{view:?}");
        };
        assert_eq!(sent.delivery, CodeDelivery::Sms);
        assert_eq!(sent.next, Some(NextDelivery::Call));
        assert_eq!(sent.max_digits, 5);
    }

    #[test]
    fn flood_wait_counts_down_to_the_resume_screen() {
        let now = Instant::now();
        let state = AuthState::FloodWait {
            until: SystemTime::now() + Duration::from_secs(60),
            resume: Box::new(AuthState::Phone { error: None }),
        };
        match project(state, &AuthView::Connecting, now).view {
            AuthView::FloodWait { until, resume } => {
                assert!(until - now <= Duration::from_secs(60));
                assert!(until - now > Duration::from_secs(50));
                assert_eq!(*resume, AuthView::Phone { error: None });
            }
            other => panic!("{other:?}"),
        }
    }
}
