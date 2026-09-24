# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `Device::serial_number` returns the serial number without opening the
  device: from sysfs on Linux, the IOKit registry on macOS, and the parent
  hub on Windows (so it works whatever driver the device is bound to).
- `Context::find_device_by_serial` and `Context::open_device_by_serial`
  locate a device by serial number among a list of vendor/product pairs.

## [0.1.3](https://github.com/KarpelesLab/rawusb/compare/v0.1.2...v0.1.3) - 2026-09-23

### Other

- Fix string descriptors with trailing junk and harden string reads

### Fixed
- String descriptors end at the first NUL character. Some devices (an
  FT2232D clone here) declare a longer descriptor holding a NUL-terminated
  string followed by leftover EEPROM bytes, which came back as garbage
  appended to the serial number; Linux cuts at the NUL too.
- String reads follow the Linux kernel's recovery: if a device stalls the
  full-size request or answers it short, the 2-byte header is read and then
  exactly the declared length. A malformed or empty language table falls
  back to US English, and the language is read once per handle instead of
  before every string.
- The hardware tests no longer probe a nonexistent string index on every
  attached device: some FTDI clones stall all requests after that until
  they are reset. The check now runs only on the nominated bulk-IN device.

## [0.1.2](https://github.com/KarpelesLab/rawusb/compare/v0.1.1...v0.1.2) - 2026-09-23

### Other

- Add USB Ethernet devices (ECM, NCM, RNDIS) and pktkit integration
- Let class helpers share a taken device and discover every interface
- Add class-helper examples and document the new features
- Add the uvc feature: video descriptors, controls and frame streaming
- Fix redundant doc links in the serial module
- Add the serial feature: CDC-ACM and FTDI behind one SerialPort
- Add the msc feature: bulk-only transport, SCSI commands, block device
- Add the hid feature: HID interfaces, report descriptor parser, report I/O

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
- `net` feature: `NetDevice` drives CDC-ECM, CDC-NCM (16-bit transfer
  blocks) and RNDIS functions, with a non-blocking send path, a receive
  callback or queue, link state and counters; `net::interfaces` lists a
  device's network functions.
- `pktkit` feature: `NetDevice` implements `pktkit::L2Device`. This is the
  only feature that adds a dependency.
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
