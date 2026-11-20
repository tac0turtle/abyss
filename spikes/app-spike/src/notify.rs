//! `UNUserNotificationCenter` from an ad-hoc bundle, and the unbundled failure.

use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationSettings,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use crate::say;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpikeNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            say("delegate: willPresentNotification called");
            handler
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

fn error_text(err: *mut NSError) -> String {
    if err.is_null() {
        return "none".into();
    }
    // SAFETY: the system passes a live NSError for the duration of the callback.
    let err = unsafe { &*err };
    format!(
        "domain={} code={} description={:?}",
        err.domain(),
        err.code(),
        err.localizedDescription().to_string()
    )
}

fn status_name(raw: isize) -> &'static str {
    match raw {
        0 => "notDetermined",
        1 => "denied",
        2 => "authorized",
        3 => "provisional",
        4 => "ephemeral",
        _ => "unknown",
    }
}

fn settings(center: &UNUserNotificationCenter, label: &'static str) {
    let block = RcBlock::new(move |s: NonNull<UNNotificationSettings>| {
        // SAFETY: the system passes a live settings object for the callback.
        let s = unsafe { s.as_ref() };
        let status = s.authorizationStatus().0;
        say(&format!(
            "settings ({label}): authorizationStatus={} ({}), alert={}, badge={}, notificationCenter={}",
            status,
            status_name(status),
            s.alertSetting().0,
            s.badgeSetting().0,
            s.notificationCenterSetting().0
        ));
    });
    center.getNotificationSettingsWithCompletionHandler(&block);
}

pub(crate) fn run(wait_secs: u64) {
    let bundle = NSBundle::mainBundle();
    say(&format!("== notify spike, pid {}", std::process::id()));
    say(&format!(
        "bundle identifier = {:?}",
        bundle.bundleIdentifier().map(|s| s.to_string())
    ));
    say(&format!("bundle path = {}", bundle.bundlePath()));
    let center = match objc2::exception::catch(AssertUnwindSafe(
        UNUserNotificationCenter::currentNotificationCenter,
    )) {
        Ok(center) => center,
        Err(exception) => {
            say(&format!("currentNotificationCenter THREW: {exception:?}"));
            return;
        }
    };
    say("currentNotificationCenter returned a center");
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(mtm);
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    settings(&center, "before request");
    request(&center);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(wait_secs));
        say(&format!("no further outcome after {wait_secs} s; exiting"));
        std::process::exit(0);
    });
    app.run();
    drop(delegate);
}

fn request(center: &Retained<UNUserNotificationCenter>) {
    say("requestAuthorization(alert|badge|sound) called; waiting for the completion handler");
    let center_for_cb = center.clone();
    let block = RcBlock::new(move |granted: Bool, err: *mut NSError| {
        say(&format!(
            "requestAuthorization completed: granted={}, error={}",
            granted.as_bool(),
            error_text(err)
        ));
        settings(&center_for_cb, "after request");
        if granted.as_bool() {
            post_banner(&center_for_cb);
        } else {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(2));
                std::process::exit(0);
            });
        }
    });
    let options = UNAuthorizationOptions::Alert
        | UNAuthorizationOptions::Badge
        | UNAuthorizationOptions::Sound;
    center.requestAuthorizationWithOptions_completionHandler(options, &block);
}

fn post_banner(center: &Retained<UNUserNotificationCenter>) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str("abyss spike"));
    content.setBody(&NSString::from_str("New message"));
    content.setThreadIdentifier(&NSString::from_str("spike-thread"));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str("spike-1"),
        &content,
        None,
    );
    let done = RcBlock::new(move |err: *mut NSError| {
        say(&format!(
            "addNotificationRequest completed: error={}",
            error_text(err)
        ));
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    std::thread::spawn(move || {
        // The center is a process-wide singleton; fetch it here, as the type is not Send.
        let center = UNUserNotificationCenter::currentNotificationCenter();
        std::thread::sleep(Duration::from_secs(3));
        let delivered = RcBlock::new(
            move |list: NonNull<objc2_foundation::NSArray<UNNotification>>| {
                // SAFETY: a live array for the callback.
                say(&format!(
                    "delivered notifications in Notification Center: {}",
                    unsafe { list.as_ref() }.count()
                ));
            },
        );
        center.getDeliveredNotificationsWithCompletionHandler(&delivered);
        std::thread::sleep(Duration::from_secs(1));
        center.removeAllDeliveredNotifications();
        say("removeAllDeliveredNotifications called");
        std::thread::sleep(Duration::from_secs(1));
        let after = RcBlock::new(
            move |list: NonNull<objc2_foundation::NSArray<UNNotification>>| {
                // SAFETY: a live array for the callback.
                say(&format!(
                    "delivered after removal: {}",
                    unsafe { list.as_ref() }.count()
                ));
            },
        );
        center.getDeliveredNotificationsWithCompletionHandler(&after);
        std::thread::sleep(Duration::from_secs(1));
        std::process::exit(0);
    });
}
