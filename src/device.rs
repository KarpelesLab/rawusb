//! An enumerated but not necessarily opened device.

use crate::context::Context;
use crate::descriptors::{ConfigDescriptor, DeviceDescriptor};
use crate::handle::DeviceHandle;
use crate::sys;
use crate::types::Speed;
use crate::{Error, ErrorKind, Result};
use std::fmt;
use std::sync::Arc;

/// A USB device known to the system.
///
/// Devices are cheap to clone and carry a snapshot of the descriptors read
/// at enumeration time; nothing here touches the device itself. Call
/// [`open`](Self::open) to start talking to it.
#[derive(Clone)]
pub struct Device {
    ctx: Context,
    info: Arc<sys::DeviceInfo>,
}

impl Device {
    pub(crate) fn new(ctx: Context, info: Arc<sys::DeviceInfo>) -> Device {
        Device { ctx, info }
    }

    #[cfg(feature = "hotplug")]
    pub(crate) fn info(&self) -> &Arc<sys::DeviceInfo> {
        &self.info
    }

    /// The context this device was enumerated from.
    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// Bus number the device is attached to.
    pub fn bus_number(&self) -> u8 {
        self.info.bus_number
    }

    /// Address of the device on its bus.
    pub fn address(&self) -> u8 {
        self.info.address
    }

    /// Port number on the parent hub, or 0 for a root hub.
    pub fn port_number(&self) -> u8 {
        self.info.port_numbers.last().copied().unwrap_or(0)
    }

    /// The chain of port numbers from the root hub down to this device.
    /// Empty for a root hub.
    pub fn port_numbers(&self) -> &[u8] {
        &self.info.port_numbers
    }

    /// Negotiated bus speed.
    pub fn speed(&self) -> Speed {
        self.info.speed
    }

    /// The device descriptor.
    pub fn device_descriptor(&self) -> DeviceDescriptor {
        self.info.device_descriptor
    }

    /// `idVendor`, for convenience.
    pub fn vendor_id(&self) -> u16 {
        self.info.device_descriptor.vendor_id
    }

    /// `idProduct`, for convenience.
    pub fn product_id(&self) -> u16 {
        self.info.device_descriptor.product_id
    }

    /// The serial number string as the operating system knows it, without
    /// opening the device.
    ///
    /// Linux and macOS return the string the kernel read when the device
    /// arrived; Windows asks the device through its parent hub, which works
    /// whatever driver it is bound to. The answer is kept for the life of
    /// this `Device`. `None` means the device has no serial number
    /// (`iSerialNumber` is 0) or the OS could not tell; in the latter case
    /// [`DeviceHandle::read_serial_number_string`] may still work.
    pub fn serial_number(&self) -> Option<&str> {
        self.info
            .serial_number
            .get_or_init(|| {
                if self.info.device_descriptor.serial_number_string_index == 0 {
                    None
                } else {
                    sys::read_serial_number(&self.info)
                }
            })
            .as_deref()
    }

    /// Number of configuration descriptors that were readable at enumeration.
    pub fn num_configurations(&self) -> u8 {
        self.info.configs.len() as u8
    }

    /// The configuration descriptor at the given index (0-based, *not* the
    /// `bConfigurationValue`).
    pub fn config_descriptor(&self, index: u8) -> Result<ConfigDescriptor> {
        let raw = self
            .info
            .configs
            .get(index as usize)
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "no such configuration"))?;
        ConfigDescriptor::from_bytes(raw)
    }

    /// The configuration descriptor whose `bConfigurationValue` matches.
    pub fn config_descriptor_by_value(&self, value: u8) -> Result<ConfigDescriptor> {
        for raw in &self.info.configs {
            if raw.get(5) == Some(&value) {
                return ConfigDescriptor::from_bytes(raw);
            }
        }
        Err(Error::with_message(ErrorKind::NotFound, "no configuration with that value"))
    }

    /// The configuration that was active when the device was enumerated, or
    /// the first one if the OS does not expose that information.
    pub fn active_config_descriptor(&self) -> Result<ConfigDescriptor> {
        if let Some(v) = self.info.active_config
            && let Ok(c) = self.config_descriptor_by_value(v)
        {
            return Ok(c);
        }
        self.config_descriptor(0)
    }

    /// Opens the device for I/O.
    pub fn open(&self) -> Result<DeviceHandle> {
        let sys = self.ctx.sys().open(&self.info)?;
        Ok(DeviceHandle::new(self.clone(), sys))
    }
}

impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.info, &other.info)
            || (self.info.bus_number == other.info.bus_number
                && self.info.address == other.info.address
                && self.info.port_numbers == other.info.port_numbers)
    }
}

impl Eq for Device {}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("bus", &self.info.bus_number)
            .field("address", &self.info.address)
            .field("ports", &self.info.port_numbers)
            .field("vendor_id", &format_args!("{:#06x}", self.vendor_id()))
            .field("product_id", &format_args!("{:#06x}", self.product_id()))
            .field("speed", &self.info.speed)
            .finish()
    }
}
