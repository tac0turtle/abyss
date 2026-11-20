//! The macOS notification center and its delegate.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::{DynBlock, RcBlock};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFNumber, CFType};
use objc2_foundation::{
    NSArray, NSBundle, NSDictionary, NSError, NSNumber, NSString, NSUserDefaults, ns_string,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSetting, UNNotificationSettings,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use super::{
    Authorization, BadgePreference, Banner, Center, Cleared, ClickQueue, Delivery, MAX_TRACKED,
    Notifications, Payload, Receipt, SystemEvent, SystemEvents, SystemSender, SystemSettings,
    channels,
};

use crate::storage::APP_ID as BUNDLE_ID;

const BADGE_KEY: &str = "showDockBadge";

struct MacCenter {
    center: Retained<UNUserNotificationCenter>,
    // The system's delegate property is weak.
    _delegate: Retained<Delegate>,
    events: SystemSender,
}

define_class!(
    // SAFETY: NSObject is a valid subclass base. The delegate has only
    // thread-safe Rust ivars and comes from alloc/init below.
    #[unsafe(super(NSObject))]
    #[name = "AbyssNotificationDelegate"]
    #[ivars = ClickQueue]
    struct Delegate;

    // SAFETY: All NSObject protocol methods are inherited from NSObject.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: The two callbacks use the protocol's specified signatures
    // and complete exactly once. They do not touch GPUI or AppKit.
    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _: &UNUserNotificationCenter,
            _: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            // SAFETY: This framework constant is a process-lifetime, immutable NSString.
            let default_action = unsafe { UNNotificationDefaultActionIdentifier };
            if &*response.actionIdentifier() == default_action {
                let info = response.notification().request().content().userInfo();
                // The main thread: never waits, and keeps the latest click.
                self.ivars().push(decode(&info));
            }
            completion.call(());
        }
    }
);

impl Delegate {
    fn new(clicks: ClickQueue) -> Retained<Self> {
        let this = Self::alloc().set_ivars(clicks);
        // SAFETY: Initializes this allocated NSObject subclass through its superclass.
        unsafe { msg_send![super(this), init] }
    }
}

/// Sends a completion the app's bookkeeping depends on: the enumeration of
/// delivered notifications (review A38), the answer to a delivery request
/// (A40), the settings read and the authorization reply (A53). None can be
/// repeated by the app: a lost authorization reply leaves the prompt in
/// flight and hides "Enable notifications" until restart. So they wait for
/// room instead of being dropped when the queue is full.
///
/// The system calls them on its own queues, which the main thread never
/// waits on, so waiting cannot deadlock. On the main thread (not expected)
/// waiting could, so it falls back to a send that may drop. Clicks are not
/// sent here: they arrive on the main thread and have their own queue
/// ([`ClickQueue`]).
fn send_reliably(events: &SystemSender, event: SystemEvent) {
    if MainThreadMarker::new().is_some() {
        let _ = events.try_send(event);
    } else {
        // Err only when the app dropped its receiver: nothing is left to tell.
        let _ = events.send_blocking(event);
    }
}

/// The system answered the authorization request, granted or not (the
/// settings read that follows says which).
fn authorization_finished(events: &SystemSender) {
    send_reliably(events, SystemEvent::AuthorizationFinished);
}

/// The system's notification settings, as the app reads them.
fn settings_read(events: &SystemSender, settings: SystemSettings) {
    send_reliably(events, SystemEvent::Settings(settings));
}

/// Maps the system's authorization status.
fn authorization_of(status: UNAuthorizationStatus) -> Authorization {
    match status {
        UNAuthorizationStatus::NotDetermined => Authorization::NotDetermined,
        UNAuthorizationStatus::Denied => Authorization::Denied,
        UNAuthorizationStatus::Authorized
        | UNAuthorizationStatus::Provisional
        | UNAuthorizationStatus::Ephemeral => Authorization::Allowed,
        _ => Authorization::Unknown,
    }
}

/// The framework can abort an unbundled process inside `dispatch_once`.
/// Run the complete guard before evaluating any `UserNotifications` call.
fn only_bundled<T>(open: impl FnOnce() -> T) -> Option<T> {
    let bundle = NSBundle::mainBundle();
    let id = bundle.bundleIdentifier()?;
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle_path = Path::new(&bundle.bundlePath().to_string())
        .canonicalize()
        .ok()?;
    matches_bundle(&id.to_string(), &bundle_path, &executable).then(open)
}

fn matches_bundle(id: &str, bundle: &Path, executable: &Path) -> bool {
    id == BUNDLE_ID
        && bundle
            .extension()
            .is_some_and(|extension| extension == "app")
        && executable.parent() == Some(bundle.join("Contents/MacOS").as_path())
}

pub(crate) fn open() -> Option<(Notifications, SystemEvents)> {
    autoreleasepool(|_| {
        let opened = only_bundled(|| {
            let (tx, clicks, rx) = channels();
            let delegate = Delegate::new(clicks);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            let center = MacCenter {
                center,
                _delegate: delegate,
                events: tx,
            };
            (center, rx)
        });
        let Some((center, rx)) = opened else {
            tracing::info!("macOS notifications disabled outside the dev.abyss app bundle");
            return None;
        };
        let mut seed = [0_u8; 8];
        let seed = getrandom::fill(&mut seed)
            .ok()
            .map(|()| u64::from_ne_bytes(seed));
        if seed.is_some() {
            tracing::info!("macOS notifications available in the dev.abyss app bundle");
        } else {
            tracing::info!(
                "macOS notifications limited to removal because an account epoch could not be generated"
            );
        }
        center.settings();
        center.existing();
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str(BADGE_KEY);
        let badge = if defaults.objectForKey(&key).is_none() || defaults.boolForKey(&key) {
            BadgePreference::On
        } else {
            BadgePreference::Off
        };
        Some((Notifications::new(Box::new(center), seed, badge), rx))
    })
}

pub(crate) fn is_hidden() -> bool {
    MainThreadMarker::new().is_some_and(|main| NSApplication::sharedApplication(main).isHidden())
}

fn encode(payload: Payload) -> Retained<NSDictionary<NSString, NSNumber>> {
    NSDictionary::from_retained_objects(
        &[ns_string!("epoch"), ns_string!("chat"), ns_string!("topic")],
        &[
            NSNumber::new_i64(payload.epoch),
            NSNumber::new_i64(payload.chat),
            NSNumber::new_i64(payload.topic),
        ],
    )
}

fn decode(info: &NSDictionary) -> Option<Payload> {
    if info.count() != 3 {
        return None;
    }
    let value = |key: &NSString| info.objectForKey(key).as_deref().and_then(integer);
    Some(Payload {
        epoch: value(ns_string!("epoch"))?,
        chat: value(ns_string!("chat"))?,
        topic: value(ns_string!("topic"))?,
    })
}

fn integer(object: &AnyObject) -> Option<i64> {
    let number = object.downcast_ref::<NSNumber>()?;
    let ptr = std::ptr::from_ref(number).cast::<CFType>();
    // SAFETY: Foundation NSNumber is toll-free bridged to Core Foundation.
    // The borrow is limited to the life of the validated Objective-C object.
    let cf = unsafe { &*ptr };
    let cf = cf.downcast_ref::<CFNumber>()?;
    if cf.is_float_type() || number > &*NSNumber::new_i64(i64::MAX) {
        return None;
    }
    cf.as_i64()
}

impl MacCenter {
    fn existing(&self) {
        let events = self.events.clone();
        let callback = RcBlock::new(move |notifications: NonNull<NSArray<UNNotification>>| {
            // SAFETY: Apple supplies a nonnull array valid for the duration of this block.
            let notifications = unsafe { notifications.as_ref() };
            let receipts = notifications
                .iter()
                .take(MAX_TRACKED)
                .filter_map(|notification| {
                    Receipt::from_identifier(&notification.request().identifier().to_string())
                })
                .collect();
            send_reliably(&events, SystemEvent::Existing(receipts));
        });
        self.center
            .getDeliveredNotificationsWithCompletionHandler(&callback);
    }
}

impl Center for MacCenter {
    fn settings(&self) {
        let events = self.events.clone();
        let callback = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: Apple supplies nonnull settings valid for this callback.
            let settings = unsafe { settings.as_ref() };
            let settings = SystemSettings {
                authorization: authorization_of(settings.authorizationStatus()),
                badges: settings.badgeSetting() == UNNotificationSetting::Enabled,
            };
            settings_read(&events, settings);
        });
        self.center
            .getNotificationSettingsWithCompletionHandler(&callback);
    }

    fn authorize(&self) {
        let events = self.events.clone();
        let callback = RcBlock::new(move |_granted: objc2::runtime::Bool, error: *mut NSError| {
            if !error.is_null() {
                tracing::warn!("macOS notification authorization request failed");
            }
            authorization_finished(&events);
        });
        self.center
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Badge
                    | UNAuthorizationOptions::Sound,
                &callback,
            );
    }

    fn deliver(&self, banner: Banner) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&banner.title));
        if let Some(subtitle) = banner.subtitle {
            content.setSubtitle(&NSString::from_str(&subtitle));
        }
        content.setBody(&NSString::from_str(&banner.body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        if banner.receipt.payload.chat != 0 {
            content.setThreadIdentifier(&NSString::from_str(
                &banner.receipt.payload.chat.to_string(),
            ));
        }
        let info = encode(banner.receipt.payload);
        // SAFETY: Both generic parameters are erased to AnyObject.
        let info = unsafe { info.cast_unchecked() };
        // SAFETY: This immutable dictionary has only NSString keys and NSNumber
        // values, all of which are property-list types accepted by the framework.
        unsafe { content.setUserInfo(info) };
        let id = NSString::from_str(&banner.receipt.identifier());
        let request =
            UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        let events = self.events.clone();
        let receipt = banner.receipt;
        let completion = RcBlock::new(move |error: *mut NSError| {
            let outcome = if error.is_null() {
                Delivery::Accepted
            } else {
                tracing::warn!("macOS notification delivery failed");
                Delivery::Failed
            };
            send_reliably(&events, SystemEvent::Delivered { receipt, outcome });
        });
        self.center
            .addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    }

    fn remove(&self, ids: &[Receipt]) {
        let ids = NSArray::from_retained_slice(
            &ids.iter()
                .map(|id| NSString::from_str(&id.identifier()))
                .collect::<Vec<_>>(),
        );
        self.center
            .removePendingNotificationRequestsWithIdentifiers(&ids);
        self.center
            .removeDeliveredNotificationsWithIdentifiers(&ids);
    }

    fn clear(&self, completion: Cleared) {
        self.center.removeAllPendingNotificationRequests();
        self.center.removeAllDeliveredNotifications();
        // The center processes requests serially. Acknowledge only after both
        // reads, issued after the removals, report that they are empty.
        let clear = Arc::new(Mutex::new((0_u8, Some(completion))));
        let pending = Arc::clone(&clear);
        let on_pending = RcBlock::new(move |items: NonNull<NSArray<UNNotificationRequest>>| {
            // SAFETY: Apple's callback supplies a live nonnull array for this invocation.
            let items = unsafe { items.as_ref() };
            report_clear(&pending, items.count());
        });
        let on_delivered = RcBlock::new(move |items: NonNull<NSArray<UNNotification>>| {
            // SAFETY: Apple's callback supplies a live nonnull array for this invocation.
            let items = unsafe { items.as_ref() };
            report_clear(&clear, items.count());
        });
        self.center
            .getPendingNotificationRequestsWithCompletionHandler(&on_pending);
        self.center
            .getDeliveredNotificationsWithCompletionHandler(&on_delivered);
    }

    fn badge(&self, label: Option<&str>) {
        if let Some(main) = MainThreadMarker::new() {
            let label = label.map(NSString::from_str);
            NSApplication::sharedApplication(main)
                .dockTile()
                .setBadgeLabel(label.as_deref());
        }
    }

    fn save_badge_preference(&self, preference: BadgePreference) {
        NSUserDefaults::standardUserDefaults().setBool_forKey(
            preference == BadgePreference::On,
            &NSString::from_str(BADGE_KEY),
        );
    }
}

fn report_clear(state: &Mutex<(u8, Option<Cleared>)>, count: usize) {
    if let Ok(mut state) = state.lock() {
        if count != 0 {
            state.1 = None;
            return;
        }
        state.0 += 1;
        if state.0 == 2
            && let Some(completion) = state.1.take()
        {
            completion();
        }
    }
}

#[cfg(test)]
mod tests;
