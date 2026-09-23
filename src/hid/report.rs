//! HID report descriptor parsing (HID 1.11 §6.2.2).

use super::ReportType;
use crate::{Error, ErrorKind, Result};

/// The three kinds of items in a report descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemType {
    /// Main items (Input, Output, Feature, Collection, End Collection).
    Main,
    /// Global items, which persist until changed.
    Global,
    /// Local items, which apply to the next main item only.
    Local,
    /// Reserved item type 3, or a long item.
    Reserved,
}

/// One short (or long) item of a report descriptor, undecoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item<'a> {
    /// The item type.
    pub kind: ItemType,
    /// The item tag (the high nibble of the prefix byte; the long-item tag for
    /// long items).
    pub tag: u8,
    /// The item's data bytes, little-endian.
    pub data: &'a [u8],
}

impl Item<'_> {
    /// The data zero-extended to 32 bits.
    pub fn unsigned(&self) -> u32 {
        let mut b = [0u8; 4];
        let n = self.data.len().min(4);
        b[..n].copy_from_slice(&self.data[..n]);
        u32::from_le_bytes(b)
    }

    /// The data sign-extended to 32 bits.
    pub fn signed(&self) -> i32 {
        match self.data.len() {
            0 => 0,
            1 => self.data[0] as i8 as i32,
            2 => i16::from_le_bytes([self.data[0], self.data[1]]) as i32,
            _ => self.unsigned() as i32,
        }
    }
}

/// Iterates over the items of a report descriptor. Stops (without error) at a
/// truncated trailing item.
pub fn items(descriptor: &[u8]) -> impl Iterator<Item = Item<'_>> {
    let mut rest = descriptor;
    std::iter::from_fn(move || {
        let (&prefix, tail) = rest.split_first()?;
        if prefix == 0xfe {
            // Long item: bDataSize, bLongItemTag, data.
            let (&size, tail) = tail.split_first()?;
            let (&tag, tail) = tail.split_first()?;
            let data = tail.get(..size as usize)?;
            rest = &tail[size as usize..];
            return Some(Item {
                kind: ItemType::Reserved,
                tag,
                data,
            });
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        let data = tail.get(..size)?;
        rest = &tail[size..];
        let kind = match (prefix >> 2) & 0x03 {
            0 => ItemType::Main,
            1 => ItemType::Global,
            2 => ItemType::Local,
            _ => ItemType::Reserved,
        };
        Some(Item {
            kind,
            tag: prefix >> 4,
            data,
        })
    })
}

/// A 32-bit usage: usage page in the high half, usage ID in the low half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Usage(pub u32);

impl Usage {
    /// Builds a usage from its page and ID.
    pub const fn new(page: u16, id: u16) -> Usage {
        Usage((page as u32) << 16 | id as u32)
    }

    /// The usage page (for example 0x01, Generic Desktop).
    pub const fn page(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// The usage ID within the page (for example 0x06, Keyboard).
    pub const fn id(self) -> u16 {
        self.0 as u16
    }
}

/// Well-known usages, for matching [`ReportDescriptor::application_usages`].
pub mod usage {
    use super::Usage;

    /// Generic Desktop / Pointer.
    pub const POINTER: Usage = Usage::new(0x01, 0x01);
    /// Generic Desktop / Mouse.
    pub const MOUSE: Usage = Usage::new(0x01, 0x02);
    /// Generic Desktop / Joystick.
    pub const JOYSTICK: Usage = Usage::new(0x01, 0x04);
    /// Generic Desktop / Gamepad.
    pub const GAMEPAD: Usage = Usage::new(0x01, 0x05);
    /// Generic Desktop / Keyboard.
    pub const KEYBOARD: Usage = Usage::new(0x01, 0x06);
    /// Generic Desktop / Keypad.
    pub const KEYPAD: Usage = Usage::new(0x01, 0x07);
    /// Generic Desktop / System Control.
    pub const SYSTEM_CONTROL: Usage = Usage::new(0x01, 0x80);
    /// Consumer / Consumer Control.
    pub const CONSUMER_CONTROL: Usage = Usage::new(0x0c, 0x01);
    /// Digitizer / Touch Screen.
    pub const TOUCH_SCREEN: Usage = Usage::new(0x0d, 0x04);
    /// FIDO Alliance / U2F Authenticator Device.
    pub const FIDO_AUTHENTICATOR: Usage = Usage::new(0xf1d0, 0x01);
}

/// Which usages a field reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Usages {
    /// An explicit list. For a variable field, element `i` has usage `i`
    /// (the last one repeats); for an array field, these are the selectable
    /// usages, indexed from the field's logical minimum.
    List(Vec<Usage>),
    /// A contiguous range, from Usage Minimum to Usage Maximum.
    Range(Usage, Usage),
}

impl Usages {
    /// The usage of element `index` of a variable field.
    pub fn get(&self, index: usize) -> Option<Usage> {
        match self {
            Usages::List(v) => v.get(index).or(v.last()).copied(),
            Usages::Range(min, max) => {
                let u = min.0.checked_add(index as u32)?;
                (u <= max.0).then_some(Usage(u))
            }
        }
    }
}

/// One Input, Output or Feature main item: `count` elements of `bit_size`
/// bits each, starting at `bit_offset` within its report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Which report kind the field belongs to.
    pub report_type: ReportType,
    /// The report ID, or 0 when the device does not use report IDs.
    pub report_id: u8,
    /// Bit offset of the first element within the report, *not* counting the
    /// report ID byte.
    pub bit_offset: u32,
    /// Size of each element in bits (Report Size).
    pub bit_size: u32,
    /// Number of elements (Report Count).
    pub count: u32,
    /// The main item's data bits (bit 0 constant, bit 1 variable, bit 2
    /// relative, ...).
    pub flags: u32,
    /// The usages the elements report.
    pub usages: Usages,
    /// Logical Minimum.
    pub logical_min: i64,
    /// Logical Maximum.
    pub logical_max: i64,
    /// Physical Minimum.
    pub physical_min: i64,
    /// Physical Maximum.
    pub physical_max: i64,
    /// Unit Exponent.
    pub unit_exponent: i32,
    /// Unit.
    pub unit: u32,
}

impl Field {
    /// `true` for padding and other constant fields.
    pub const fn is_constant(&self) -> bool {
        self.flags & 0x01 != 0
    }

    /// `true` for a variable field (one value per usage); `false` for an
    /// array field (a list of selected usage indices, like keyboard keys).
    pub const fn is_variable(&self) -> bool {
        self.flags & 0x02 != 0
    }

    /// `true` if values are relative (mouse motion) rather than absolute.
    pub const fn is_relative(&self) -> bool {
        self.flags & 0x04 != 0
    }

    /// Extracts element `index` from a report. `report` must *not* include the
    /// report ID byte (see [`ReportDescriptor::strip_id`]). The value is
    /// sign-extended when the logical minimum is negative. `None` if the
    /// element lies outside `report`.
    pub fn value(&self, report: &[u8], index: u32) -> Option<i64> {
        if index >= self.count || self.bit_size == 0 || self.bit_size > 32 {
            return None;
        }
        let start = self.bit_offset as usize + (index * self.bit_size) as usize;
        let end = start + self.bit_size as usize;
        if end > report.len() * 8 {
            return None;
        }
        let mut v: u64 = 0;
        for (i, bit) in (start..end).enumerate() {
            if report[bit / 8] >> (bit % 8) & 1 != 0 {
                v |= 1 << i;
            }
        }
        if self.logical_min < 0 && self.bit_size < 64 && v >> (self.bit_size - 1) & 1 != 0 {
            Some(v as i64 - (1i64 << self.bit_size))
        } else {
            Some(v as i64)
        }
    }

    /// For an array field, maps a value read with [`value`](Self::value) to
    /// the usage it selects. `None` for "no selection" and out-of-range
    /// values.
    pub fn array_usage(&self, value: i64) -> Option<Usage> {
        if value < self.logical_min || value > self.logical_max {
            return None;
        }
        let index = usize::try_from(value - self.logical_min).ok()?;
        match &self.usages {
            Usages::List(v) => v.get(index).copied(),
            r @ Usages::Range(..) => r.get(index),
        }
    }
}

/// A parsed report descriptor: its fields and report layouts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReportDescriptor {
    /// Every Input, Output and Feature field, in descriptor order.
    pub fields: Vec<Field>,
    /// The usages of the top-level application collections, which say what
    /// the device is (keyboard, mouse, FIDO token, ...).
    pub application_usages: Vec<Usage>,
    reports: Vec<(ReportType, u8, u32)>,
    uses_ids: bool,
}

#[derive(Clone, Default)]
struct Globals {
    usage_page: u16,
    logical_min: i64,
    logical_max: i64,
    physical_min: i64,
    physical_max: i64,
    unit_exponent: i32,
    unit: u32,
    report_size: u32,
    report_id: u8,
    report_count: u32,
    /// Size of the Logical Maximum item, to reinterpret it as unsigned.
    logical_max_size: usize,
}

#[derive(Default)]
struct Locals {
    usages: Vec<Usage>,
    usage_min: Option<Usage>,
    usage_max: Option<Usage>,
}

impl ReportDescriptor {
    /// Parses a report descriptor.
    pub fn parse(descriptor: &[u8]) -> Result<ReportDescriptor> {
        let bad = |m: &'static str| Error::with_message(ErrorKind::InvalidParam, m);
        let mut out = ReportDescriptor::default();
        let mut g = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut l = Locals::default();
        let mut depth = 0usize;
        // Usages are resolved at the main item: a 1- or 2-byte usage takes
        // the Usage Page current *then*, not the one current when the usage
        // item was read (HID 1.11 §6.2.2.8). A 4-byte usage carries its page.
        let mut raw_usages: Vec<(u32, bool)> = Vec::new();
        let mut raw_min: Option<(u32, bool)> = None;
        let mut raw_max: Option<(u32, bool)> = None;

        for item in items(descriptor) {
            match item.kind {
                ItemType::Global => match item.tag {
                    0x0 => g.usage_page = item.unsigned() as u16,
                    0x1 => g.logical_min = item.signed() as i64,
                    0x2 => {
                        g.logical_max = item.signed() as i64;
                        g.logical_max_size = item.data.len();
                    }
                    0x3 => g.physical_min = item.signed() as i64,
                    0x4 => g.physical_max = item.signed() as i64,
                    0x5 => g.unit_exponent = item.signed(),
                    0x6 => g.unit = item.unsigned(),
                    0x7 => g.report_size = item.unsigned(),
                    0x8 => {
                        let id = item.unsigned();
                        if id == 0 || id > 255 {
                            return Err(bad("report ID out of range"));
                        }
                        g.report_id = id as u8;
                        out.uses_ids = true;
                    }
                    0x9 => g.report_count = item.unsigned(),
                    0xa => stack.push(g.clone()),
                    0xb => g = stack.pop().ok_or_else(|| bad("Pop without Push"))?,
                    _ => {}
                },
                ItemType::Local => match item.tag {
                    0x0 => raw_usages.push((item.unsigned(), item.data.len() == 4)),
                    0x1 => raw_min = Some((item.unsigned(), item.data.len() == 4)),
                    0x2 => raw_max = Some((item.unsigned(), item.data.len() == 4)),
                    _ => {}
                },
                ItemType::Main => {
                    let page = g.usage_page;
                    let resolve = |(v, ext): (u32, bool)| {
                        if ext { Usage(v) } else { Usage::new(page, v as u16) }
                    };
                    l.usages = raw_usages.drain(..).map(resolve).collect();
                    l.usage_min = raw_min.take().map(resolve);
                    l.usage_max = raw_max.take().map(resolve);
                    match item.tag {
                        0x8 | 0x9 | 0xb => {
                            let report_type = match item.tag {
                                0x8 => ReportType::Input,
                                0x9 => ReportType::Output,
                                _ => ReportType::Feature,
                            };
                            let bits = g.report_size.checked_mul(g.report_count).ok_or_else(|| bad("report too large"))?;
                            let offset = out.report_bits_mut(report_type, g.report_id);
                            let bit_offset = *offset;
                            *offset = offset.checked_add(bits).ok_or_else(|| bad("report too large"))?;
                            let usages = match (l.usage_min, l.usage_max) {
                                (Some(min), Some(max)) if l.usages.is_empty() => Usages::Range(min, max),
                                _ => Usages::List(std::mem::take(&mut l.usages)),
                            };
                            // Logical Maximum is unsigned when Logical Minimum
                            // is not negative (HID 1.11 §5.8, and what every
                            // host stack does with 0..255 in one byte).
                            let logical_max = if g.logical_min >= 0 && g.logical_max < 0 {
                                match g.logical_max_size {
                                    1 => g.logical_max as u8 as i64,
                                    2 => g.logical_max as u16 as i64,
                                    _ => g.logical_max as u32 as i64,
                                }
                            } else {
                                g.logical_max
                            };
                            out.fields.push(Field {
                                report_type,
                                report_id: g.report_id,
                                bit_offset,
                                bit_size: g.report_size,
                                count: g.report_count,
                                flags: item.unsigned(),
                                usages,
                                logical_min: g.logical_min,
                                logical_max,
                                physical_min: g.physical_min,
                                physical_max: g.physical_max,
                                unit_exponent: g.unit_exponent,
                                unit: g.unit,
                            });
                        }
                        0xa => {
                            if depth == 0 && item.unsigned() == 1 {
                                if let Some(&u) = l.usages.first() {
                                    out.application_usages.push(u);
                                } else if let Some(u) = l.usage_min {
                                    out.application_usages.push(u);
                                }
                            }
                            depth += 1;
                        }
                        0xc => depth = depth.checked_sub(1).ok_or_else(|| bad("End Collection without Collection"))?,
                        _ => {}
                    }
                    l = Locals::default();
                }
                ItemType::Reserved => {}
            }
        }
        Ok(out)
    }

    fn report_bits_mut(&mut self, kind: ReportType, id: u8) -> &mut u32 {
        let pos = match self.reports.iter().position(|&(k, i, _)| k == kind && i == id) {
            Some(p) => p,
            None => {
                self.reports.push((kind, id, 0));
                self.reports.len() - 1
            }
        };
        &mut self.reports[pos].2
    }

    /// `true` if the descriptor declares report IDs, in which case every
    /// report on the wire starts with its ID byte.
    pub fn uses_report_ids(&self) -> bool {
        self.uses_ids
    }

    /// The report IDs declared for a report type (`[0]` when the device does
    /// not use IDs and has reports of that type).
    pub fn report_ids(&self, kind: ReportType) -> Vec<u8> {
        self.reports.iter().filter(|r| r.0 == kind).map(|r| r.1).collect()
    }

    /// Size in bytes of a report's payload, *excluding* the report ID byte.
    pub fn report_len(&self, kind: ReportType, id: u8) -> Option<usize> {
        self.reports
            .iter()
            .find(|r| r.0 == kind && r.1 == id)
            .map(|r| r.2.div_ceil(8) as usize)
    }

    /// The fields of one report.
    pub fn report_fields(&self, kind: ReportType, id: u8) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(move |f| f.report_type == kind && f.report_id == id)
    }

    /// Splits a report as read from the device into its ID and payload. When
    /// the device does not use report IDs the ID is 0 and the payload is the
    /// whole report.
    pub fn strip_id<'a>(&self, report: &'a [u8]) -> (u8, &'a [u8]) {
        match (self.uses_ids, report.split_first()) {
            (true, Some((&id, rest))) => (id, rest),
            _ => (0, report),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot keyboard report descriptor from HID 1.11 Appendix E.6.
    const KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03,
        0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
    ];

    #[test]
    fn boot_keyboard() {
        let d = ReportDescriptor::parse(KEYBOARD).unwrap();
        assert!(!d.uses_report_ids());
        assert_eq!(d.application_usages, vec![usage::KEYBOARD]);
        assert_eq!(d.report_len(ReportType::Input, 0), Some(8));
        assert_eq!(d.report_len(ReportType::Output, 0), Some(1));
        assert_eq!(d.report_len(ReportType::Feature, 0), None);

        let fields: Vec<_> = d.report_fields(ReportType::Input, 0).collect();
        assert_eq!(fields.len(), 3);
        let mods = fields[0];
        assert!(mods.is_variable());
        assert_eq!(mods.usages.get(1), Some(Usage::new(0x07, 0xe1)));
        let keys = fields[2];
        assert!(!keys.is_variable());
        assert_eq!(keys.bit_offset, 16);

        // Left shift + 'a' (usage 0x04).
        let report = [0x02, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(mods.value(&report, 1), Some(1));
        assert_eq!(mods.value(&report, 0), Some(0));
        assert_eq!(keys.array_usage(keys.value(&report, 0).unwrap()), Some(Usage::new(0x07, 0x04)));
        assert_eq!(keys.value(&report, 6), None);

        let leds: Vec<_> = d.report_fields(ReportType::Output, 0).collect();
        assert_eq!(leds[0].usages, Usages::Range(Usage::new(0x08, 1), Usage::new(0x08, 5)));
    }

    #[test]
    fn report_ids_signed_values_and_push_pop() {
        let desc = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, // Mouse application
            0x85, 0x01, // Report ID 1
            0x09, 0x30, 0x09, 0x31, // X, Y
            0x15, 0x81, 0x25, 0x7f, // -127..127
            0x75, 0x08, 0x95, 0x02, 0x81, 0x06, // 2 x 8 bits, relative
            0xa4, // Push
            0x85, 0x02, 0x06, 0x00, 0xff, 0x09, 0x01, // Report ID 2, vendor page
            0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x03, 0xb1, 0x02, // 3-byte feature
            0xb4, // Pop: back to report ID 1, signed range
            0x09, 0x38, 0x95, 0x01, 0x81, 0x06, // wheel
            0xc0,
        ];
        let d = ReportDescriptor::parse(&desc).unwrap();
        assert!(d.uses_report_ids());
        assert_eq!(d.application_usages, vec![usage::MOUSE]);
        assert_eq!(d.report_len(ReportType::Input, 1), Some(3));
        assert_eq!(d.report_len(ReportType::Feature, 2), Some(3));
        assert_eq!(d.report_ids(ReportType::Input), vec![1]);

        let feature = d.report_fields(ReportType::Feature, 2).next().unwrap();
        assert_eq!(feature.logical_max, 255);
        assert_eq!(feature.usages.get(0), Some(Usage::new(0xff00, 0x01)));

        let (id, payload) = d.strip_id(&[1, 0xff, 5, 0x80]);
        assert_eq!(id, 1);
        let xy = d.report_fields(ReportType::Input, 1).next().unwrap();
        assert_eq!(xy.value(payload, 0), Some(-1));
        assert_eq!(xy.value(payload, 1), Some(5));
        let wheel = d.report_fields(ReportType::Input, 1).nth(1).unwrap();
        assert_eq!(wheel.bit_offset, 16);
        assert_eq!(wheel.value(payload, 0), Some(-128));
        assert!(wheel.is_relative());
    }

    #[test]
    fn malformed() {
        assert!(ReportDescriptor::parse(&[0xc0]).is_err());
        assert!(ReportDescriptor::parse(&[0xb4]).is_err());
        assert!(ReportDescriptor::parse(&[0x85, 0x00]).is_err());
        // Truncated trailing item: ignored.
        let d = ReportDescriptor::parse(&[0x05, 0x01, 0x26, 0xff]).unwrap();
        assert!(d.fields.is_empty());
        // Long items are skipped.
        assert_eq!(items(&[0xfe, 1, 0x42, 0xaa, 0x05, 0x01]).count(), 2);
    }
}
