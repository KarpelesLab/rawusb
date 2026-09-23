//! Human Interface Devices (HID 1.11) over raw USB: keyboards, mice, game
//! controllers, FIDO tokens, UPS and PSU telemetry, vendor HID gadgets.
//!
//! This module is behind the `hid` cargo feature.
//!
//! [`HidDevice`] claims a HID interface, reads its report descriptor, and
//! exchanges reports the way hidapi does: the first byte of every report you
//! *send* is its report ID (0 when the device does not use IDs, in which case
//! it is not transmitted), and reports you *read* start with their ID byte
//! only when the device uses IDs. [`ReportDescriptor`] parses the report
//! descriptor so that fields can be decoded without hard-coding offsets.
//!
//! ```no_run
//! use rawusb::hid::{HidDevice, ReportType};
//! use std::time::Duration;
//!
//! let ctx = rawusb::Context::new()?;
//! let dev = ctx.find_device(0x1b1c, 0x1c27)?.expect("device not plugged in");
//! let hid = HidDevice::open(&dev)?;
//! println!("{:?}", hid.report_descriptor().application_usages);
//! let mut report = [0u8; 64];
//! let n = hid.read(&mut report, Duration::from_secs(1))?;
//! println!("input report: {:02x?}", &report[..n]);
//! # Ok::<(), rawusb::Error>(())
//! ```
//!
//! # Platform notes
//!
//! The operating system's HID driver normally owns these interfaces. On
//! Linux, [`HidDevice`] detaches it while open and re-attaches it on drop. On
//! macOS and Windows the HID driver cannot be displaced, so this only works
//! for devices bound to a generic driver (WinUSB on Windows); use the OS HID
//! API (hidraw, IOHIDManager, `hid.dll`) for devices that keep their HID
//! driver.

mod report;

pub use report::{Field, Item, ItemType, ReportDescriptor, Usage, Usages, items, usage};

use crate::class::{self, Claim};
use crate::descriptors::InterfaceDescriptor;
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::types::{ControlType, Direction, Recipient, TransferType, descriptor_type, request, request_type};
use crate::{Error, ErrorKind, Result};
use std::time::Duration;

/// Timeout for the descriptor reads done while opening.
const OPEN_TIMEOUT: Duration = Duration::from_secs(1);

/// HID class requests (HID 1.11 §7.2).
mod req {
    pub(super) const GET_REPORT: u8 = 0x01;
    pub(super) const GET_IDLE: u8 = 0x02;
    pub(super) const GET_PROTOCOL: u8 = 0x03;
    pub(super) const SET_REPORT: u8 = 0x09;
    pub(super) const SET_IDLE: u8 = 0x0a;
    pub(super) const SET_PROTOCOL: u8 = 0x0b;
}

/// The three report types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportType {
    /// Device to host, normally over the interrupt IN endpoint.
    Input,
    /// Host to device, over the interrupt OUT endpoint if there is one.
    Output,
    /// Configuration or status, over the control endpoint in either direction.
    Feature,
}

impl ReportType {
    /// The value used in the high byte of `wValue` by GET_REPORT/SET_REPORT.
    pub const fn code(self) -> u8 {
        match self {
            ReportType::Input => 1,
            ReportType::Output => 2,
            ReportType::Feature => 3,
        }
    }
}

/// The protocol a boot-capable device speaks (HID 1.11 §7.2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// The fixed boot keyboard / mouse report format.
    Boot,
    /// The format described by the report descriptor (the default).
    Report,
}

/// Boot interface kind, from `bInterfaceProtocol` of a boot-subclass interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootDevice {
    /// Boot keyboard.
    Keyboard,
    /// Boot mouse.
    Mouse,
}

/// The HID class descriptor (type 0x21) that follows a HID interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDescriptor {
    /// `bcdHID`: HID specification release.
    pub hid_version: crate::types::Version,
    /// `bCountryCode`: 0 unless the hardware is localised (keyboards).
    pub country_code: u8,
    /// `(bDescriptorType, wDescriptorLength)` of each class descriptor; the
    /// report descriptor (type 0x22) is always among them.
    pub descriptors: Vec<(u8, u16)>,
}

impl HidDescriptor {
    /// Parses the HID descriptor from its wire encoding.
    pub fn from_bytes(b: &[u8]) -> Result<HidDescriptor> {
        if b.len() < 6 || b[1] != descriptor_type::HID {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a HID descriptor"));
        }
        let count = b[5] as usize;
        let descriptors = (0..count)
            .map_while(|i| {
                let at = 6 + 3 * i;
                (at + 3 <= b.len()).then(|| (b[at], class::le16(b, at + 1)))
            })
            .collect();
        Ok(HidDescriptor {
            hid_version: crate::types::Version(class::le16(b, 2)),
            country_code: b[4],
            descriptors,
        })
    }

    /// Length of the report descriptor, as declared.
    pub fn report_descriptor_length(&self) -> Option<u16> {
        self.descriptors.iter().find(|d| d.0 == descriptor_type::HID_REPORT).map(|d| d.1)
    }

    /// Finds the HID descriptor among an interface's class-specific
    /// descriptors (or, for a few old devices, after its endpoints).
    pub fn of_interface(iface: &InterfaceDescriptor) -> Option<HidDescriptor> {
        std::iter::once(&iface.extra)
            .chain(iface.endpoints.iter().map(|e| &e.extra))
            .flat_map(|extra| class::descriptors(extra))
            .find(|(ty, _)| *ty == descriptor_type::HID)
            .and_then(|(_, d)| HidDescriptor::from_bytes(d).ok())
    }
}

/// The HID interfaces of a device (alternate setting 0 of each), in
/// descriptor order. Many devices have several: a keyboard's media keys, a
/// receiver's paired devices, or a gadget's separate raw HID channels. Open
/// one with [`HidDevice::open_interface`], or all with
/// [`HidDevice::open_all`].
pub fn interfaces(device: &Device) -> Result<Vec<InterfaceDescriptor>> {
    class::interfaces_where(device, is_hid)
}

fn is_hid(a: &InterfaceDescriptor) -> bool {
    a.class == crate::types::class::HID
}

/// An open HID interface. See the [module documentation](self).
///
/// The interface is claimed (detaching the kernel driver if necessary) for as
/// long as this value lives. Methods take `&self`, so one thread can block in
/// [`read`](Self::read) while another sends reports.
#[derive(Debug)]
pub struct HidDevice {
    claim: Claim,
    interface: InterfaceDescriptor,
    hid: HidDescriptor,
    report_descriptor_raw: Vec<u8>,
    report_descriptor: ReportDescriptor,
    ep_in: u8,
    ep_in_size: usize,
    ep_out: Option<u8>,
}

impl HidDevice {
    /// Opens the first HID interface of a device.
    pub fn open(device: &Device) -> Result<HidDevice> {
        let iface = class::find_interface(device, "device has no HID interface", is_hid)?;
        Self::open_interface(device.open()?, iface.number)
    }

    /// Opens every HID interface of a device through one handle. Fails (and
    /// opens none) if any of them cannot be opened.
    pub fn open_all(handle: &DeviceHandle) -> Result<Vec<HidDevice>> {
        interfaces(handle.device())?
            .iter()
            .map(|i| Self::open_interface(handle.clone(), i.number))
            .collect()
    }

    /// Opens a specific HID interface of an already-open device. Devices with
    /// several HID interfaces (a keyboard with media keys, a mouse receiver
    /// for several devices) expose one per function.
    pub fn open_interface(handle: DeviceHandle, interface: u8) -> Result<HidDevice> {
        let iface = class::interface(handle.device(), interface)?;
        if !is_hid(&iface) {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a HID interface"));
        }
        let hid =
            HidDescriptor::of_interface(&iface).ok_or_else(|| Error::with_message(ErrorKind::Io, "HID interface has no HID descriptor"))?;
        let ep_in = class::endpoint(&iface, Direction::In, TransferType::Interrupt)
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "HID interface has no interrupt IN endpoint"))?;
        let (ep_in, ep_in_size) = (ep_in.address, ep_in.max_packet_size() as usize);
        let ep_out = class::endpoint(&iface, Direction::Out, TransferType::Interrupt).map(|e| e.address);

        let claim = Claim::new(handle, &[interface])?;
        let len = hid.report_descriptor_length().unwrap_or(4096).max(1) as usize;
        let mut raw = vec![0u8; len];
        let n = claim.handle().control_read(
            request_type(Direction::In, ControlType::Standard, Recipient::Interface),
            request::GET_DESCRIPTOR,
            (descriptor_type::HID_REPORT as u16) << 8,
            interface as u16,
            &mut raw,
            OPEN_TIMEOUT,
        )?;
        raw.truncate(n);
        let report_descriptor = ReportDescriptor::parse(&raw)?;
        Ok(HidDevice {
            claim,
            interface: iface,
            hid,
            report_descriptor_raw: raw,
            report_descriptor,
            ep_in,
            ep_in_size,
            ep_out,
        })
    }

    /// The underlying device handle.
    pub fn handle(&self) -> &DeviceHandle {
        self.claim.handle()
    }

    /// The claimed interface number.
    pub fn interface_number(&self) -> u8 {
        self.interface.number
    }

    /// The HID class descriptor.
    pub fn hid_descriptor(&self) -> &HidDescriptor {
        &self.hid
    }

    /// Whether the interface supports the boot protocol, and as what.
    pub fn boot_device(&self) -> Option<BootDevice> {
        match (self.interface.sub_class, self.interface.protocol) {
            (1, 1) => Some(BootDevice::Keyboard),
            (1, 2) => Some(BootDevice::Mouse),
            _ => None,
        }
    }

    /// The parsed report descriptor.
    pub fn report_descriptor(&self) -> &ReportDescriptor {
        &self.report_descriptor
    }

    /// The report descriptor exactly as the device sent it.
    pub fn raw_report_descriptor(&self) -> &[u8] {
        &self.report_descriptor_raw
    }

    /// Size of the largest input report on the wire, report ID included;
    /// a buffer this large never truncates a [`read`](Self::read).
    pub fn max_input_report_len(&self) -> usize {
        let d = &self.report_descriptor;
        let id = usize::from(d.uses_report_ids());
        d.report_ids(ReportType::Input)
            .into_iter()
            .filter_map(|i| d.report_len(ReportType::Input, i))
            .max()
            .map_or(self.ep_in_size, |n| n + id)
            .max(self.ep_in_size)
    }

    /// Reads one input report from the interrupt IN endpoint. The report
    /// starts with its ID byte if the device uses report IDs. Returns the
    /// report length; fails with [`ErrorKind::Timeout`] if none arrives in
    /// time.
    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.handle().interrupt_read(self.ep_in, buf, timeout)
    }

    /// Sends an output report. `data[0]` is the report ID; when it is 0 (the
    /// device does not use IDs) it is stripped before sending. Uses the
    /// interrupt OUT endpoint when the interface has one, otherwise a
    /// SET_REPORT control request. Returns the bytes sent, ID byte included.
    pub fn write(&self, data: &[u8], timeout: Duration) -> Result<usize> {
        let (&id, _) = data
            .split_first()
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "report must start with its ID byte"))?;
        let wire = if id == 0 { &data[1..] } else { data };
        let n = match self.ep_out {
            Some(ep) => self.handle().interrupt_write(ep, wire, timeout)?,
            None => self.set_report(ReportType::Output, id, wire, timeout)?,
        };
        Ok(n + usize::from(id == 0))
    }

    /// Reads a feature report, hidapi style: set `buf[0]` to the report ID;
    /// on return `buf` holds the report with its ID in `buf[0]` (even for ID
    /// 0). Returns the length including the ID byte.
    pub fn get_feature_report(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.get_report_prefixed(ReportType::Feature, buf, timeout)
    }

    /// Sends a feature report. `data[0]` is the report ID, stripped when 0.
    /// Returns the bytes sent, ID byte included.
    pub fn send_feature_report(&self, data: &[u8], timeout: Duration) -> Result<usize> {
        let (&id, _) = data
            .split_first()
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "report must start with its ID byte"))?;
        let wire = if id == 0 { &data[1..] } else { data };
        Ok(self.set_report(ReportType::Feature, id, wire, timeout)? + usize::from(id == 0))
    }

    /// Polls an input report over the control endpoint, hidapi style (see
    /// [`get_feature_report`](Self::get_feature_report)).
    pub fn get_input_report(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.get_report_prefixed(ReportType::Input, buf, timeout)
    }

    fn get_report_prefixed(&self, kind: ReportType, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let (&mut id, rest) = buf
            .split_first_mut()
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "buffer must have room for the ID byte"))?;
        if id == 0 {
            Ok(self.get_report(kind, 0, rest, timeout)? + 1)
        } else {
            self.get_report(kind, id, buf, timeout)
        }
    }

    /// Raw GET_REPORT: the report exactly as the device returns it (starting
    /// with the ID byte when the device uses report IDs).
    pub fn get_report(&self, kind: ReportType, report_id: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.handle().control_read(
            request_type(Direction::In, ControlType::Class, Recipient::Interface),
            req::GET_REPORT,
            (kind.code() as u16) << 8 | report_id as u16,
            self.interface.number as u16,
            buf,
            timeout,
        )
    }

    /// Raw SET_REPORT: `data` is sent unchanged (it must start with the ID
    /// byte when the device uses report IDs).
    pub fn set_report(&self, kind: ReportType, report_id: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            req::SET_REPORT,
            (kind.code() as u16) << 8 | report_id as u16,
            self.interface.number as u16,
            data,
            timeout,
        )
    }

    /// SET_IDLE: limits how often the device repeats an unchanged input
    /// report. `None` means only on change; otherwise the rate is rounded to
    /// 4 ms units (max 1.02 s). `report_id` 0 applies to every report.
    pub fn set_idle(&self, rate: Option<Duration>, report_id: u8, timeout: Duration) -> Result<()> {
        let units = rate.map_or(0, |d| (d.as_millis() / 4).clamp(1, 255) as u16);
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            req::SET_IDLE,
            units << 8 | report_id as u16,
            self.interface.number as u16,
            &[],
            timeout,
        )?;
        Ok(())
    }

    /// GET_IDLE: the current idle rate for a report; `None` means "only on
    /// change".
    pub fn get_idle(&self, report_id: u8, timeout: Duration) -> Result<Option<Duration>> {
        let mut b = [0u8; 1];
        self.class_read(req::GET_IDLE, report_id as u16, &mut b, timeout)?;
        Ok((b[0] != 0).then(|| Duration::from_millis(b[0] as u64 * 4)))
    }

    /// SET_PROTOCOL: switches a boot-capable device between boot and report
    /// protocol.
    pub fn set_protocol(&self, protocol: Protocol, timeout: Duration) -> Result<()> {
        let value = match protocol {
            Protocol::Boot => 0,
            Protocol::Report => 1,
        };
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            req::SET_PROTOCOL,
            value,
            self.interface.number as u16,
            &[],
            timeout,
        )?;
        Ok(())
    }

    /// GET_PROTOCOL.
    pub fn get_protocol(&self, timeout: Duration) -> Result<Protocol> {
        let mut b = [0u8; 1];
        self.class_read(req::GET_PROTOCOL, 0, &mut b, timeout)?;
        Ok(if b[0] == 0 { Protocol::Boot } else { Protocol::Report })
    }

    fn class_read(&self, request: u8, value: u16, buf: &mut [u8], timeout: Duration) -> Result<()> {
        let n = self.handle().control_read(
            request_type(Direction::In, ControlType::Class, Recipient::Interface),
            request,
            value,
            self.interface.number as u16,
            buf,
            timeout,
        )?;
        if n < buf.len() {
            return Err(Error::with_message(ErrorKind::Io, "short response to a HID class request"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_descriptor() {
        let d = HidDescriptor::from_bytes(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x3f, 0]).unwrap();
        assert_eq!(d.hid_version, crate::types::Version(0x0111));
        assert_eq!(d.report_descriptor_length(), Some(63));
        // Truncated class descriptor list: keep what is there.
        let d = HidDescriptor::from_bytes(&[7, 0x21, 0x11, 0x01, 0, 2, 0x22]).unwrap();
        assert!(d.descriptors.is_empty());
        assert!(HidDescriptor::from_bytes(&[9, 0x24, 0, 0, 0, 0]).is_err());
    }
}
