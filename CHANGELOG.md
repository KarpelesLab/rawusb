# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Class helpers, each behind its own opt-in feature:
  - `hid`: `HidDevice` (report I/O with hidapi's report-ID conventions,
    GET/SET_REPORT, idle and protocol requests) and `ReportDescriptor`, a
    report descriptor parser that yields every field's offset, size,
    usages and logical range.
  - `msc`: `MassStorage`, SCSI over the bulk-only transport with stall and
    reset recovery, sense decoding and UNIT ATTENTION retries, and
    `BlockDevice`, a `Read + Write + Seek` view of a logical unit.
  - `serial`: `SerialPort` for CDC-ACM devices and FTDI chips, with line
    settings, flow control, modem lines and status, break, purging,
    `std::io::Read`/`Write`, and FTDI chip detection, baud divisors, latency
    timer and bit modes.
  - `uvc`: `Camera` with format/frame/control descriptors and unit controls,
    and `Stream`, which negotiates, streams over isochronous or bulk
    endpoints and reassembles frames.
- `DeviceHandle::claim_all_interfaces` takes a whole device (detaching every
  kernel driver) so several class helpers can run on it; helpers lease
  their interfaces exclusively and borrow the handle's claims when present.
  `DeviceHandle::is_claimed` reports a claim.
- Discovery for composite devices: `hid::interfaces`, `msc::interfaces`,
  `serial::ports`, `uvc::interfaces`, plus `HidDevice::open_all`,
  `SerialPort::open_all` and `SerialPort::open_interface` (CDC-ACM or FTDI,
  detected).
- `hid_dump`, `msc_info`, `serial_monitor` and `uvc_capture` examples, and a
  class-helper hardware test suite driven by environment variables.

## [0.1.1](https://github.com/KarpelesLab/rawusb/compare/v0.1.0...v0.1.1) - 2026-09-22

### Other

- Fix issues found reviewing the new hotplug and isochronous code
- Test that two sessions can watch hotplug at the same time
- Simplify the netlink drain loop
- Assert the Windows structure layouts at compile time
- Document the hotplug feature and the Windows isochronous packet rules
- Add Windows and macOS hotplug backends, and Windows isochronous transfers
- Add optional hotplug notifications with a Linux netlink backend
- Make hardware tests tolerate synthesised root hubs
- Write README and changelog for the initial release
- Add macOS backend (IOKit IOUSBLib, CFRunLoop event thread)
- Add Windows backend (SetupAPI/CfgMgr enumeration, hub ioctls, WinUSB + IOCP)
- Add hardware tests and list_devices example
- Add core API and Linux usbfs backend
- Fix config descriptor test fixture attributes

### Added
- Core API: `Context`, `Device`, `DeviceHandle`, `Transfer` with synchronous
  helpers and the asynchronous submit/cancel/wait/callback/`Future` layer.
- Descriptor parsing (device, configuration tree, interface associations,
  SuperSpeed companions, string descriptors, BOS retrieval).
- Linux backend (sysfs + usbfs) with bulk URB splitting, isochronous
  transfers, timeouts, cancellation, disconnect handling and kernel-driver
  detach/attach.
- Windows backend (SetupAPI/CfgMgr enumeration, hub ioctls for descriptors,
  WinUSB transfers on an I/O completion port, composite-function support).
- macOS backend (IOKit `IOUSBLib` device/interface objects, CFRunLoop event
  thread, kernel-side timeouts on the `182` interface revisions).
- Optional `hotplug` feature: `Context::hotplug()` builds a filtered watcher
  that reports devices arriving and leaving, either through a callback or a
  queue, with `enumerate_existing` for a race-free start. Backed by netlink
  uevents on Linux, `CM_Register_Notification` on Windows and IOKit matching
  notifications on macOS.
- Isochronous transfers on Windows, through the WinUSB isoch API (Windows
  8.1 and newer), including per-packet results for IN transfers and seamless
  stream continuation while transfers stay queued on an endpoint.
- `list_devices` and `hotplug` examples, and a self-skipping hardware test
  suite.
