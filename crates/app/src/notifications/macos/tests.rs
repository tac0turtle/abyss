use super::*;

#[test]
fn unbundled_test_process_cannot_reach_the_notification_center() {
    let opened = only_bundled(|| panic!("an unbundled process must never call the center"));
    assert!(opened.is_none());
    assert!(open().is_none());
}

#[test]
fn bundle_path_and_identifier_are_both_required() {
    let bundle = Path::new("/Applications/abyss.app");
    let exe = bundle.join("Contents/MacOS/abyss");
    assert!(matches_bundle(BUNDLE_ID, bundle, &exe));
    assert!(!matches_bundle("other.app", bundle, &exe));
    assert!(!matches_bundle(BUNDLE_ID, bundle, Path::new("/tmp/abyss")));
    assert!(!matches_bundle(
        BUNDLE_ID,
        Path::new("/tmp/abyss"),
        Path::new("/tmp/abyss/Contents/MacOS/abyss"),
    ));
}

#[test]
fn user_info_requires_exact_integer_fields() {
    autoreleasepool(|_| {
        let payload = Payload {
            epoch: i64::MAX,
            chat: -1_000_000_000_001,
            topic: 1,
        };
        let info = encode(payload);
        // SAFETY: Both types are erased to their common Objective-C base.
        let info = unsafe { info.cast_unchecked() };
        assert_eq!(decode(info), Some(payload));
        for wrong in [
            NSNumber::new_bool(true),
            NSNumber::new_f64(1.),
            NSNumber::new_u64(u64::MAX),
        ] {
            let info = NSDictionary::from_slices(
                &[ns_string!("epoch"), ns_string!("chat"), ns_string!("topic")],
                &[&*NSNumber::new_i64(1), &*NSNumber::new_i64(1), &*wrong],
            );
            // SAFETY: All entries remain Objective-C objects after erasure.
            assert_eq!(decode(unsafe { info.cast_unchecked() }), None);
        }
        let info = NSDictionary::from_slices(
            &[ns_string!("epoch"), ns_string!("chat"), ns_string!("topic")],
            &[ns_string!("1"), ns_string!("1"), ns_string!("0")],
        );
        // SAFETY: All entries remain Objective-C objects after erasure.
        assert_eq!(decode(unsafe { info.cast_unchecked() }), None);
    });
}

/// Review A38: the ordinary callbacks drop events when the queue is full. The
/// one-shot enumeration must not be one of them: it waits for room, and the
/// receiver gets it once the queue drains.
#[test]
fn a_full_callback_queue_does_not_lose_the_enumeration() {
    let (tx, rx) = async_channel::bounded(32);
    for _ in 0..32 {
        assert!(tx.try_send(SystemEvent::AuthorizationFinished).is_ok());
    }
    assert!(
        tx.try_send(SystemEvent::Existing(Vec::new())).is_err(),
        "an ordinary send is dropped while the queue is full"
    );
    let callback = std::thread::spawn(move || {
        send_reliably(&tx, SystemEvent::Existing(Vec::new()));
        send_reliably(&tx, SystemEvent::AuthorizationFinished);
    });
    let mut enumerations = 0;
    for _ in 0..34 {
        if matches!(rx.recv_blocking(), Ok(SystemEvent::Existing(_))) {
            enumerations += 1;
        }
    }
    assert!(callback.join().is_ok());
    assert_eq!(enumerations, 1);
}

#[test]
fn a_dropped_receiver_does_not_block_a_completion() {
    let (tx, rx) = async_channel::bounded(1);
    drop(rx);
    send_reliably(&tx, SystemEvent::Existing(Vec::new()));
}

/// Review A53: the authorization reply and the settings read come from the
/// system's queues and cannot be repeated by the app (a lost reply leaves the
/// prompt in flight and hides "Enable notifications" until restart), so a
/// full queue makes them wait for room rather than dropping them.
#[test]
fn a_full_queue_does_not_lose_the_authorization_reply_or_a_settings_read() {
    let (tx, rx) = async_channel::bounded(2);
    for _ in 0..2 {
        assert!(tx.try_send(SystemEvent::Existing(Vec::new())).is_ok());
    }
    let callback = std::thread::spawn(move || {
        authorization_finished(&tx);
        let read = SystemSettings {
            authorization: Authorization::NotDetermined,
            badges: true,
        };
        settings_read(&tx, read);
    });
    let mut finished = 0;
    let mut settings = Vec::new();
    for _ in 0..4 {
        match rx.recv_blocking() {
            Ok(SystemEvent::AuthorizationFinished) => finished += 1,
            Ok(SystemEvent::Settings(read)) => settings.push(read),
            _ => {}
        }
    }
    assert!(callback.join().is_ok());
    assert_eq!(finished, 1, "the reply arrived");
    assert_eq!(
        settings,
        [SystemSettings {
            authorization: Authorization::NotDetermined,
            badges: true
        }],
        "the settings read arrived"
    );
}

#[test]
fn authorization_statuses_map_to_the_apps_states() {
    for (status, expected) in [
        (
            UNAuthorizationStatus::NotDetermined,
            Authorization::NotDetermined,
        ),
        (UNAuthorizationStatus::Denied, Authorization::Denied),
        (UNAuthorizationStatus::Authorized, Authorization::Allowed),
        (UNAuthorizationStatus::Provisional, Authorization::Allowed),
    ] {
        assert_eq!(authorization_of(status), expected);
    }
}
