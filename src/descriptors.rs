//! Standard USB descriptors and a tolerant parser for configuration trees.
//!
//! Parsing follows libusb's rules: unknown or class-specific descriptors that
//! appear between the standard ones are preserved verbatim in the `extra`
//! field of the enclosing configuration, interface or endpoint, and malformed
//! trailing data is ignored rather than rejected.

use crate::types::{Direction, SyncType, TransferType, UsageType, Version, descriptor_type};
use crate::{Error, ErrorKind, Result};
use std::fmt;

/// The standard 18-byte device descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceDescriptor {
    /// `bcdUSB`: USB specification release the device complies with.
    pub usb_version: Version,
    /// `bDeviceClass`.
    pub class: u8,
    /// `bDeviceSubClass`.
    pub sub_class: u8,
    /// `bDeviceProtocol`.
    pub protocol: u8,
    /// `bMaxPacketSize0`: maximum packet size of endpoint 0.
    pub max_packet_size_0: u8,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `bcdDevice`: device release number.
    pub device_version: Version,
    /// `iManufacturer`: string descriptor index, 0 if none.
    pub manufacturer_string_index: u8,
    /// `iProduct`: string descriptor index, 0 if none.
    pub product_string_index: u8,
    /// `iSerialNumber`: string descriptor index, 0 if none.
    pub serial_number_string_index: u8,
    /// `bNumConfigurations`.
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    /// Length in bytes of the descriptor on the wire.
    pub const SIZE: usize = 18;

    /// Parses the descriptor from its wire encoding.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < Self::SIZE {
            return Err(Error::with_message(ErrorKind::InvalidParam, "device descriptor too short"));
        }
        if b[0] < Self::SIZE as u8 || b[1] != descriptor_type::DEVICE {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a device descriptor"));
        }
        Ok(DeviceDescriptor {
            usb_version: Version(u16::from_le_bytes([b[2], b[3]])),
            class: b[4],
            sub_class: b[5],
            protocol: b[6],
            max_packet_size_0: b[7],
            vendor_id: u16::from_le_bytes([b[8], b[9]]),
            product_id: u16::from_le_bytes([b[10], b[11]]),
            device_version: Version(u16::from_le_bytes([b[12], b[13]])),
            manufacturer_string_index: b[14],
            product_string_index: b[15],
            serial_number_string_index: b[16],
            num_configurations: b[17],
        })
    }

    /// Serialises the descriptor to its wire encoding.
    pub fn to_bytes(&self) -> [u8; 18] {
        let mut b = [0u8; 18];
        b[0] = 18;
        b[1] = descriptor_type::DEVICE;
        b[2..4].copy_from_slice(&self.usb_version.0.to_le_bytes());
        b[4] = self.class;
        b[5] = self.sub_class;
        b[6] = self.protocol;
        b[7] = self.max_packet_size_0;
        b[8..10].copy_from_slice(&self.vendor_id.to_le_bytes());
        b[10..12].copy_from_slice(&self.product_id.to_le_bytes());
        b[12..14].copy_from_slice(&self.device_version.0.to_le_bytes());
        b[14] = self.manufacturer_string_index;
        b[15] = self.product_string_index;
        b[16] = self.serial_number_string_index;
        b[17] = self.num_configurations;
        b
    }
}

/// A parsed configuration descriptor with its interfaces and endpoints.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfigDescriptor {
    /// `wTotalLength` as declared by the device.
    pub total_length: u16,
    /// `bNumInterfaces` as declared by the device.
    pub num_interfaces: u8,
    /// `bConfigurationValue`: the value to pass to `SET_CONFIGURATION`.
    pub configuration_value: u8,
    /// `iConfiguration`: string descriptor index, 0 if none.
    pub string_index: u8,
    /// `bmAttributes`.
    pub attributes: u8,
    /// `bMaxPower` in units of 2 mA (8 mA for SuperSpeed devices).
    pub max_power: u8,
    /// Interfaces, grouped by interface number with their alternate settings.
    pub interfaces: Vec<Interface>,
    /// Descriptors that follow the configuration header but precede the first
    /// interface (for example interface association descriptors).
    pub extra: Vec<u8>,
    raw: Vec<u8>,
}

impl ConfigDescriptor {
    /// `true` if the device is self-powered in this configuration.
    pub const fn self_powered(&self) -> bool {
        self.attributes & 0x40 != 0
    }

    /// `true` if the device supports remote wakeup in this configuration.
    pub const fn remote_wakeup(&self) -> bool {
        self.attributes & 0x20 != 0
    }

    /// Maximum current draw in milliamps, assuming USB 2.0 units (2 mA).
    pub const fn max_power_ma(&self) -> u32 {
        self.max_power as u32 * 2
    }

    /// The raw bytes of the whole configuration descriptor tree.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// Looks up an interface by `bInterfaceNumber`.
    pub fn interface(&self, number: u8) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.number == number)
    }

    /// Iterates over every alternate setting of every interface.
    pub fn all_alt_settings(&self) -> impl Iterator<Item = &InterfaceDescriptor> {
        self.interfaces.iter().flat_map(|i| i.alt_settings.iter())
    }

    /// Iterates over every endpoint of every alternate setting.
    pub fn all_endpoints(&self) -> impl Iterator<Item = &EndpointDescriptor> {
        self.all_alt_settings().flat_map(|a| a.endpoints.iter())
    }

    /// Parses a configuration descriptor tree from its wire encoding.
    ///
    /// The slice must start with the 9-byte configuration header. Data past
    /// `wTotalLength` is ignored; a truncated tree is parsed as far as it goes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 9 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "config descriptor too short"));
        }
        if data[0] < 9 || data[1] != descriptor_type::CONFIG {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a config descriptor"));
        }
        let total_length = u16::from_le_bytes([data[2], data[3]]);
        let end = (total_length as usize).clamp(9, data.len());
        let raw = data[..end].to_vec();
        let mut cfg = ConfigDescriptor {
            total_length,
            num_interfaces: data[4],
            configuration_value: data[5],
            string_index: data[6],
            attributes: data[7],
            max_power: data[8],
            interfaces: Vec::new(),
            extra: Vec::new(),
            raw,
        };

        let mut pos = data[0] as usize;
        let mut cur_iface: Option<InterfaceDescriptor> = None;
        let mut cur_ep: Option<EndpointDescriptor> = None;

        while pos + 2 <= end {
            let len = data[pos] as usize;
            let ty = data[pos + 1];
            if len < 2 || pos + len > end {
                break; // malformed tail: keep what we have
            }
            let d = &data[pos..pos + len];
            match ty {
                descriptor_type::INTERFACE if len >= 9 => {
                    if let Some(ep) = cur_ep.take()
                        && let Some(i) = cur_iface.as_mut()
                    {
                        i.endpoints.push(ep);
                    }
                    if let Some(i) = cur_iface.take() {
                        cfg.push_interface(i);
                    }
                    cur_iface = Some(InterfaceDescriptor {
                        number: d[2],
                        alternate_setting: d[3],
                        num_endpoints: d[4],
                        class: d[5],
                        sub_class: d[6],
                        protocol: d[7],
                        string_index: d[8],
                        endpoints: Vec::new(),
                        extra: Vec::new(),
                    });
                }
                descriptor_type::ENDPOINT if len >= 7 && cur_iface.is_some() => {
                    if let Some(ep) = cur_ep.take()
                        && let Some(i) = cur_iface.as_mut()
                    {
                        i.endpoints.push(ep);
                    }
                    cur_ep = Some(EndpointDescriptor {
                        address: d[2],
                        attributes: d[3],
                        max_packet_size: u16::from_le_bytes([d[4], d[5]]),
                        interval: d[6],
                        refresh: d.get(7).copied().unwrap_or(0),
                        synch_address: d.get(8).copied().unwrap_or(0),
                        ss_companion: None,
                        extra: Vec::new(),
                    });
                }
                descriptor_type::SS_ENDPOINT_COMPANION if len >= 6 && cur_ep.is_some() => {
                    if let Some(ep) = cur_ep.as_mut() {
                        ep.ss_companion = Some(SsEndpointCompanion {
                            max_burst: d[2],
                            attributes: d[3],
                            bytes_per_interval: u16::from_le_bytes([d[4], d[5]]),
                        });
                        ep.extra.extend_from_slice(d);
                    }
                }
                // Anything else is attached as "extra" to the innermost open
                // descriptor, as libusb does.
                _ => {
                    if let Some(ep) = cur_ep.as_mut() {
                        ep.extra.extend_from_slice(d);
                    } else if let Some(i) = cur_iface.as_mut() {
                        i.extra.extend_from_slice(d);
                    } else {
                        cfg.extra.extend_from_slice(d);
                    }
                }
            }
            pos += len;
        }
        if let Some(ep) = cur_ep.take()
            && let Some(i) = cur_iface.as_mut()
        {
            i.endpoints.push(ep);
        }
        if let Some(i) = cur_iface.take() {
            cfg.push_interface(i);
        }
        Ok(cfg)
    }

    fn push_interface(&mut self, alt: InterfaceDescriptor) {
        if let Some(i) = self.interfaces.iter_mut().find(|i| i.number == alt.number) {
            i.alt_settings.push(alt);
        } else {
            self.interfaces.push(Interface {
                number: alt.number,
                alt_settings: vec![alt],
            });
        }
    }

    /// Parses the interface association descriptors found in `extra`.
    pub fn interface_associations(&self) -> Vec<InterfaceAssociation> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 2 <= self.extra.len() {
            let len = self.extra[pos] as usize;
            if len < 2 || pos + len > self.extra.len() {
                break;
            }
            let d = &self.extra[pos..pos + len];
            if d[1] == descriptor_type::INTERFACE_ASSOCIATION && len >= 8 {
                out.push(InterfaceAssociation {
                    first_interface: d[2],
                    interface_count: d[3],
                    function_class: d[4],
                    function_sub_class: d[5],
                    function_protocol: d[6],
                    string_index: d[7],
                });
            }
            pos += len;
        }
        out
    }
}

impl fmt::Debug for ConfigDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigDescriptor")
            .field("total_length", &self.total_length)
            .field("num_interfaces", &self.num_interfaces)
            .field("configuration_value", &self.configuration_value)
            .field("string_index", &self.string_index)
            .field("attributes", &format_args!("{:#04x}", self.attributes))
            .field("max_power", &self.max_power)
            .field("interfaces", &self.interfaces)
            .field("extra", &self.extra)
            .finish()
    }
}

/// An interface association descriptor (USB 2.0 ECN).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InterfaceAssociation {
    /// `bFirstInterface`.
    pub first_interface: u8,
    /// `bInterfaceCount`.
    pub interface_count: u8,
    /// `bFunctionClass`.
    pub function_class: u8,
    /// `bFunctionSubClass`.
    pub function_sub_class: u8,
    /// `bFunctionProtocol`.
    pub function_protocol: u8,
    /// `iFunction`.
    pub string_index: u8,
}

/// One interface number together with all its alternate settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// Alternate settings in the order the device listed them. Index 0 is
    /// almost always `bAlternateSetting == 0`.
    pub alt_settings: Vec<InterfaceDescriptor>,
}

impl Interface {
    /// The alternate setting with the given `bAlternateSetting` value.
    pub fn alt_setting(&self, value: u8) -> Option<&InterfaceDescriptor> {
        self.alt_settings.iter().find(|a| a.alternate_setting == value)
    }

    /// The first listed alternate setting (normally alternate setting 0).
    pub fn first(&self) -> &InterfaceDescriptor {
        &self.alt_settings[0]
    }
}

/// A single interface descriptor (one alternate setting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// `bAlternateSetting`.
    pub alternate_setting: u8,
    /// `bNumEndpoints` as declared (the `endpoints` vector is authoritative).
    pub num_endpoints: u8,
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub sub_class: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
    /// `iInterface`: string descriptor index, 0 if none.
    pub string_index: u8,
    /// Endpoints of this alternate setting.
    pub endpoints: Vec<EndpointDescriptor>,
    /// Class- or vendor-specific descriptors following this interface.
    pub extra: Vec<u8>,
}

impl InterfaceDescriptor {
    /// Finds an endpoint by address (`bEndpointAddress`, direction bit included).
    pub fn endpoint(&self, address: u8) -> Option<&EndpointDescriptor> {
        self.endpoints.iter().find(|e| e.address == address)
    }
}

/// SuperSpeed endpoint companion descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SsEndpointCompanion {
    /// `bMaxBurst`: maximum packets per burst minus one.
    pub max_burst: u8,
    /// `bmAttributes`: max streams (bulk) or mult (isochronous).
    pub attributes: u8,
    /// `wBytesPerInterval`.
    pub bytes_per_interval: u16,
}

/// An endpoint descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// `bEndpointAddress`: number in bits 0-3, direction in bit 7.
    pub address: u8,
    /// `bmAttributes`.
    pub attributes: u8,
    /// `wMaxPacketSize`, raw. Bits 11-12 encode extra transactions per
    /// microframe for high-speed periodic endpoints; see
    /// [`max_packet_size`](Self::max_packet_size).
    pub max_packet_size: u16,
    /// `bInterval`.
    pub interval: u8,
    /// `bRefresh` (audio endpoints only).
    pub refresh: u8,
    /// `bSynchAddress` (audio endpoints only).
    pub synch_address: u8,
    /// SuperSpeed companion, when one followed this endpoint.
    pub ss_companion: Option<SsEndpointCompanion>,
    /// Class- or vendor-specific descriptors following this endpoint
    /// (a SuperSpeed companion is included here as well as parsed).
    pub extra: Vec<u8>,
}

impl EndpointDescriptor {
    /// Endpoint number (0-15), without the direction bit.
    pub const fn number(&self) -> u8 {
        self.address & 0x0f
    }

    /// Direction of the endpoint.
    pub const fn direction(&self) -> Direction {
        Direction::from_address(self.address)
    }

    /// Transfer type of the endpoint.
    pub const fn transfer_type(&self) -> TransferType {
        TransferType::from_attributes(self.attributes)
    }

    /// Synchronisation type (meaningful for isochronous endpoints).
    pub const fn sync_type(&self) -> SyncType {
        match (self.attributes >> 2) & 0x03 {
            0 => SyncType::None,
            1 => SyncType::Asynchronous,
            2 => SyncType::Adaptive,
            _ => SyncType::Synchronous,
        }
    }

    /// Usage type (meaningful for isochronous and interrupt endpoints).
    pub const fn usage_type(&self) -> UsageType {
        match (self.attributes >> 4) & 0x03 {
            0 => UsageType::Data,
            1 => UsageType::Feedback,
            2 => UsageType::Implicit,
            _ => UsageType::Reserved,
        }
    }

    /// Maximum packet size in bytes, i.e. bits 0-10 of `wMaxPacketSize`.
    pub const fn packet_size(&self) -> u16 {
        self.max_packet_size & 0x07ff
    }

    /// Maximum bytes per (micro)frame: packet size times the number of
    /// transactions encoded in bits 11-12 (high-speed periodic endpoints).
    pub const fn max_packet_size(&self) -> u32 {
        let mult = ((self.max_packet_size >> 11) & 0x03) as u32 + 1;
        self.packet_size() as u32 * mult
    }
}

/// Decodes a string descriptor (type 3) payload into a Rust string.
///
/// Malformed UTF-16 code units are replaced with U+FFFD.
pub fn decode_string_descriptor(data: &[u8]) -> Result<String> {
    if data.len() < 2 || data[1] != descriptor_type::STRING {
        return Err(Error::with_message(ErrorKind::Io, "not a string descriptor"));
    }
    let len = (data[0] as usize).min(data.len());
    let units: Vec<u16> = data[2..len].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Decodes the language ID table of string descriptor 0.
pub fn decode_language_ids(data: &[u8]) -> Result<Vec<u16>> {
    if data.len() < 2 || data[1] != descriptor_type::STRING {
        return Err(Error::with_message(ErrorKind::Io, "not a string descriptor"));
    }
    let len = (data[0] as usize).min(data.len());
    Ok(data[2..len].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A CDC-ACM style device: config with IAD, two interfaces, three endpoints,
    // one class-specific descriptor on the first interface.
    fn sample_config() -> Vec<u8> {
        let mut v = vec![
            9, 2, 0, 0, 2, 1, 0, 0xe0, 50, // config (total length patched below)
            8, 0x0b, 0, 2, 2, 2, 1, 0, // IAD
            9, 4, 0, 0, 1, 2, 2, 1, 0, // interface 0 alt 0
            5, 0x24, 0x00, 0x10, 0x01, // CDC header (class specific)
            7, 5, 0x81, 3, 8, 0, 10, // EP 0x81 interrupt
            9, 4, 1, 0, 2, 10, 0, 0, 0, // interface 1 alt 0
            7, 5, 0x02, 2, 0, 2, 0, // EP 0x02 bulk
            7, 5, 0x82, 2, 0, 2, 0, // EP 0x82 bulk
            6, 0x30, 0, 0, 0, 0, // SS companion on EP 0x82
            9, 4, 1, 1, 0, 10, 0, 0, 0, // interface 1 alt 1 (no endpoints)
        ];
        let total = v.len() as u16;
        v[2..4].copy_from_slice(&total.to_le_bytes());
        v
    }

    #[test]
    fn parse_config_tree() {
        let raw = sample_config();
        let cfg = ConfigDescriptor::from_bytes(&raw).unwrap();
        assert_eq!(cfg.configuration_value, 1);
        assert_eq!(cfg.num_interfaces, 2);
        assert!(cfg.self_powered());
        assert!(cfg.remote_wakeup());
        assert_eq!(cfg.max_power_ma(), 100);
        assert_eq!(cfg.raw(), &raw[..]);
        assert_eq!(cfg.interfaces.len(), 2);

        let iad = cfg.interface_associations();
        assert_eq!(iad.len(), 1);
        assert_eq!(iad[0].first_interface, 0);
        assert_eq!(iad[0].interface_count, 2);

        let i0 = cfg.interface(0).unwrap();
        assert_eq!(i0.alt_settings.len(), 1);
        assert_eq!(i0.first().class, 2);
        assert_eq!(i0.first().extra, vec![5, 0x24, 0x00, 0x10, 0x01]);
        assert_eq!(i0.first().endpoints.len(), 1);
        let ep = &i0.first().endpoints[0];
        assert_eq!(ep.address, 0x81);
        assert_eq!(ep.number(), 1);
        assert_eq!(ep.direction(), Direction::In);
        assert_eq!(ep.transfer_type(), TransferType::Interrupt);
        assert_eq!(ep.max_packet_size(), 8);
        assert_eq!(ep.interval, 10);

        let i1 = cfg.interface(1).unwrap();
        assert_eq!(i1.alt_settings.len(), 2);
        assert_eq!(i1.alt_setting(0).unwrap().endpoints.len(), 2);
        assert_eq!(i1.alt_setting(1).unwrap().endpoints.len(), 0);
        let ep82 = i1.alt_setting(0).unwrap().endpoint(0x82).unwrap();
        assert_eq!(ep82.transfer_type(), TransferType::Bulk);
        assert_eq!(ep82.max_packet_size(), 512);
        assert!(ep82.ss_companion.is_some());
        assert_eq!(cfg.all_endpoints().count(), 3);
    }

    #[test]
    fn truncated_config_is_tolerated() {
        let raw = sample_config();
        let cfg = ConfigDescriptor::from_bytes(&raw[..30]).unwrap();
        assert_eq!(cfg.interfaces.len(), 1);
        assert_eq!(cfg.interfaces[0].first().endpoints.len(), 0);
        assert!(ConfigDescriptor::from_bytes(&raw[..5]).is_err());
        assert!(ConfigDescriptor::from_bytes(&[9, 4, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn high_speed_packet_multiplier() {
        let ep = EndpointDescriptor {
            address: 0x83,
            attributes: 0x05,
            max_packet_size: 0x1400, // 2 extra transactions, 1024 bytes
            interval: 1,
            refresh: 0,
            synch_address: 0,
            ss_companion: None,
            extra: Vec::new(),
        };
        assert_eq!(ep.packet_size(), 1024);
        assert_eq!(ep.max_packet_size(), 3072);
        assert_eq!(ep.transfer_type(), TransferType::Isochronous);
        assert_eq!(ep.sync_type(), SyncType::Asynchronous);
        assert_eq!(ep.usage_type(), UsageType::Data);
    }

    #[test]
    fn device_descriptor_roundtrip() {
        let raw = [18, 1, 0x10, 0x02, 0, 0, 0, 64, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01, 1, 2, 3, 1];
        let d = DeviceDescriptor::from_bytes(&raw).unwrap();
        assert_eq!(d.usb_version, Version(0x0210));
        assert_eq!(d.vendor_id, 0x1234);
        assert_eq!(d.product_id, 0x5678);
        assert_eq!(d.max_packet_size_0, 64);
        assert_eq!(d.num_configurations, 1);
        assert_eq!(d.to_bytes(), raw);
        assert!(DeviceDescriptor::from_bytes(&raw[..17]).is_err());
    }

    #[test]
    fn string_descriptors() {
        let s = [8, 3, b'H', 0, b'i', 0, 0x3d, 0xd8]; // trailing lone surrogate
        assert_eq!(decode_string_descriptor(&s).unwrap(), "Hi\u{fffd}");
        let langs = [6, 3, 0x09, 0x04, 0x11, 0x04];
        assert_eq!(decode_language_ids(&langs).unwrap(), vec![0x0409, 0x0411]);
        assert!(decode_string_descriptor(&[2, 2]).is_err());
    }
}
