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

mod context;
pub mod descriptors;
mod device;
mod error;
mod handle;
mod sys;
pub mod transfer;
pub mod types;

pub use context::Context;
pub use descriptors::{ConfigDescriptor, DeviceDescriptor, EndpointDescriptor, Interface, InterfaceDescriptor};
pub use device::Device;
pub use error::{Error, ErrorKind, Result};
pub use handle::DeviceHandle;
pub use transfer::{Transfer, TransferFlags};
pub use types::{
    ControlSetup, ControlType, Direction, IsoPacket, NO_TIMEOUT, Recipient, Speed, TransferStatus, TransferType, Version, request_type,
};
