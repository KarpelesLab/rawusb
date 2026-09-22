//! Platform backends.
//!
//! Each backend module exposes the same items, which the platform-neutral
//! layer above uses without further `cfg` gating:
//!
//! - `Context`: `new() -> Result<Arc<Self>>`, `enumerate(&self) ->
//!   Result<Vec<DeviceInfo>>`, `open(&self, &Arc<DeviceInfo>) -> Result<Arc<Handle>>`.
//! - `DeviceInfo`: the enumeration snapshot with its public fields
//!   (`bus_number`, `address`, `port_numbers`, `speed`, `device_descriptor`,
//!   `configs`, `active_config`).
//! - `Handle`: configuration/interface operations, `submit`, `cancel`,
//!   `cancel_all`.
//! - `Context::watch_hotplug(&Arc<Self>, notify)` with the `hotplug` feature:
//!   arranges for `notify()` to be called whenever the set of attached
//!   devices may have changed. The neutral layer re-enumerates and diffs, so
//!   a backend may over-notify but must never miss a change.
//! - `TransferData`: per-transfer backend state, `Default`.
//! - `errno_kind(code) -> ErrorKind`.

use crate::descriptors::DeviceDescriptor;
use crate::types::Speed;

/// What enumeration learns about a device without opening it.
pub(crate) struct DeviceInfo {
    pub(crate) bus_number: u8,
    pub(crate) address: u8,
    pub(crate) port_numbers: Vec<u8>,
    pub(crate) speed: Speed,
    pub(crate) device_descriptor: DeviceDescriptor,
    /// Raw configuration descriptor trees, in index order.
    pub(crate) configs: Vec<Vec<u8>>,
    /// `bConfigurationValue` of the active configuration, when known.
    pub(crate) active_config: Option<u8>,
    /// Backend-specific locator used to open the device.
    pub(crate) location: Location,
}

/// Whether two enumeration snapshots describe the same attached device.
///
/// A device is identified by where it sits on the bus (its port chain) plus
/// its address and its vendor/product pair. Unplugging a device and plugging
/// a different one into the same port therefore reads as a departure and an
/// arrival, but swapping two identical devices between two ports may not.
#[cfg(feature = "hotplug")]
pub(crate) fn same_device(a: &DeviceInfo, b: &DeviceInfo) -> bool {
    a.bus_number == b.bus_number
        && a.address == b.address
        && a.port_numbers == b.port_numbers
        && a.device_descriptor.vendor_id == b.device_descriptor.vendor_id
        && a.device_descriptor.product_id == b.device_descriptor.product_id
}

/// Splits a concatenated run of configuration descriptors (as found in the
/// Linux sysfs `descriptors` file, or read back to back from a device) into
/// one `Vec` per configuration.
#[allow(dead_code)]
pub(crate) fn split_config_descriptors(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while data.len() >= 9 {
        if data[1] != crate::types::descriptor_type::CONFIG || data[0] < 9 {
            break;
        }
        let total = (u16::from_le_bytes([data[2], data[3]]) as usize).clamp(9, data.len());
        out.push(data[..total].to_vec());
        data = &data[total..];
    }
    out
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::*;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod macos;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) use macos::*;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
)))]
mod unsupported;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
)))]
pub(crate) use unsupported::*;
