# rawusb

[![crates.io](https://img.shields.io/crates/v/rawusb.svg)](https://crates.io/crates/rawusb)
[![docs.rs](https://docs.rs/rawusb/badge.svg)](https://docs.rs/rawusb)
[![CI](https://github.com/KarpelesLab/rawusb/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/rawusb/actions/workflows/ci.yml)

Dependency-free, cross-platform USB device access for Rust, in the spirit of
libusb. `rawusb` talks to each operating system's native USB stack directly:
usbfs on Linux, WinUSB on Windows and IOKit on macOS. There are no external
crates, no C library to install, and no build script.

- **Enumeration and descriptors** without opening or having permissions on a
  device: device, configuration, interface, endpoint, interface association
  and SuperSpeed companion descriptors, parsed the way libusb parses them.
- **Convenience layer**: open, set configuration, claim interfaces, alternate
  settings, kernel-driver detach/attach, synchronous control, bulk and
  interrupt transfers with timeouts, string descriptors, BOS.
- **Low-level layer**: allocate a [`Transfer`] once, submit it, cancel it,
  wait for it, get a callback, or `.await` it from any async runtime.
  Isochronous transfers, multi-packet layouts, short-not-ok and
  zero-length-packet flags are all exposed.
- **Hotplug notifications** (optional `hotplug` feature): learn when devices
  arrive and leave, filtered by vendor, product or class, as a callback or a
  queue you pull from.
- One background event thread per [`Context`] drives completion, so callers
  never have to pump events.

Minimum supported Rust version: 1.89 (edition 2024).

## Example

```rust
use rawusb::{Context, ControlType, Direction, Recipient, request_type};
use std::time::Duration;

fn main() -> rawusb::Result<()> {
    let ctx = Context::new()?;
    for dev in ctx.devices()? {
        let d = dev.device_descriptor();
        println!("{:03}:{:03} {:04x}:{:04x} {}", dev.bus_number(), dev.address(), d.vendor_id, d.product_id, dev.speed());
    }

    let handle = ctx.open_device_with_vid_pid(0x1234, 0x5678)?;
    handle.set_auto_detach_kernel_driver(true);
    handle.claim_interface(0)?;

    let mut buf = [0u8; 64];
    let n = handle.bulk_read(0x81, &mut buf, Duration::from_secs(1))?;
    println!("read {n} bytes");

    let rt = request_type(Direction::In, ControlType::Vendor, Recipient::Device);
    let n = handle.control_read(rt, 0x01, 0, 0, &mut buf, Duration::from_secs(1))?;
    println!("vendor request returned {n} bytes");
    Ok(())
}
```

Hotplug, with the `hotplug` feature enabled:

```rust
let ctx = rawusb::Context::new()?;
// `enumerate_existing` reports what is already plugged in, so nothing is
// missed between enumerating and starting to watch.
let watcher = ctx.hotplug().vendor_id(0x046d).enumerate_existing(true).watch()?;
for event in watcher.iter() {
    match event {
        rawusb::HotplugEvent::Arrived(dev) => println!("arrived: {dev:?}"),
        rawusb::HotplugEvent::Left(dev) => println!("left: {dev:?}"),
    }
}
```

Asynchronous use, with the same transfer resubmitted from its callback:

```rust
use rawusb::{Transfer, TransferStatus};

let t = Transfer::bulk(&handle, 0x81, vec![0u8; 512]);
t.set_callback(|t| {
    if t.status() == TransferStatus::Completed {
        println!("{} bytes: {:?}", t.actual_length(), &*t.data().unwrap());
        let _ = t.submit(); // keep streaming
    }
})?;
t.submit()?;

// Or from async code, no runtime dependency needed:
// let status = t.completion().await;
```

## Platform notes

| | Linux | Windows | macOS |
|---|---|---|---|
| Enumeration, descriptors | sysfs | SetupAPI + hub driver ioctls | IOKit registry |
| I/O | usbfs URBs, `poll` | WinUSB, I/O completion port | `IOUSBLib`, CFRunLoop |
| Control / bulk / interrupt | yes | yes | yes |
| Isochronous | yes | yes (Windows 8.1+) | yes (experimental) |
| Hotplug (`hotplug` feature) | netlink uevents | `CM_Register_Notification` (Windows 10 1709+) | IOKit notifications |
| Kernel driver detach | yes | n/a (WinUSB only) | not possible |
| Device reset | yes | not supported by WinUSB | yes |
| Set configuration | yes | only the current one | yes |

**Linux** needs read/write access to `/dev/bus/usb/BBB/DDD`; a udev rule
granting your user (or a group such as `plugdev`) access is the usual way.

**Windows** can enumerate and read descriptors from every device, but only
devices (or composite-device functions) bound to the WinUSB driver can be
claimed and used for transfers. Bind one with Zadig, WCID descriptors, or an
INF file.

**macOS** can enumerate everything; claiming an interface that a kernel driver
owns (HID, mass storage, CDC, ...) fails with `ErrorKind::Access`, as it does
with libusb. Descriptor requests still work on such devices.

**Isochronous transfers** are portable as long as every packet is exactly the
endpoint's maximum packet size: WinUSB slices the buffer itself at that size
rather than following a packet table, and rejects any other layout (the last
packet of an OUT transfer may be shorter). Linux and macOS accept arbitrary
per-packet lengths.

## Status

Pre-1.0. The Linux backend is exercised against real hardware in the test
suite (`tests/hardware.rs`, which skips itself when no suitable device is
nominated through the environment). The Windows and macOS backends are
compiled on every target in CI and follow libusb's proven call sequences,
but have had less real-device time; bug reports with the failing call are
welcome. The Linux hotplug backend is exercised against real kernel uevents;
the Windows and macOS ones are compile-checked only.

## License

MIT, see [LICENSE](LICENSE).
