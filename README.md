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
- **Class helpers** (optional, one feature each): HID, mass storage, USB
  serial (CDC-ACM and FTDI) and USB video, ready to use. See below.
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

## Class helpers

Each helper is behind its own cargo feature, so you only compile what you
use:

```toml
rawusb = { version = "0.1", features = ["serial", "hid"] }
```

| Feature | Module | What you get |
|---|---|---|
| `hid` | `rawusb::hid` | `HidDevice`: input/output/feature reports with hidapi's report-ID conventions, idle and protocol requests; `ReportDescriptor`: a parser that gives each field's offset, size, usages and logical range. |
| `msc` | `rawusb::msc` | `MassStorage`: SCSI over the bulk-only transport with the spec's error recovery (INQUIRY, READ CAPACITY, READ/WRITE 10/16, REQUEST SENSE, write-protect, eject, ...); `BlockDevice`: a logical unit as `Read + Write + Seek`. |
| `serial` | `rawusb::serial` | `SerialPort` for CDC-ACM devices and FTDI chips (AM through FT4232HA, with the Linux driver's baud divisors): line settings, flow control, DTR/RTS, break, modem status, `std::io::Read`/`Write`, FTDI latency timer and bit modes. |
| `uvc` | `rawusb::uvc` | `Camera`: formats, frame sizes and rates, camera and processing-unit controls; `Stream`: probe/commit negotiation, isochronous or bulk streaming, frames reassembled from payloads. |

```rust
use rawusb::serial::{LineConfig, SerialPort};
use std::io::Write;

let ctx = rawusb::Context::new()?;
let dev = ctx.find_device(0x0403, 0x6001)?.expect("adapter not plugged in");
let mut port = SerialPort::open(&dev)?;
port.set_line_config(&LineConfig::new(115_200))?;
port.write_all(b"hello\r\n")?;
```

On a composite device (a debug probe with UARTs and raw HID channels, a
dock, a phone), take the device first and run the helpers you need on it.
Each module lists what it found (`hid::interfaces`, `serial::ports`,
`msc::interfaces`, `uvc::interfaces`) and can open one interface or all:

```rust
let handle = dev.open()?;
handle.claim_all_interfaces()?;           // detach every kernel driver up front
let uarts = SerialPort::open_all(&handle)?;
let raw_hid = HidDevice::open_all(&handle)?;
let keypad = HidDevice::open_interface(handle.clone(), 3)?;
```

Helpers never share an interface (a second one on the same interface fails
with `ErrorKind::Busy`), and on a taken device they borrow the handle's
claims, so dropping one does not hand its interface back to the kernel.

Without taking the device first, a helper claims the interfaces it drives for as long as it lives. On Linux
the kernel driver (usbhid, usb-storage, ftdi_sio, cdc_acm, uvcvideo) is
detached meanwhile and re-attached when the helper is dropped, so the
matching `/dev` node disappears in between; a process that is killed before
dropping it leaves the driver detached until the device is replugged. On
macOS and Windows these drivers cannot be displaced, so the helpers only work
with devices bound to a generic driver (WinUSB on Windows); there, prefer the
operating system's own HID, storage, serial and camera APIs for devices that
keep their class driver.

Examples: `hid_dump`, `msc_info`, `serial_monitor` and `uvc_capture`, each
run with its feature, e.g. `cargo run --features uvc --example uvc_capture`.

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
the Windows and macOS ones are compile-checked only. Of the class helpers,
HID and FTDI serial have run against real devices on Linux; the mass-storage,
CDC-ACM and UVC helpers are covered by unit tests of their protocol logic
and by hardware tests (`tests/classes.rs`) waiting for a device to run on.

## License

MIT, see [LICENSE](LICENSE).
