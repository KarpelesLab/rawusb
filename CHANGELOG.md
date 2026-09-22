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
- `list_devices` example and a self-skipping hardware test suite.
