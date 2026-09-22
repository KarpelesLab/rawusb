# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
