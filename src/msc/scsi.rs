//! SCSI command blocks and response parsing (SPC-4 / SBC-3), limited to what
//! USB mass-storage devices implement.

use crate::ErrorKind;
use std::fmt;

/// Operation codes used by [`MassStorage`](super::MassStorage).
pub mod opcode {
    /// TEST UNIT READY
    pub const TEST_UNIT_READY: u8 = 0x00;
    /// REQUEST SENSE
    pub const REQUEST_SENSE: u8 = 0x03;
    /// INQUIRY
    pub const INQUIRY: u8 = 0x12;
    /// MODE SENSE (6)
    pub const MODE_SENSE_6: u8 = 0x1a;
    /// START STOP UNIT
    pub const START_STOP_UNIT: u8 = 0x1b;
    /// PREVENT ALLOW MEDIUM REMOVAL
    pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1e;
    /// READ CAPACITY (10)
    pub const READ_CAPACITY_10: u8 = 0x25;
    /// READ (10)
    pub const READ_10: u8 = 0x28;
    /// WRITE (10)
    pub const WRITE_10: u8 = 0x2a;
    /// SYNCHRONIZE CACHE (10)
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
    /// READ (16)
    pub const READ_16: u8 = 0x88;
    /// WRITE (16)
    pub const WRITE_16: u8 = 0x8a;
    /// SERVICE ACTION IN (16), whose action 0x10 is READ CAPACITY (16)
    pub const SERVICE_ACTION_IN_16: u8 = 0x9e;
}

/// Standard INQUIRY data (the first 36 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type (0 = direct access block device, 5 = CD/DVD).
    pub device_type: u8,
    /// Removable medium bit.
    pub removable: bool,
    /// Claimed SPC version.
    pub version: u8,
    /// T10 vendor identification, trimmed.
    pub vendor: String,
    /// Product identification, trimmed.
    pub product: String,
    /// Product revision level, trimmed.
    pub revision: String,
}

impl Inquiry {
    /// Parses standard INQUIRY data. Missing trailing bytes read as spaces.
    pub fn from_bytes(b: &[u8]) -> Option<Inquiry> {
        if b.len() < 5 {
            return None;
        }
        let text = |r: std::ops::Range<usize>| {
            let s = b.get(r.start..r.end.min(b.len())).unwrap_or(&[]);
            String::from_utf8_lossy(s).trim_matches(|c: char| c == ' ' || c == '\0').to_string()
        };
        Some(Inquiry {
            device_type: b[0] & 0x1f,
            removable: b[1] & 0x80 != 0,
            version: b[2],
            vendor: text(8..16),
            product: text(16..32),
            revision: text(32..36),
        })
    }
}

/// The size of a logical unit, from READ CAPACITY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capacity {
    /// Number of logical blocks.
    pub block_count: u64,
    /// Size of each block in bytes (almost always 512 or 4096; 2048 for
    /// optical media).
    pub block_size: u32,
}

impl Capacity {
    /// Total size in bytes.
    pub const fn bytes(&self) -> u64 {
        self.block_count * self.block_size as u64
    }
}

/// SCSI sense key (SPC-4 §4.5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SenseKey {
    /// No error.
    NoSense,
    /// The command succeeded after recovery.
    RecoveredError,
    /// The unit cannot be accessed right now (spinning up, no medium, ...).
    NotReady,
    /// Unrecoverable medium error.
    MediumError,
    /// Hardware failure.
    HardwareError,
    /// Bad command or parameter.
    IllegalRequest,
    /// The medium changed or the device was reset; retry the command.
    UnitAttention,
    /// Write-protected.
    DataProtect,
    /// Blank or unwritten medium.
    BlankCheck,
    /// Vendor-specific.
    VendorSpecific,
    /// Copy aborted.
    CopyAborted,
    /// The device aborted the command.
    AbortedCommand,
    /// The command reached the end of the medium.
    VolumeOverflow,
    /// Source data did not match the medium.
    Miscompare,
    /// Other values.
    Reserved(u8),
}

impl SenseKey {
    /// Decodes a 4-bit sense key.
    pub const fn from_code(key: u8) -> SenseKey {
        match key & 0x0f {
            0x0 => SenseKey::NoSense,
            0x1 => SenseKey::RecoveredError,
            0x2 => SenseKey::NotReady,
            0x3 => SenseKey::MediumError,
            0x4 => SenseKey::HardwareError,
            0x5 => SenseKey::IllegalRequest,
            0x6 => SenseKey::UnitAttention,
            0x7 => SenseKey::DataProtect,
            0x8 => SenseKey::BlankCheck,
            0x9 => SenseKey::VendorSpecific,
            0xa => SenseKey::CopyAborted,
            0xb => SenseKey::AbortedCommand,
            0xd => SenseKey::VolumeOverflow,
            0xe => SenseKey::Miscompare,
            k => SenseKey::Reserved(k),
        }
    }
}

/// Sense data returned by REQUEST SENSE after a failed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sense {
    /// The sense key: the broad category of the failure.
    pub key: SenseKey,
    /// Additional sense code.
    pub asc: u8,
    /// Additional sense code qualifier.
    pub ascq: u8,
}

impl Sense {
    /// Parses fixed-format (0x70/0x71) or descriptor-format (0x72/0x73)
    /// sense data.
    pub fn from_bytes(b: &[u8]) -> Option<Sense> {
        match b.first()? & 0x7f {
            0x70 | 0x71 => Some(Sense {
                key: SenseKey::from_code(*b.get(2)?),
                asc: b.get(12).copied().unwrap_or(0),
                ascq: b.get(13).copied().unwrap_or(0),
            }),
            0x72 | 0x73 => Some(Sense {
                key: SenseKey::from_code(*b.get(1)?),
                asc: b.get(2).copied().unwrap_or(0),
                ascq: b.get(3).copied().unwrap_or(0),
            }),
            _ => None,
        }
    }

    /// `true` for "medium not present" (ASC 0x3A): an empty card reader slot
    /// or optical drive.
    pub const fn medium_not_present(&self) -> bool {
        self.asc == 0x3a
    }

    /// The crate error kind that best describes this failure.
    pub fn error_kind(&self) -> ErrorKind {
        match self.key {
            SenseKey::NotReady if self.medium_not_present() => ErrorKind::NotFound,
            SenseKey::NotReady => ErrorKind::Busy,
            SenseKey::IllegalRequest => ErrorKind::NotSupported,
            SenseKey::DataProtect => ErrorKind::Access,
            SenseKey::VolumeOverflow => ErrorKind::InvalidParam,
            _ => ErrorKind::Io,
        }
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} (ASC {:#04x}, ASCQ {:#04x})", self.key, self.asc, self.ascq)
    }
}

/// Builds a READ or WRITE CDB, choosing the 10- or 16-byte form. Returns the
/// CDB and its length.
pub(crate) fn rw_cdb(write: bool, lba: u64, blocks: u32) -> ([u8; 16], usize) {
    let mut cdb = [0u8; 16];
    if lba + blocks as u64 <= u32::MAX as u64 + 1 && blocks <= u16::MAX as u32 {
        cdb[0] = if write { opcode::WRITE_10 } else { opcode::READ_10 };
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&(blocks as u16).to_be_bytes());
        (cdb, 10)
    } else {
        cdb[0] = if write { opcode::WRITE_16 } else { opcode::READ_16 };
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        (cdb, 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inquiry() {
        let mut b = [b' '; 36];
        b[0] = 0;
        b[1] = 0x80;
        b[2] = 6;
        b[8..15].copy_from_slice(b"SanDisk");
        b[16..28].copy_from_slice(b"Cruzer Blade");
        b[32..36].copy_from_slice(b"1.00");
        let i = Inquiry::from_bytes(&b).unwrap();
        assert!(i.removable);
        assert_eq!(i.vendor, "SanDisk");
        assert_eq!(i.product, "Cruzer Blade");
        assert_eq!(i.revision, "1.00");
        // Truncated INQUIRY data (some devices send 31 bytes).
        let i = Inquiry::from_bytes(&b[..20]).unwrap();
        assert_eq!(i.product, "Cruz");
        assert_eq!(i.revision, "");
    }

    #[test]
    fn sense() {
        let mut fixed = [0u8; 18];
        fixed[0] = 0xf0;
        fixed[2] = 0x02;
        fixed[12] = 0x3a;
        let s = Sense::from_bytes(&fixed).unwrap();
        assert_eq!(s.key, SenseKey::NotReady);
        assert!(s.medium_not_present());
        assert_eq!(s.error_kind(), ErrorKind::NotFound);
        let s = Sense::from_bytes(&[0x72, 0x06, 0x28, 0x00]).unwrap();
        assert_eq!(s.key, SenseKey::UnitAttention);
        assert_eq!(s.asc, 0x28);
        assert!(Sense::from_bytes(&[0x00, 0, 0]).is_none());
    }

    #[test]
    fn read_write_cdbs() {
        let (c, n) = rw_cdb(false, 0x1234_5678, 8);
        assert_eq!(n, 10);
        assert_eq!(&c[..10], &[0x28, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 8, 0]);
        // The last addressable block of a 2 TiB disk still fits READ (10).
        assert_eq!(rw_cdb(false, u32::MAX as u64, 1).1, 10);
        let (c, n) = rw_cdb(true, 1 << 32, 1);
        assert_eq!(n, 16);
        assert_eq!(&c[..16], &[0x8a, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(rw_cdb(false, 0, 0x10000).1, 16);
    }
}
