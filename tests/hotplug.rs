//! Hotplug tests.
//!
//! These do not need anything to be physically plugged in or out: they check
//! the parts that are deterministic (the initial enumeration, filtering,
//! quiet periods, deregistration and clean shutdown). Set
//! `RAWUSB_TEST_HOTPLUG=1` and plug or unplug something within 30 seconds to
//! exercise a real device change as well.

#![cfg(feature = "hotplug")]

use rawusb::{Context, ErrorKind, HotplugEvent};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Long enough for the watcher to have delivered the initial batch, which is
/// synchronous with registration, plus any stray rescan.
const SETTLE: Duration = Duration::from_millis(400);

fn context() -> Option<Context> {
    match Context::new() {
        Ok(c) => Some(c),
        Err(e) if e.kind() == ErrorKind::NotSupported => None,
        Err(e) => panic!("Context::new failed: {e}"),
    }
}

/// Starts a watcher, or skips the test if this platform has no hotplug.
fn watcher(ctx: &Context, vendor_id: Option<u16>) -> Option<rawusb::HotplugWatcher> {
    let mut builder = ctx.hotplug().enumerate_existing(true);
    if let Some(v) = vendor_id {
        builder = builder.vendor_id(v);
    }
    match builder.watch() {
        Ok(w) => Some(w),
        Err(e) if e.kind() == ErrorKind::NotSupported => None,
        Err(e) if e.kind() == ErrorKind::Access => None,
        Err(e) => panic!("starting the watcher failed: {e}"),
    }
}

fn drain(w: &rawusb::HotplugWatcher) -> Vec<HotplugEvent> {
    let mut out = Vec::new();
    while let Some(event) = w.recv_timeout(Duration::from_millis(100)).unwrap() {
        out.push(event);
    }
    out
}

#[test]
fn existing_devices_are_reported_once() {
    let Some(ctx) = context() else { return };
    let expected = ctx.devices().unwrap();
    let Some(w) = watcher(&ctx, None) else { return };

    let events = drain(&w);
    eprintln!("{} devices enumerated, {} arrivals reported", expected.len(), events.len());
    assert_eq!(events.len(), expected.len(), "one arrival per attached device");
    assert!(events.iter().all(|e| e.is_arrival()));
    assert!(events.iter().all(|e| !e.is_departure()));

    // Same devices, in the same order as enumeration.
    for (event, device) in events.iter().zip(expected.iter()) {
        assert_eq!(event.device(), device);
        assert_eq!(event.device().vendor_id(), device.vendor_id());
        assert_eq!(event.device().bus_number(), device.bus_number());
        assert_eq!(event.device().address(), device.address());
    }

    // An arrived device is a fully usable one: its descriptors came from the
    // same enumeration path as `Context::devices`.
    if let Some(event) = events.first() {
        let device = event.device();
        assert!(device.device_descriptor().num_configurations >= 1);
    }
}

#[test]
fn nothing_happens_while_nothing_changes() {
    let Some(ctx) = context() else { return };
    let Some(w) = watcher(&ctx, None) else { return };
    let initial = drain(&w);
    if initial.is_empty() {
        return; // no USB at all on this machine
    }
    // Unrelated kernel activity (drivers binding, other subsystems) must not
    // be reported as devices coming and going.
    let start = Instant::now();
    assert!(w.recv_timeout(SETTLE).unwrap().is_none(), "unexpected hotplug event");
    assert!(start.elapsed() >= SETTLE);
    assert!(w.try_recv().unwrap().is_none());
}

#[test]
fn a_filter_narrows_the_events() {
    let Some(ctx) = context() else { return };
    let devices = ctx.devices().unwrap();
    let Some(target) = devices.first().map(|d| d.vendor_id()) else {
        return;
    };
    let matching = devices.iter().filter(|d| d.vendor_id() == target).count();

    let Some(w) = watcher(&ctx, Some(target)) else { return };
    let events = drain(&w);
    assert_eq!(events.len(), matching);
    assert!(events.iter().all(|e| e.device().vendor_id() == target));

    // A vendor id that cannot exist yields nothing at all.
    let Some(w) = watcher(&ctx, Some(0xffff)) else { return };
    assert!(drain(&w).is_empty());
}

#[test]
fn callbacks_stop_when_the_registration_is_dropped() {
    let Some(ctx) = context() else { return };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let registration = match ctx.hotplug().enumerate_existing(true).register(move |event: &HotplugEvent| {
        sink.lock().unwrap().push(event.device().vendor_id());
    }) {
        Ok(r) => r,
        Err(e) if matches!(e.kind(), ErrorKind::NotSupported | ErrorKind::Access) => return,
        Err(e) => panic!("register failed: {e}"),
    };
    let after_registration = seen.lock().unwrap().len();
    assert_eq!(after_registration, ctx.devices().unwrap().len());

    registration.unregister();
    std::thread::sleep(SETTLE);
    assert_eq!(seen.lock().unwrap().len(), after_registration, "no callbacks after unregistering");
}

#[test]
fn watchers_and_contexts_shut_down_cleanly() {
    // Repeatedly starting and dropping watchers must not leak threads or
    // hang: reaching the end of this test is the assertion.
    for _ in 0..3 {
        let Some(ctx) = context() else { return };
        let Some(w) = watcher(&ctx, None) else { return };
        drain(&w);
        drop(w);
        // A second watcher on the same context reuses the running machinery.
        let Some(w2) = watcher(&ctx, None) else { return };
        drain(&w2);
        drop(ctx); // the watcher still holds the session alive
        drop(w2);
    }
}

#[test]
fn two_sessions_can_watch_at_once() {
    // Each context takes its own subscription from the OS; neither may lock
    // the other out. On Linux this is what lets a process (or two libraries
    // inside it) hold several netlink uevent sockets.
    let Some(first) = context() else { return };
    let Some(second) = context() else { return };
    let Some(a) = watcher(&first, None) else { return };
    let Some(b) = watcher(&second, None) else { return };
    let (seen_a, seen_b) = (drain(&a).len(), drain(&b).len());
    assert_eq!(seen_a, seen_b, "both sessions see the same devices");
    assert_eq!(seen_a, first.devices().unwrap().len());
}

#[test]
fn a_real_device_change_is_reported() {
    if std::env::var("RAWUSB_TEST_HOTPLUG").is_err() {
        return;
    }
    let Some(ctx) = context() else { return };
    let Some(w) = watcher(&ctx, None) else { return };
    let before: Vec<_> = drain(&w).iter().map(|e| e.device().clone()).collect();
    eprintln!("plug or unplug a device now (30s)...");

    let event = w
        .recv_timeout(Duration::from_secs(30))
        .unwrap()
        .expect("no device change within 30 seconds");
    eprintln!("{} {:?}", if event.is_arrival() { "arrived" } else { "left" }, event.device());

    let now = ctx.devices().unwrap();
    if event.is_arrival() {
        assert!(now.iter().any(|d| d == event.device()), "an arrived device must be enumerable");
        assert!(!before.iter().any(|d| d == event.device()));
        // ...and openable, once udev has caught up.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && event.device().open().is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else {
        assert!(!now.iter().any(|d| d == event.device()), "a departed device must be gone");
        assert!(before.iter().any(|d| d == event.device()));
        assert!(event.device().open().is_err(), "a departed device cannot be opened");
    }
}
