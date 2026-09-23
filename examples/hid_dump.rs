//! Prints a HID device's report layout, then its input reports for ten
//! seconds.
//!
//! Run with `cargo run --features hid --example hid_dump -- 1b1c:1c27`. On
//! Linux the kernel HID driver is detached while this runs and re-attached
//! when it exits normally (not when killed): do not point it at the keyboard
//! you are typing on.

use rawusb::hid::{HidDevice, ReportType};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (vid, pid) = parse_id(&std::env::args().nth(1).ok_or("usage: hid_dump vvvv:pppp")?)?;
    let ctx = rawusb::Context::new()?;
    let dev = ctx.find_device(vid, pid)?.ok_or("device not found")?;
    let hid = HidDevice::open(&dev)?;
    let d = hid.report_descriptor();
    println!(
        "HID {:?}, application usages {:?}",
        hid.hid_descriptor().hid_version,
        d.application_usages
    );
    for kind in [ReportType::Input, ReportType::Output, ReportType::Feature] {
        for id in d.report_ids(kind) {
            println!("{kind:?} report {id}: {} bytes", d.report_len(kind, id).unwrap_or(0));
            for f in d.report_fields(kind, id) {
                println!(
                    "  bits {:>4}+{}x{:<2} {:<8} {:?} logical {}..{}",
                    f.bit_offset,
                    f.count,
                    f.bit_size,
                    if f.is_constant() {
                        "padding"
                    } else if f.is_variable() {
                        "variable"
                    } else {
                        "array"
                    },
                    f.usages,
                    f.logical_min,
                    f.logical_max
                );
            }
        }
    }
    println!("reading input reports for 10 seconds");
    let mut buf = vec![0u8; hid.max_input_report_len()];
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        match hid.read(&mut buf, Duration::from_millis(500)) {
            Ok(n) => println!("{:02x?}", &buf[..n]),
            Err(e) if e.is_timeout() => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn parse_id(s: &str) -> Result<(u16, u16), Box<dyn std::error::Error>> {
    let (v, p) = s.split_once(':').ok_or("expected vvvv:pppp")?;
    Ok((u16::from_str_radix(v, 16)?, u16::from_str_radix(p, 16)?))
}
