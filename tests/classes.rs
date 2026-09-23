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

#[cfg(feature = "msc")]
#[test]
fn msc_inquiry_capacity_and_read() {
    use rawusb::msc::{DataPhase, MassStorage};
    use std::io::{Read, Seek, SeekFrom};
    let Some((dev, _)) = env_device("RAWUSB_TEST_MSC") else { return };
    let msc = MassStorage::open(&dev).unwrap();
    let info = msc.inquiry(0).unwrap();
    eprintln!("{} {} {} (max LUN {})", info.vendor, info.product, info.revision, msc.max_lun());
    let mut disk = msc.block_device(0).unwrap();
    let cap = disk.capacity();
    eprintln!(
        "{} blocks of {} bytes, write protected: {:?}",
        cap.block_count,
        cap.block_size,
        msc.is_write_protected(0)
    );
    assert!(cap.block_count > 0);

    // Block reads, direct and through the stream, agree.
    let bs = cap.block_size as usize;
    let mut direct = vec![0u8; bs * 4];
    disk.read_blocks(0, &mut direct).unwrap();
    let mut streamed = vec![0u8; bs * 4];
    disk.read_exact(&mut streamed[..7]).unwrap();
    disk.read_exact(&mut streamed[7..]).unwrap();
    assert_eq!(direct, streamed);

    // Unaligned read across a block boundary.
    disk.seek(SeekFrom::Start(bs as u64 - 3)).unwrap();
    let mut six = [0u8; 6];
    disk.read_exact(&mut six).unwrap();
    assert_eq!(&six[..], &direct[bs - 3..bs + 3]);

    // Reading at the end returns EOF, and a block past the end is refused.
    disk.seek(SeekFrom::End(-2)).unwrap();
    let mut tail = [0u8; 8];
    assert_eq!(disk.read(&mut tail).unwrap(), 2);
    assert_eq!(disk.read(&mut tail).unwrap(), 0);
    assert!(disk.read_blocks(cap.block_count, &mut direct[..bs]).is_err());

    // A command the device rejects comes back as a failed status plus sense,
    // and the transport stays usable afterwards.
    let r = msc.execute(0, &[0xff, 0, 0, 0, 0, 0], DataPhase::None).unwrap();
    assert!(!r.passed);
    let sense = msc.request_sense(0).unwrap();
    eprintln!("bogus opcode: {sense}");
    assert_eq!(sense.key, rawusb::msc::SenseKey::IllegalRequest);
    msc.test_unit_ready(0).unwrap();
}
