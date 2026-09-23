//! Plumbing shared by the class helpers (`hid`, `msc`, `serial`, `uvc`):
//! finding an interface in the active configuration and holding a claim on
//! it for as long as the helper lives.

// Each helper uses a different subset of this module; with only one class
// feature enabled some items are legitimately unused.
#![cfg_attr(not(all(feature = "hid", feature = "msc", feature = "serial", feature = "uvc")), allow(dead_code))]

use crate::descriptors::{EndpointDescriptor, InterfaceDescriptor};
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::types::{Direction, TransferType};
use crate::{Error, ErrorKind, Result};

/// Interfaces claimed on behalf of a class helper, released (and their kernel
/// drivers re-attached) when dropped.
pub(crate) struct Claim {
    handle: DeviceHandle,
    interfaces: Vec<u8>,
}

impl Claim {
    /// Claims every listed interface, detaching kernel drivers as needed. On
    /// failure, whatever was already claimed is released again.
    pub(crate) fn new(handle: DeviceHandle, interfaces: &[u8]) -> Result<Claim> {
        let mut claim = Claim {
            handle,
            interfaces: Vec::with_capacity(interfaces.len()),
        };
        for &i in interfaces {
            if claim.interfaces.contains(&i) {
                continue;
            }
            claim.handle.claim_interface_detaching(i)?;
            claim.interfaces.push(i);
        }
        Ok(claim)
    }

    pub(crate) fn handle(&self) -> &DeviceHandle {
        &self.handle
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        for &i in &self.interfaces {
            let _ = self.handle.release_interface(i);
        }
    }
}

impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claim")
            .field("device", self.handle.device())
            .field("interfaces", &self.interfaces)
            .finish()
    }
}

/// The first interface (alternate setting 0) of the active configuration that
/// satisfies `pred`.
pub(crate) fn find_interface(
    device: &Device,
    what: &'static str,
    pred: impl Fn(&InterfaceDescriptor) -> bool,
) -> Result<InterfaceDescriptor> {
    let cfg = device.active_config_descriptor()?;
    cfg.interfaces
        .iter()
        .map(|i| i.first())
        .find(|a| pred(a))
        .cloned()
        .ok_or_else(|| Error::with_message(ErrorKind::NotFound, what))
}

/// Alternate setting 0 of interface `number` in the active configuration.
pub(crate) fn interface(device: &Device, number: u8) -> Result<InterfaceDescriptor> {
    let cfg = device.active_config_descriptor()?;
    cfg.interface(number)
        .map(|i| i.alt_setting(0).unwrap_or_else(|| i.first()).clone())
        .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "no such interface in the active configuration"))
}

/// The first endpoint of the given direction and type.
pub(crate) fn endpoint(iface: &InterfaceDescriptor, direction: Direction, kind: TransferType) -> Option<&EndpointDescriptor> {
    iface
        .endpoints
        .iter()
        .find(|e| e.direction() == direction && e.transfer_type() == kind)
}

/// Iterates over the class-specific descriptors in an `extra` blob as
/// `(bDescriptorType, whole descriptor)` pairs, stopping at malformed data.
pub(crate) fn descriptors(extra: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = extra;
    std::iter::from_fn(move || {
        if rest.len() < 2 {
            return None;
        }
        let len = rest[0] as usize;
        if len < 2 || len > rest.len() {
            return None;
        }
        let (d, tail) = rest.split_at(len);
        rest = tail;
        Some((d[1], d))
    })
}

/// Little-endian reader that tolerates short input (missing bytes read as 0), for
/// descriptors that devices routinely truncate.
pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b.get(at).copied().unwrap_or(0), b.get(at + 1).copied().unwrap_or(0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_walk_stops_at_garbage() {
        let extra = [3, 0x24, 1, 2, 0x21, 4, 5, 0x24];
        let v: Vec<_> = descriptors(&extra).collect();
        assert_eq!(v, vec![(0x24, &extra[..3]), (0x21, &extra[3..5])]);
        assert_eq!(le16(&[1], 0), 1);
    }
}
