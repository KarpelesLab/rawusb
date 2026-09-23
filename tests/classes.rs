//! Class-helper tests against real devices. Each one skips itself unless a
//! device is nominated through the environment, because opening a class
//! helper detaches the kernel driver for as long as the test runs:
//!
//! - `RAWUSB_TEST_HID=vvvv:pppp`: any HID device that is safe to take from
//!   its driver for a moment (not your keyboard).
//! - `RAWUSB_TEST_MSC=vvvv:pppp`: an *unmounted* mass-storage device; only
//!   read commands are sent.
//! - `RAWUSB_TEST_SERIAL=vvvv:pppp[:iface]`: a USB serial adapter (CDC-ACM or
//!   FTDI). Line settings and modem lines are changed; nothing is sent.
//! - `RAWUSB_TEST_UVC=vvvv:pppp`: a webcam; a few frames are captured.

#![cfg(any(feature = "hid", feature = "msc", feature = "serial", feature = "uvc"))]

use rawusb::{Context, Device, ErrorKind};
#[allow(unused_imports)]
use std::time::Duration;

fn env_device(var: &str) -> Option<(Device, Vec<String>)> {
    let spec = std::env::var(var).ok()?;
    let parts: Vec<String> = spec.split(':').map(str::to_string).collect();
    let vid = u16::from_str_radix(&parts[0], 16).unwrap();
    let pid = u16::from_str_radix(&parts[1], 16).unwrap();
    let ctx = match Context::new() {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotSupported => return None,
        Err(e) => panic!("{e}"),
    };
    let dev = ctx
        .find_device(vid, pid)
        .unwrap()
        .unwrap_or_else(|| panic!("{var}: {spec} is not plugged in"));
    Some((dev, parts[2..].to_vec()))
}

#[cfg(feature = "hid")]
#[test]
fn hid_report_descriptor_and_input() {
    use rawusb::hid::{HidDevice, ReportType};
    let Some((dev, _)) = env_device("RAWUSB_TEST_HID") else { return };
    let hid = HidDevice::open(&dev).unwrap();
    let d = hid.report_descriptor();
    eprintln!(
        "HID {:?}: {} byte report descriptor, application usages {:04x?}, ids {}",
        hid.hid_descriptor().hid_version,
        hid.raw_report_descriptor().len(),
        d.application_usages,
        d.uses_report_ids()
    );
    assert_eq!(
        hid.raw_report_descriptor().len(),
        hid.hid_descriptor().report_descriptor_length().unwrap() as usize
    );
    assert!(!d.fields.is_empty());
    for kind in [ReportType::Input, ReportType::Output, ReportType::Feature] {
        for id in d.report_ids(kind) {
            eprintln!("  {kind:?} report {id}: {} bytes", d.report_len(kind, id).unwrap());
        }
    }
    // Many devices only report on change; a timeout is as good as a report.
    let mut buf = vec![0u8; hid.max_input_report_len()];
    match hid.read(&mut buf, Duration::from_millis(300)) {
        Ok(n) => eprintln!("  input: {:02x?}", &buf[..n]),
        Err(e) => assert_eq!(e.kind(), ErrorKind::Timeout, "{e}"),
    }
    let handle = hid.handle().clone();
    let iface = hid.interface_number();
    drop(hid);
    // Dropping the helper gives the interface back to the kernel driver.
    std::thread::sleep(Duration::from_millis(200));
    assert!(handle.claimed_interfaces().is_empty());
    if cfg!(target_os = "linux") {
        assert!(handle.kernel_driver_active(iface).unwrap());
    }
}
