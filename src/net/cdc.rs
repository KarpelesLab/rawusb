//! CDC networking: ECM and NCM functional descriptors, notifications, and the
//! NCM transfer block (NTB16) format (USB CDC 1.2 ECM 1.2, NCM 1.0).

use crate::class::{self, le16};
use crate::descriptors::{ConfigDescriptor, InterfaceDescriptor};
use crate::{Error, ErrorKind, Result};

/// Class requests.
pub(crate) mod req {
    pub(crate) const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
    pub(crate) const GET_NTB_PARAMETERS: u8 = 0x80;
    pub(crate) const SET_NTB_INPUT_SIZE: u8 = 0x86;
}

/// SET_ETHERNET_PACKET_FILTER bits.
pub(crate) mod filter {
    pub(crate) const PROMISCUOUS: u16 = 0x01;
    pub(crate) const ALL_MULTICAST: u16 = 0x02;
    pub(crate) const DIRECTED: u16 = 0x04;
    pub(crate) const BROADCAST: u16 = 0x08;
}

const CS_INTERFACE: u8 = 0x24;
const SUBTYPE_UNION: u8 = 0x06;
const SUBTYPE_ETHERNET: u8 = 0x0f;
const SUBTYPE_NCM: u8 = 0x1a;

/// Subclass of a communications interface.
pub(crate) const SUBCLASS_ECM: u8 = 0x06;
pub(crate) const SUBCLASS_NCM: u8 = 0x0d;

/// What the functional descriptors of an ECM or NCM control interface say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Functional {
    pub(crate) data_interface: Option<u8>,
    pub(crate) mac_string: u8,
    pub(crate) max_segment_size: u16,
    /// NCM `bmNetworkCapabilities`; bit 0 means SET_ETHERNET_PACKET_FILTER is
    /// supported.
    pub(crate) ncm_capabilities: Option<u8>,
}

impl Functional {
    pub(crate) fn parse(comm: &InterfaceDescriptor) -> Functional {
        let mut f = Functional::default();
        for (ty, d) in class::descriptors(&comm.extra) {
            if ty != CS_INTERFACE || d.len() < 3 {
                continue;
            }
            match d[2] {
                SUBTYPE_UNION if d.len() >= 5 => f.data_interface = Some(d[4]),
                SUBTYPE_ETHERNET if d.len() >= 13 => {
                    f.mac_string = d[3];
                    f.max_segment_size = le16(d, 8);
                }
                SUBTYPE_NCM if d.len() >= 6 => f.ncm_capabilities = Some(d[5]),
                _ => {}
            }
        }
        f
    }

    /// The data interface: from the union descriptor, else the next
    /// interface if it is a CDC data interface.
    pub(crate) fn data_interface(&self, cfg: &ConfigDescriptor, comm: u8) -> Option<u8> {
        let is_data = |n: u8| {
            cfg.interface(n)
                .is_some_and(|i| i.alt_settings.iter().any(|a| a.class == crate::types::class::DATA))
        };
        self.data_interface
            .filter(|&n| n != comm && cfg.interface(n).is_some())
            .or_else(|| comm.checked_add(1).filter(|&n| is_data(n)))
    }
}

/// Parses the MAC address string descriptor: twelve hex digits.
pub(crate) fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let s = s.trim();
    if s.len() != 12 || !s.is_ascii() {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(mac)
}

/// A decoded ECM/NCM notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Notification {
    Connection(bool),
    Speed { down: u32, up: u32 },
}

pub(crate) fn decode_notification(n: &[u8]) -> Option<Notification> {
    if n.len() < 8 || n[0] != 0xa1 {
        return None;
    }
    match n[1] {
        0x00 => Some(Notification::Connection(le16(n, 2) != 0)),
        0x2a if n.len() >= 16 => Some(Notification::Speed {
            down: le32(n, 8),
            up: le32(n, 12),
        }),
        _ => None,
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    le16(b, at) as u32 | (le16(b, at + 2) as u32) << 16
}

/// GET_NTB_PARAMETERS response: the NTB sizes and datagram alignment rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NtbParameters {
    pub(crate) in_max_size: u32,
    pub(crate) out_max_size: u32,
    pub(crate) out_divisor: u16,
    pub(crate) out_remainder: u16,
    pub(crate) out_alignment: u16,
}

impl NtbParameters {
    pub(crate) fn parse(b: &[u8]) -> Option<NtbParameters> {
        if b.len() < 28 || le16(b, 2) & 0x01 == 0 {
            return None; // no NTB16 support
        }
        Some(NtbParameters {
            in_max_size: le32(b, 4),
            out_max_size: le32(b, 16),
            out_divisor: le16(b, 20),
            out_remainder: le16(b, 22),
            out_alignment: le16(b, 24),
        })
    }
}

const NTH16_SIGNATURE: &[u8; 4] = b"NCMH";
const NDP16_NO_CRC: &[u8; 4] = b"NCM0";
const NDP16_CRC: &[u8; 4] = b"NCM1";
const NTH16_LEN: usize = 12;
/// An NDP16 carrying one datagram: header, one entry, and the terminator.
const NDP16_ONE_LEN: usize = 16;

fn align_up(v: usize, to: usize) -> usize {
    if to <= 1 { v } else { v.div_ceil(to) * to }
}

/// Wraps one Ethernet frame into an NTB16, following the device's alignment
/// rules. `pad_multiple` is the bulk OUT packet size: a block that would be
/// an exact multiple of it gets one more byte, so it ends with a short packet
/// on every platform (the spec allows padding past the datagrams).
pub(crate) fn build_ntb16(frame: &[u8], sequence: u16, p: &NtbParameters, pad_multiple: usize) -> Result<Vec<u8>> {
    let ndp = align_up(NTH16_LEN, (p.out_alignment as usize).max(4));
    let divisor = (p.out_divisor as usize).max(1);
    let remainder = p.out_remainder as usize % divisor;
    // The datagram starts where offset % divisor == remainder.
    let data = align_up(ndp + NDP16_ONE_LEN, divisor) + remainder;
    let mut total = data + frame.len();
    if pad_multiple > 0 && total.is_multiple_of(pad_multiple) {
        total += 1;
    }
    if total > p.out_max_size as usize || total > u16::MAX as usize {
        return Err(Error::with_message(
            ErrorKind::InvalidParam,
            "frame too large for the device's NTB size",
        ));
    }
    let mut b = vec![0u8; total];
    b[0..4].copy_from_slice(NTH16_SIGNATURE);
    b[4..6].copy_from_slice(&(NTH16_LEN as u16).to_le_bytes());
    b[6..8].copy_from_slice(&sequence.to_le_bytes());
    b[8..10].copy_from_slice(&(total as u16).to_le_bytes());
    b[10..12].copy_from_slice(&(ndp as u16).to_le_bytes());
    b[ndp..ndp + 4].copy_from_slice(NDP16_NO_CRC);
    b[ndp + 4..ndp + 6].copy_from_slice(&(NDP16_ONE_LEN as u16).to_le_bytes());
    // wNextNdpIndex stays 0; the terminating (0, 0) entry is already zero.
    b[ndp + 8..ndp + 10].copy_from_slice(&(data as u16).to_le_bytes());
    b[ndp + 10..ndp + 12].copy_from_slice(&(frame.len() as u16).to_le_bytes());
    b[data..data + frame.len()].copy_from_slice(frame);
    Ok(b)
}

/// Splits an NTB16 into its datagrams, calling `emit` for each. Returns
/// `false` if the block is malformed (datagrams found before the problem are
/// still emitted).
pub(crate) fn parse_ntb16(b: &[u8], mut emit: impl FnMut(&[u8])) -> bool {
    if b.len() < NTH16_LEN || &b[0..4] != NTH16_SIGNATURE || le16(b, 4) as usize != NTH16_LEN {
        return false;
    }
    let block = (le16(b, 8) as usize).min(b.len());
    let mut ndp = le16(b, 10) as usize;
    // Each NDP must lie after the header; a bounded walk defeats loops.
    for _ in 0..16 {
        if ndp == 0 {
            return true;
        }
        if ndp < NTH16_LEN || !ndp.is_multiple_of(4) || ndp + 8 > block {
            return false;
        }
        let sig = &b[ndp..ndp + 4];
        let crc = sig == NDP16_CRC;
        if sig != NDP16_NO_CRC && !crc {
            return false;
        }
        let len = le16(b, ndp + 4) as usize;
        if len < 16 || ndp + len > block {
            return false;
        }
        let mut entry = ndp + 8;
        while entry + 4 <= ndp + len {
            let (index, length) = (le16(b, entry) as usize, le16(b, entry + 2) as usize);
            if index == 0 || length == 0 {
                break;
            }
            // With CRC mode the datagram carries a trailing 4-byte CRC.
            let length = if crc { length.saturating_sub(4) } else { length };
            if index < NTH16_LEN || index + length > block || length < 14 {
                return false;
            }
            emit(&b[index..index + length]);
            entry += 4;
        }
        ndp = le16(b, ndp + 6) as usize;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> NtbParameters {
        NtbParameters {
            in_max_size: 16384,
            out_max_size: 16384,
            out_divisor: 4,
            out_remainder: 2,
            out_alignment: 4,
        }
    }

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn ntb_roundtrip_honours_alignment() {
        let f = frame(60);
        let b = build_ntb16(&f, 7, &params(), 512).unwrap();
        assert_eq!(&b[0..4], b"NCMH");
        assert_eq!(le16(&b, 6), 7);
        assert_eq!(le16(&b, 8) as usize, b.len());
        let ndp = le16(&b, 10) as usize;
        assert_eq!(ndp, 12);
        let data = le16(&b, ndp + 8) as usize;
        assert_eq!(data % 4, 2, "datagram offset honours divisor/remainder");
        assert!(data >= ndp + 16);
        let mut got = Vec::new();
        assert!(parse_ntb16(&b, |d| got.push(d.to_vec())));
        assert_eq!(got, vec![f]);
    }

    #[test]
    fn ntb_pads_exact_packet_multiples() {
        let p = NtbParameters {
            out_remainder: 0,
            ..params()
        };
        // Header 12 + NDP 16 = 28; 28 + 484 = 512: one byte of padding.
        let b = build_ntb16(&frame(484), 0, &p, 512).unwrap();
        assert_eq!(b.len(), 513);
        assert_eq!(le16(&b, 8), 513);
        let mut n = 0;
        assert!(parse_ntb16(&b, |d| {
            assert_eq!(d.len(), 484);
            n += 1;
        }));
        assert_eq!(n, 1);
        let small = NtbParameters { out_max_size: 100, ..p };
        assert!(build_ntb16(&frame(200), 0, &small, 512).is_err());
    }

    #[test]
    fn ntb_parse_multiple_datagrams_and_chained_ndps() {
        // NTH, NDP at 12 with two datagrams and a next NDP at 64 with one.
        let mut b = vec![0u8; 256];
        b[0..4].copy_from_slice(b"NCMH");
        b[4..6].copy_from_slice(&12u16.to_le_bytes());
        b[8..10].copy_from_slice(&256u16.to_le_bytes());
        b[10..12].copy_from_slice(&12u16.to_le_bytes());
        b[12..16].copy_from_slice(b"NCM0");
        b[16..18].copy_from_slice(&20u16.to_le_bytes());
        b[18..20].copy_from_slice(&64u16.to_le_bytes());
        for (i, (idx, len)) in [(96u16, 20u16), (128, 30)].iter().enumerate() {
            b[20 + 4 * i..22 + 4 * i].copy_from_slice(&idx.to_le_bytes());
            b[22 + 4 * i..24 + 4 * i].copy_from_slice(&len.to_le_bytes());
        }
        b[64..68].copy_from_slice(b"NCM0");
        b[68..70].copy_from_slice(&16u16.to_le_bytes());
        b[72..74].copy_from_slice(&200u16.to_le_bytes());
        b[74..76].copy_from_slice(&40u16.to_le_bytes());
        let mut lens = Vec::new();
        assert!(parse_ntb16(&b, |d| lens.push(d.len())));
        assert_eq!(lens, vec![20, 30, 40]);

        // A datagram past the block end is rejected; an NDP pointing at
        // itself is not followed forever.
        let mut bad = b.clone();
        bad[74..76].copy_from_slice(&100u16.to_le_bytes());
        assert!(!parse_ntb16(&bad, |_| {}));
        let mut looped = b.clone();
        looped[18..20].copy_from_slice(&12u16.to_le_bytes());
        let mut count = 0;
        assert!(!parse_ntb16(&looped, |_| count += 1));
        assert!(count <= 32);
        assert!(!parse_ntb16(b"NCMX", |_| {}));
    }

    #[test]
    fn descriptors_and_notifications() {
        let comm = InterfaceDescriptor {
            number: 0,
            alternate_setting: 0,
            num_endpoints: 1,
            class: 2,
            sub_class: SUBCLASS_NCM,
            protocol: 0,
            string_index: 0,
            endpoints: Vec::new(),
            extra: vec![
                5, 0x24, 0, 0x10, 1, // header
                13, 0x24, 0x0f, 4, 0, 0, 0, 0, 0xea, 0x05, 0, 0, 0, // ethernet: MAC string 4, 1514
                5, 0x24, 6, 0, 1, // union 0 -> 1
                6, 0x24, 0x1a, 0, 1, 0x21, // NCM
            ],
        };
        let f = Functional::parse(&comm);
        assert_eq!(f.data_interface, Some(1));
        assert_eq!(f.mac_string, 4);
        assert_eq!(f.max_segment_size, 1514);
        assert_eq!(f.ncm_capabilities, Some(0x21));
        assert_eq!(parse_mac("02005E1000FF"), Some([2, 0, 0x5e, 0x10, 0, 0xff]));
        assert_eq!(parse_mac("02005E1000F"), None);
        assert_eq!(parse_mac("02005E1000FG"), None);

        assert_eq!(
            decode_notification(&[0xa1, 0, 1, 0, 0, 0, 0, 0]),
            Some(Notification::Connection(true))
        );
        let mut speed = vec![0xa1, 0x2a, 0, 0, 0, 0, 8, 0];
        speed.extend(1_000_000_000u32.to_le_bytes());
        speed.extend(100_000_000u32.to_le_bytes());
        assert_eq!(
            decode_notification(&speed),
            Some(Notification::Speed {
                down: 1_000_000_000,
                up: 100_000_000
            })
        );
        let mut p = vec![28, 0, 1, 0];
        p.extend(16384u32.to_le_bytes());
        p.extend([4, 0, 0, 0, 4, 0, 0, 0]);
        p.extend(8192u32.to_le_bytes());
        p.extend([4, 0, 2, 0, 4, 0, 1, 0]);
        let n = NtbParameters::parse(&p).unwrap();
        assert_eq!((n.in_max_size, n.out_max_size, n.out_remainder), (16384, 8192, 2));
    }
}
