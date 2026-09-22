//! Small value types: directions, transfer kinds, speeds, control setup
//! packets, and the standard-request constants from the USB specification.

use std::fmt;
use std::time::Duration;

/// Direction of a transfer or endpoint, as seen from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Host to device.
    Out,
    /// Device to host.
    In,
}

impl Direction {
    /// Extracts the direction encoded in bit 7 of an endpoint address or a
    /// `bmRequestType`.
    pub const fn from_address(address: u8) -> Self {
        if address & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }

    /// The bit-7 mask for this direction (`0x80` for IN, `0` for OUT).
    pub const fn mask(self) -> u8 {
        match self {
            Direction::Out => 0x00,
            Direction::In => 0x80,
        }
    }
}

/// The four USB transfer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferType {
    /// Control transfer (always on endpoint 0 in practice).
    Control,
    /// Isochronous transfer.
    Isochronous,
    /// Bulk transfer.
    Bulk,
    /// Interrupt transfer.
    Interrupt,
}

impl TransferType {
    /// Decodes the low two bits of an endpoint descriptor's `bmAttributes`.
    pub const fn from_attributes(attributes: u8) -> Self {
        match attributes & 0x03 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }
}

/// Synchronisation type of an isochronous endpoint (`bmAttributes` bits 2-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncType {
    /// No synchronisation.
    None,
    /// Asynchronous.
    Asynchronous,
    /// Adaptive.
    Adaptive,
    /// Synchronous.
    Synchronous,
}

/// Usage type of an isochronous or interrupt endpoint (`bmAttributes` bits 4-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageType {
    /// Data endpoint (isochronous) or periodic (interrupt).
    Data,
    /// Feedback endpoint (isochronous) or notification (interrupt).
    Feedback,
    /// Implicit feedback data endpoint.
    Implicit,
    /// Reserved value.
    Reserved,
}

/// Negotiated bus speed of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Speed {
    /// The operating system did not report a speed.
    Unknown,
    /// USB 1.0 low speed, 1.5 Mbit/s.
    Low,
    /// USB 1.1 full speed, 12 Mbit/s.
    Full,
    /// USB 2.0 high speed, 480 Mbit/s.
    High,
    /// USB 3.0 SuperSpeed, 5 Gbit/s.
    Super,
    /// USB 3.1 SuperSpeed+, 10 Gbit/s.
    SuperPlus,
    /// USB 3.2 SuperSpeed+ dual-lane, 20 Gbit/s.
    SuperPlusX2,
}

impl Speed {
    /// Nominal signalling rate in bits per second, or `None` when unknown.
    pub const fn bits_per_second(self) -> Option<u64> {
        match self {
            Speed::Unknown => None,
            Speed::Low => Some(1_500_000),
            Speed::Full => Some(12_000_000),
            Speed::High => Some(480_000_000),
            Speed::Super => Some(5_000_000_000),
            Speed::SuperPlus => Some(10_000_000_000),
            Speed::SuperPlusX2 => Some(20_000_000_000),
        }
    }

    /// Maps a rate in Mbit/s (as reported by Linux sysfs) to a `Speed`.
    pub fn from_mbps(mbps: f64) -> Self {
        // Compare on integers so a "1.5" parses cleanly.
        match (mbps * 10.0) as u64 {
            15 => Speed::Low,
            120 => Speed::Full,
            4800 => Speed::High,
            50000 => Speed::Super,
            100000 => Speed::SuperPlus,
            200000 => Speed::SuperPlusX2,
            _ => Speed::Unknown,
        }
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Speed::Unknown => "unknown",
            Speed::Low => "1.5 Mbit/s (low)",
            Speed::Full => "12 Mbit/s (full)",
            Speed::High => "480 Mbit/s (high)",
            Speed::Super => "5 Gbit/s (super)",
            Speed::SuperPlus => "10 Gbit/s (super+)",
            Speed::SuperPlusX2 => "20 Gbit/s (super+ x2)",
        })
    }
}

/// Type field of a control request (`bmRequestType` bits 5-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlType {
    /// Standard request defined by the USB specification.
    Standard,
    /// Class-specific request.
    Class,
    /// Vendor-specific request.
    Vendor,
    /// Reserved.
    Reserved,
}

/// Recipient field of a control request (`bmRequestType` bits 0-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recipient {
    /// The device as a whole.
    Device,
    /// An interface (index in the low byte of `wIndex`).
    Interface,
    /// An endpoint (address in the low byte of `wIndex`).
    Endpoint,
    /// Other.
    Other,
}

/// Builds a `bmRequestType` byte from its three components.
pub const fn request_type(direction: Direction, kind: ControlType, recipient: Recipient) -> u8 {
    let dir = direction.mask();
    let ty = match kind {
        ControlType::Standard => 0 << 5,
        ControlType::Class => 1 << 5,
        ControlType::Vendor => 2 << 5,
        ControlType::Reserved => 3 << 5,
    };
    let rec = match recipient {
        Recipient::Device => 0,
        Recipient::Interface => 1,
        Recipient::Endpoint => 2,
        Recipient::Other => 3,
    };
    dir | ty | rec
}

/// The 8-byte SETUP packet of a control transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ControlSetup {
    /// `bmRequestType`: direction, type and recipient.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`: number of data bytes that follow the setup packet.
    pub length: u16,
}

impl ControlSetup {
    /// Size in bytes of an encoded setup packet.
    pub const SIZE: usize = 8;

    /// Creates a setup packet from its components.
    pub const fn new(
        direction: Direction,
        kind: ControlType,
        recipient: Recipient,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Self {
        ControlSetup {
            request_type: request_type(direction, kind, recipient),
            request,
            value,
            index,
            length,
        }
    }

    /// Creates a setup packet from a raw `bmRequestType` byte.
    pub const fn raw(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        ControlSetup {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    /// Direction of the data stage.
    pub const fn direction(&self) -> Direction {
        Direction::from_address(self.request_type)
    }

    /// Encodes the packet in wire (little-endian) order.
    pub const fn to_bytes(&self) -> [u8; 8] {
        let v = self.value.to_le_bytes();
        let i = self.index.to_le_bytes();
        let l = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            v[0],
            v[1],
            i[0],
            i[1],
            l[0],
            l[1],
        ]
    }

    /// Decodes a packet from wire order.
    pub const fn from_bytes(b: [u8; 8]) -> Self {
        ControlSetup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
}

/// Final state of a completed transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransferStatus {
    /// The transfer completed. It may still be shorter than requested; check
    /// the actual length.
    Completed,
    /// The transfer failed for an unspecified reason (protocol error, babble,
    /// device error, ...).
    Error,
    /// The transfer's own timeout elapsed.
    TimedOut,
    /// The transfer was cancelled by the caller.
    Cancelled,
    /// The endpoint is halted, or the device rejected the control request.
    Stall,
    /// The device was disconnected.
    NoDevice,
    /// The device sent more data than the buffer could hold.
    Overflow,
}

impl TransferStatus {
    /// `true` only for [`TransferStatus::Completed`].
    pub const fn is_ok(self) -> bool {
        matches!(self, TransferStatus::Completed)
    }

    /// Converts a non-successful status into the matching [`crate::Error`].
    pub fn into_result(self) -> crate::Result<()> {
        use crate::ErrorKind as K;
        let kind = match self {
            TransferStatus::Completed => return Ok(()),
            TransferStatus::Error => K::Io,
            TransferStatus::TimedOut => K::Timeout,
            TransferStatus::Cancelled => K::Interrupted,
            TransferStatus::Stall => K::Pipe,
            TransferStatus::NoDevice => K::NoDevice,
            TransferStatus::Overflow => K::Overflow,
        };
        Err(crate::Error::new(kind))
    }
}

impl fmt::Display for TransferStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TransferStatus::Completed => "completed",
            TransferStatus::Error => "error",
            TransferStatus::TimedOut => "timed out",
            TransferStatus::Cancelled => "cancelled",
            TransferStatus::Stall => "stall",
            TransferStatus::NoDevice => "no device",
            TransferStatus::Overflow => "overflow",
        })
    }
}

/// One packet of an isochronous transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoPacket {
    /// Bytes reserved for this packet in the transfer buffer.
    pub length: u32,
    /// Bytes actually transferred, filled in on completion.
    pub actual_length: u32,
    /// Per-packet status, filled in on completion.
    pub status: TransferStatus,
}

impl IsoPacket {
    /// A packet of the given length that has not run yet.
    pub const fn new(length: u32) -> Self {
        IsoPacket {
            length,
            actual_length: 0,
            status: TransferStatus::Error,
        }
    }
}

/// Timeout that means "wait forever", mirroring libusb's `timeout = 0`.
pub const NO_TIMEOUT: Duration = Duration::ZERO;

/// A USB `bcdXXX` version number (e.g. `bcdUSB`, `bcdDevice`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version(pub u16);

impl Version {
    /// Major digit(s) (the high byte, decoded from BCD).
    pub const fn major(self) -> u8 {
        let b = (self.0 >> 8) as u8;
        (b >> 4) * 10 + (b & 0x0f)
    }

    /// Minor digit (high nibble of the low byte).
    pub const fn minor(self) -> u8 {
        ((self.0 >> 4) & 0x0f) as u8
    }

    /// Sub-minor digit (low nibble of the low byte).
    pub const fn sub_minor(self) -> u8 {
        (self.0 & 0x0f) as u8
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Version({self})")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major(), self.minor())?;
        if self.sub_minor() != 0 {
            write!(f, ".{}", self.sub_minor())?;
        }
        Ok(())
    }
}

/// Standard request codes (`bRequest`) from USB 2.0 §9.4.
pub mod request {
    /// GET_STATUS
    pub const GET_STATUS: u8 = 0x00;
    /// CLEAR_FEATURE
    pub const CLEAR_FEATURE: u8 = 0x01;
    /// SET_FEATURE
    pub const SET_FEATURE: u8 = 0x03;
    /// SET_ADDRESS
    pub const SET_ADDRESS: u8 = 0x05;
    /// GET_DESCRIPTOR
    pub const GET_DESCRIPTOR: u8 = 0x06;
    /// SET_DESCRIPTOR
    pub const SET_DESCRIPTOR: u8 = 0x07;
    /// GET_CONFIGURATION
    pub const GET_CONFIGURATION: u8 = 0x08;
    /// SET_CONFIGURATION
    pub const SET_CONFIGURATION: u8 = 0x09;
    /// GET_INTERFACE
    pub const GET_INTERFACE: u8 = 0x0a;
    /// SET_INTERFACE
    pub const SET_INTERFACE: u8 = 0x0b;
    /// SYNCH_FRAME
    pub const SYNCH_FRAME: u8 = 0x0c;
    /// SET_SEL (USB 3)
    pub const SET_SEL: u8 = 0x30;
    /// SET_ISOCH_DELAY (USB 3)
    pub const SET_ISOCH_DELAY: u8 = 0x31;
}

/// Descriptor type codes (`bDescriptorType`).
pub mod descriptor_type {
    /// Device descriptor.
    pub const DEVICE: u8 = 0x01;
    /// Configuration descriptor.
    pub const CONFIG: u8 = 0x02;
    /// String descriptor.
    pub const STRING: u8 = 0x03;
    /// Interface descriptor.
    pub const INTERFACE: u8 = 0x04;
    /// Endpoint descriptor.
    pub const ENDPOINT: u8 = 0x05;
    /// Device qualifier (USB 2.0 only).
    pub const DEVICE_QUALIFIER: u8 = 0x06;
    /// Other-speed configuration (USB 2.0 only).
    pub const OTHER_SPEED_CONFIG: u8 = 0x07;
    /// Interface power.
    pub const INTERFACE_POWER: u8 = 0x08;
    /// OTG descriptor.
    pub const OTG: u8 = 0x09;
    /// Debug descriptor.
    pub const DEBUG: u8 = 0x0a;
    /// Interface association descriptor.
    pub const INTERFACE_ASSOCIATION: u8 = 0x0b;
    /// Binary Object Store (USB 3 / 2.1).
    pub const BOS: u8 = 0x0f;
    /// Device capability (inside a BOS).
    pub const DEVICE_CAPABILITY: u8 = 0x10;
    /// HID descriptor.
    pub const HID: u8 = 0x21;
    /// HID report descriptor.
    pub const HID_REPORT: u8 = 0x22;
    /// SuperSpeed endpoint companion.
    pub const SS_ENDPOINT_COMPANION: u8 = 0x30;
    /// SuperSpeedPlus isochronous endpoint companion.
    pub const SSP_ISOCH_ENDPOINT_COMPANION: u8 = 0x31;
}

/// Well-known device/interface class codes (`bDeviceClass`, `bInterfaceClass`).
pub mod class {
    /// Class is defined per interface.
    pub const PER_INTERFACE: u8 = 0x00;
    /// Audio.
    pub const AUDIO: u8 = 0x01;
    /// Communications (CDC).
    pub const COMM: u8 = 0x02;
    /// Human interface device.
    pub const HID: u8 = 0x03;
    /// Physical.
    pub const PHYSICAL: u8 = 0x05;
    /// Still image (PTP).
    pub const IMAGE: u8 = 0x06;
    /// Printer.
    pub const PRINTER: u8 = 0x07;
    /// Mass storage.
    pub const MASS_STORAGE: u8 = 0x08;
    /// Hub.
    pub const HUB: u8 = 0x09;
    /// CDC data.
    pub const DATA: u8 = 0x0a;
    /// Smart card.
    pub const SMART_CARD: u8 = 0x0b;
    /// Content security.
    pub const CONTENT_SECURITY: u8 = 0x0d;
    /// Video.
    pub const VIDEO: u8 = 0x0e;
    /// Personal healthcare.
    pub const PERSONAL_HEALTHCARE: u8 = 0x0f;
    /// Diagnostic device.
    pub const DIAGNOSTIC: u8 = 0xdc;
    /// Wireless controller.
    pub const WIRELESS: u8 = 0xe0;
    /// Miscellaneous.
    pub const MISCELLANEOUS: u8 = 0xef;
    /// Application specific.
    pub const APPLICATION: u8 = 0xfe;
    /// Vendor specific.
    pub const VENDOR_SPECIFIC: u8 = 0xff;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_roundtrip() {
        let s = ControlSetup::new(
            Direction::In,
            ControlType::Standard,
            Recipient::Device,
            request::GET_DESCRIPTOR,
            0x0100,
            0,
            18,
        );
        assert_eq!(s.request_type, 0x80);
        let b = s.to_bytes();
        assert_eq!(b, [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0]);
        assert_eq!(ControlSetup::from_bytes(b), s);
        assert_eq!(s.direction(), Direction::In);
    }

    #[test]
    fn request_type_bits() {
        assert_eq!(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            0x21
        );
        assert_eq!(
            request_type(Direction::In, ControlType::Vendor, Recipient::Endpoint),
            0xc2
        );
    }

    #[test]
    fn version_bcd() {
        let v = Version(0x0210);
        assert_eq!(v.major(), 2);
        assert_eq!(v.minor(), 1);
        assert_eq!(v.sub_minor(), 0);
        assert_eq!(v.to_string(), "2.1");
        assert_eq!(Version(0x0312).to_string(), "3.1.2");
        assert_eq!(Version(0x1234).major(), 12);
    }

    #[test]
    fn speed_from_mbps() {
        assert_eq!(Speed::from_mbps(1.5), Speed::Low);
        assert_eq!(Speed::from_mbps(12.0), Speed::Full);
        assert_eq!(Speed::from_mbps(480.0), Speed::High);
        assert_eq!(Speed::from_mbps(5000.0), Speed::Super);
        assert_eq!(Speed::from_mbps(10000.0), Speed::SuperPlus);
        assert_eq!(Speed::from_mbps(20000.0), Speed::SuperPlusX2);
        assert_eq!(Speed::from_mbps(7.0), Speed::Unknown);
    }
}
