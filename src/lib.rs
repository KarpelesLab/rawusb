//! Dependency-free, cross-platform USB device access in the spirit of libusb.
//!
//! `rawusb` talks to the operating system's native USB stack directly:
//! usbfs on Linux, WinUSB on Windows and IOKit on macOS. There are no
//! external crates and no C library to install. The API mirrors libusb's
//! concepts so that existing knowledge carries over, while staying idiomatic
//! Rust.
//!
//! # Layers
//!
//! - [`Context`] owns a session and the event thread that drives completion.
//! - [`Device`] is an enumerated device with its descriptors; [`Device::open`]
//!   yields a [`DeviceHandle`].
//! - [`DeviceHandle`] configures the device (configuration, interfaces,
//!   alternate settings, kernel drivers) and offers synchronous
//!   [`control`](DeviceHandle::control_read), [`bulk`](DeviceHandle::bulk_read)
//!   and [`interrupt`](DeviceHandle::interrupt_read) transfers.
//! - [`Transfer`] is the asynchronous primitive underneath: allocate once,
//!   [`submit`](Transfer::submit), [`cancel`](Transfer::cancel),
//!   [`wait`](Transfer::wait), or `.await` its [`completion`](Transfer::completion).
//!   Isochronous transfers are only available through it.
//!
//! # Features
//!
//! Everything below is off by default.
//!
//! - `hotplug` adds the [`hotplug`] module, which reports devices arriving
//!   and leaving. See [`Context::hotplug`].
//!
//! Class helpers, ready-made drivers for common device classes built on the
//! public API above. Each claims the interfaces it needs (detaching the
//! kernel driver on Linux) and gives them back when dropped:
//!
//! - `hid`: [`hid::HidDevice`] and a report descriptor parser.
//! - `msc`: [`msc::MassStorage`] (bulk-only transport, SCSI commands) and a
//!   `Read + Write + Seek` [`msc::BlockDevice`].
//! - `serial`: [`serial::SerialPort`] for CDC-ACM devices and FTDI chips.
//! - `uvc`: [`uvc::Camera`] for webcams: formats, controls, frame streaming.
//! - `net`: [`net::NetDevice`] for USB Ethernet functions (CDC-ECM, CDC-NCM,
//!   RNDIS).
//! - `pktkit`: implements `pktkit::L2Device` for [`net::NetDevice`]. The only
//!   feature with a dependency, the [pktkit](https://docs.rs/pktkit) crate.
//!
//! On a composite device, take the whole device first with
//! [`DeviceHandle::claim_all_interfaces`], then start whatever helpers you
//! need on clones of that handle. Each helper drives its own interfaces
//! (a second one on the same interface fails with [`ErrorKind::Busy`]), and
//! helpers can be dropped and reopened while the device stays taken:
//!
//! ```no_run
//! # #[cfg(all(feature = "hid", feature = "serial"))] {
//! # let ctx = rawusb::Context::new()?;
//! # let dev = ctx.find_device(0x1234, 0x5678)?.unwrap();
//! let handle = dev.open()?;
//! handle.claim_all_interfaces()?; // detaches every kernel driver
//! let console = rawusb::serial::SerialPort::open_all(&handle)?;
//! let raw_hid = rawusb::hid::HidDevice::open_all(&handle)?;
//! # }
//! # Ok::<(), rawusb::Error>(())
//! ```
//!
//! # Example
//!
//! ```no_run
//! use rawusb::{Context, Direction, ControlType, Recipient, request_type};
//! use std::time::Duration;
//!
//! let ctx = Context::new()?;
//! for dev in ctx.devices()? {
//!     let d = dev.device_descriptor();
//!     println!("{:03}:{:03} {:04x}:{:04x}", dev.bus_number(), dev.address(), d.vendor_id, d.product_id);
//! }
//!
//! let handle = ctx.open_device_with_vid_pid(0x1234, 0x5678)?;
//! handle.set_auto_detach_kernel_driver(true);
//! handle.claim_interface(0)?;
//! let mut buf = [0u8; 64];
//! let n = handle.bulk_read(0x81, &mut buf, Duration::from_secs(1))?;
//! println!("got {n} bytes");
//!
//! // A vendor control request.
//! let rt = request_type(Direction::In, ControlType::Vendor, Recipient::Device);
//! let n = handle.control_read(rt, 0x01, 0, 0, &mut buf, Duration::from_secs(1))?;
//! # Ok::<(), rawusb::Error>(())
//! ```

#![warn(missing_debug_implementations)]

#[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
mod class;
mod context;
pub mod descriptors;
mod device;
mod error;
mod handle;
#[cfg(feature = "hid")]
pub mod hid;
#[cfg(feature = "hotplug")]
pub mod hotplug;
#[cfg(feature = "msc")]
pub mod msc;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "serial")]
pub mod serial;
mod sys;
pub mod transfer;
pub mod types;
#[cfg(feature = "uvc")]
pub mod uvc;

pub use context::Context;
pub use descriptors::{ConfigDescriptor, DeviceDescriptor, EndpointDescriptor, Interface, InterfaceDescriptor};
pub use device::Device;
pub use error::{Error, ErrorKind, Result};
pub use handle::DeviceHandle;
#[cfg(feature = "hotplug")]
pub use hotplug::{HotplugEvent, HotplugRegistration, HotplugWatcher};
pub use transfer::{Transfer, TransferFlags};
pub use types::{
    ControlSetup, ControlType, Direction, IsoPacket, NO_TIMEOUT, Recipient, Speed, TransferStatus, TransferType, Version, request_type,
};
