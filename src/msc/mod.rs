//! USB Mass Storage: the Bulk-Only Transport and the SCSI commands that USB
//! flash drives, card readers and external disks understand.
//!
//! This module is behind the `msc` cargo feature.
//!
//! [`MassStorage`] claims a bulk-only mass-storage interface and runs SCSI
//! commands on its logical units, with the error recovery the USB
//! specification mandates (clear-halt after a stall, reset recovery after a
//! phase error or timeout). [`BlockDevice`] turns a logical unit into a
//! `Read + Write + Seek` byte stream, so a filesystem or partition-table crate
//! can use it directly.
//!
//! ```no_run
//! use rawusb::msc::MassStorage;
//! use std::io::Read;
//!
//! let ctx = rawusb::Context::new()?;
//! let dev = ctx.find_device(0x0781, 0x5567)?.expect("drive not plugged in");
//! let msc = MassStorage::open(&dev)?;
//! let info = msc.inquiry(0)?;
//! let mut disk = msc.block_device(0)?;
//! println!("{} {}: {} bytes", info.vendor, info.product, disk.capacity().bytes());
//! let mut mbr = [0u8; 512];
//! disk.read_exact(&mut mbr)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Scope and platform notes
//!
//! Only the Bulk-Only Transport (`bInterfaceProtocol` 0x50) is supported,
//! which covers practically every device made this century; USB Attached
//! SCSI (UAS) devices also offer a bulk-only alternate setting, but the
//! legacy CBI floppy transport is not implemented.
//!
//! The operating system's storage driver owns these interfaces. On Linux it
//! is detached while [`MassStorage`] lives, which removes the block device
//! from the system: never do this to a mounted drive. On macOS the storage
//! driver cannot be displaced, and on Windows the device must be bound to
//! WinUSB.

pub mod scsi;

pub use scsi::{Capacity, Inquiry, Sense, SenseKey};

use crate::class::{self, Claim};
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::types::{ControlType, Direction, Recipient, TransferType, request_type};
use crate::{Error, ErrorKind, Result};
use scsi::opcode;
use std::io;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Default timeout for a whole command (CBW, data and CSW phases each).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Default largest data phase issued by [`BlockDevice`]: 120 KiB, the limit
/// Linux uses for USB storage because some bridges fail beyond it.
const DEFAULT_MAX_TRANSFER: usize = 120 * 1024;
/// How many times a command is retried after UNIT ATTENTION.
const UNIT_ATTENTION_RETRIES: usize = 3;

const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"
const REQ_RESET: u8 = 0xff;
const REQ_GET_MAX_LUN: u8 = 0xfe;

/// The data stage of a command.
#[derive(Debug)]
pub enum DataPhase<'a> {
    /// No data.
    None,
    /// Device to host; the buffer's length is the expected transfer length.
    In(&'a mut [u8]),
    /// Host to device.
    Out(&'a [u8]),
}

impl DataPhase<'_> {
    fn len(&self) -> usize {
        match self {
            DataPhase::None => 0,
            DataPhase::In(b) => b.len(),
            DataPhase::Out(b) => b.len(),
        }
    }
}

/// Outcome of a command that made it through the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandResult {
    /// `true` if the device reported success; `false` means CHECK CONDITION
    /// (issue [`MassStorage::request_sense`] to learn why).
    pub passed: bool,
    /// Bytes of the data phase actually moved.
    pub transferred: usize,
    /// `dCSWDataResidue`: how much of the expected data phase the device
    /// did not process.
    pub residue: u32,
}

struct State {
    tag: u32,
    timeout: Duration,
    max_transfer: usize,
}

/// A bulk-only mass-storage interface. See the [module documentation](self).
///
/// Commands are serialised internally, so the methods take `&self` and the
/// value can be shared between threads.
pub struct MassStorage {
    claim: Claim,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
    max_lun: u8,
    state: Mutex<State>,
}

impl std::fmt::Debug for MassStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MassStorage")
            .field("claim", &self.claim)
            .field("ep_in", &format_args!("{:#04x}", self.ep_in))
            .field("ep_out", &format_args!("{:#04x}", self.ep_out))
            .field("max_lun", &self.max_lun)
            .finish()
    }
}

fn is_bulk_only(a: &crate::InterfaceDescriptor) -> bool {
    a.class == crate::types::class::MASS_STORAGE && a.protocol == 0x50
}

/// The mass-storage interfaces of a device that offer the bulk-only
/// transport (on some alternate setting; UAS devices list it second), as the
/// descriptor of that alternate setting.
pub fn interfaces(device: &Device) -> Result<Vec<crate::InterfaceDescriptor>> {
    let cfg = device.active_config_descriptor()?;
    Ok(cfg
        .interfaces
        .iter()
        .filter_map(|i| i.alt_settings.iter().find(|a| is_bulk_only(a)).cloned())
        .collect())
}

impl MassStorage {
    /// Opens the first bulk-only mass-storage interface of a device. For a
    /// UAS device, the bulk-only alternate setting is selected.
    pub fn open(device: &Device) -> Result<MassStorage> {
        let iface = interfaces(device)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "device has no bulk-only mass-storage interface"))?;
        Self::open_interface(device.open()?, iface.number)
    }

    /// Opens a specific mass-storage interface of an already-open device.
    pub fn open_interface(handle: DeviceHandle, interface: u8) -> Result<MassStorage> {
        let cfg = handle.device().active_config_descriptor()?;
        let alt = cfg
            .interface(interface)
            .and_then(|i| i.alt_settings.iter().find(|a| is_bulk_only(a)))
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "not a bulk-only mass-storage interface"))?
            .clone();
        let ep = |dir| {
            class::endpoint(&alt, dir, TransferType::Bulk)
                .map(|e| e.address)
                .ok_or_else(|| Error::with_message(ErrorKind::Io, "mass-storage interface lacks a bulk endpoint"))
        };
        let (ep_in, ep_out) = (ep(Direction::In)?, ep(Direction::Out)?);
        let claim = Claim::new(handle, &[interface])?;
        if alt.alternate_setting != 0 {
            claim.handle().set_alternate_setting(interface, alt.alternate_setting)?;
        }
        let mut msc = MassStorage {
            claim,
            interface,
            ep_in,
            ep_out,
            max_lun: 0,
            state: Mutex::new(State {
                tag: 1,
                timeout: DEFAULT_TIMEOUT,
                max_transfer: DEFAULT_MAX_TRANSFER,
            }),
        };
        msc.max_lun = msc.read_max_lun();
        Ok(msc)
    }

    /// GET MAX LUN. Single-LUN devices may stall it, which means 0.
    fn read_max_lun(&self) -> u8 {
        let mut b = [0u8; 1];
        match self.handle().control_read(
            request_type(Direction::In, ControlType::Class, Recipient::Interface),
            REQ_GET_MAX_LUN,
            0,
            self.interface as u16,
            &mut b,
            Duration::from_secs(1),
        ) {
            Ok(1) => b[0].min(15),
            _ => 0,
        }
    }

    /// The underlying device handle.
    pub fn handle(&self) -> &DeviceHandle {
        self.claim.handle()
    }

    /// The claimed interface number.
    pub fn interface_number(&self) -> u8 {
        self.interface
    }

    /// The highest logical unit number (0 for single-LUN devices; card
    /// readers typically have one LUN per slot).
    pub fn max_lun(&self) -> u8 {
        self.max_lun
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the timeout applied to each phase of a command (20 s by default).
    pub fn set_timeout(&self, timeout: Duration) {
        self.lock().timeout = timeout;
    }

    /// Sets the largest data phase [`BlockDevice`] issues in one command
    /// (120 KiB by default). Raise it for speed on devices that cope.
    pub fn set_max_transfer(&self, bytes: usize) {
        self.lock().max_transfer = bytes.max(1);
    }

    // ----- transport -------------------------------------------------------

    /// Runs one command through the Bulk-Only Transport: the command block
    /// wrapper, the data phase, and the status wrapper.
    ///
    /// Transport failures (stalls on the status phase, phase errors, bad
    /// status wrappers, timeouts) trigger reset recovery and come back as
    /// errors. A command the device *rejects* is not an error here: it comes
    /// back with [`CommandResult::passed`] false.
    pub fn execute(&self, lun: u8, cdb: &[u8], data: DataPhase<'_>) -> Result<CommandResult> {
        let mut st = self.lock();
        self.transport(&mut st, lun, cdb, data)
    }

    fn transport(&self, st: &mut State, lun: u8, cdb: &[u8], data: DataPhase<'_>) -> Result<CommandResult> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "CDB must be 1 to 16 bytes"));
        }
        if lun > self.max_lun {
            return Err(Error::with_message(ErrorKind::InvalidParam, "no such logical unit"));
        }
        let expected = u32::try_from(data.len()).map_err(|_| Error::with_message(ErrorKind::InvalidParam, "data phase too large"))?;
        let tag = st.tag;
        st.tag = st.tag.wrapping_add(1);
        let timeout = st.timeout;
        let h = self.handle();

        let mut cbw = [0u8; 31];
        cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&expected.to_le_bytes());
        cbw[12] = if matches!(data, DataPhase::In(_)) { 0x80 } else { 0 };
        cbw[13] = lun;
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        if let Err(e) = h.bulk_write(self.ep_out, &cbw, timeout) {
            return Err(self.recover(e));
        }

        let moved = match data {
            DataPhase::None => Ok(0),
            DataPhase::In(buf) => h.bulk_read(self.ep_in, buf, timeout),
            DataPhase::Out(buf) => h.bulk_write(self.ep_out, buf, timeout),
        };
        let transferred = match moved {
            Ok(n) => n,
            // A stalled data phase is legal: clear it and read the status.
            Err(e) if e.is_stall() => {
                let ep = if cbw[12] & 0x80 != 0 { self.ep_in } else { self.ep_out };
                h.clear_halt(ep).map_err(|e| self.recover(e))?;
                0
            }
            Err(e) => return Err(self.recover(e)),
        };

        let mut csw = [0u8; 13];
        let got = match h.bulk_read(self.ep_in, &mut csw, timeout) {
            Err(e) if e.is_stall() => {
                h.clear_halt(self.ep_in).map_err(|e| self.recover(e))?;
                h.bulk_read(self.ep_in, &mut csw, timeout)
            }
            r => r,
        };
        match got {
            Ok(13) => {}
            Ok(_) => return Err(self.recover(Error::with_message(ErrorKind::Io, "short command status wrapper"))),
            Err(e) => return Err(self.recover(e)),
        }
        if le32(&csw, 0) != CSW_SIGNATURE || le32(&csw, 4) != tag {
            return Err(self.recover(Error::with_message(ErrorKind::Io, "invalid command status wrapper")));
        }
        let residue = le32(&csw, 8);
        match csw[12] {
            0 => Ok(CommandResult {
                passed: true,
                transferred,
                residue,
            }),
            1 => Ok(CommandResult {
                passed: false,
                transferred,
                residue,
            }),
            _ => Err(self.recover(Error::with_message(ErrorKind::Io, "mass-storage phase error"))),
        }
    }

    /// Performs reset recovery (unless the device is gone) and returns `e`.
    fn recover(&self, e: Error) -> Error {
        if !e.is_no_device() {
            let _ = self.reset_recovery();
        }
        e
    }

    /// Bulk-Only Mass Storage Reset followed by clearing both endpoint halts,
    /// which returns the device to a known state after a transport error.
    pub fn reset_recovery(&self) -> Result<()> {
        let h = self.handle();
        h.control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            REQ_RESET,
            0,
            self.interface as u16,
            &[],
            Duration::from_secs(5),
        )?;
        h.clear_halt(self.ep_in)?;
        h.clear_halt(self.ep_out)
    }

    // ----- SCSI ------------------------------------------------------------

    /// Runs a command and turns CHECK CONDITION into an error carrying the
    /// sense data, retrying after UNIT ATTENTION (which devices report once
    /// after a reset or medium change).
    fn command(&self, lun: u8, cdb: &[u8], mut data: DataPhase<'_>) -> Result<usize> {
        let mut st = self.lock();
        let mut attempts = 0;
        loop {
            let data = match &mut data {
                DataPhase::None => DataPhase::None,
                DataPhase::In(b) => DataPhase::In(b),
                DataPhase::Out(b) => DataPhase::Out(b),
            };
            let r = self.transport(&mut st, lun, cdb, data)?;
            if r.passed {
                return Ok(r.transferred);
            }
            let sense = self.sense_locked(&mut st, lun)?;
            if sense.key == SenseKey::UnitAttention && attempts < UNIT_ATTENTION_RETRIES {
                attempts += 1;
                continue;
            }
            return Err(Error::with_message(
                sense.error_kind(),
                format!("SCSI command {:#04x} failed: {sense}", cdb[0]),
            ));
        }
    }

    fn sense_locked(&self, st: &mut State, lun: u8) -> Result<Sense> {
        let mut buf = [0u8; 18];
        let cdb = [opcode::REQUEST_SENSE, 0, 0, 0, buf.len() as u8, 0];
        let r = self.transport(st, lun, &cdb, DataPhase::In(&mut buf))?;
        if !r.passed {
            return Err(Error::with_message(ErrorKind::Io, "REQUEST SENSE failed"));
        }
        Sense::from_bytes(&buf[..r.transferred]).ok_or_else(|| Error::with_message(ErrorKind::Io, "unrecognised sense data format"))
    }

    /// REQUEST SENSE: why the previous command on this LUN failed.
    pub fn request_sense(&self, lun: u8) -> Result<Sense> {
        let mut st = self.lock();
        self.sense_locked(&mut st, lun)
    }

    /// INQUIRY: device type, vendor, product and revision.
    pub fn inquiry(&self, lun: u8) -> Result<Inquiry> {
        let mut buf = [0u8; 36];
        let cdb = [opcode::INQUIRY, 0, 0, 0, buf.len() as u8, 0];
        let n = self.command(lun, &cdb, DataPhase::In(&mut buf))?;
        Inquiry::from_bytes(&buf[..n]).ok_or_else(|| Error::with_message(ErrorKind::Io, "short INQUIRY data"))
    }

    /// TEST UNIT READY: succeeds once the medium is present and usable.
    /// Fails with [`ErrorKind::NotFound`] for an empty slot and
    /// [`ErrorKind::Busy`] while the unit is becoming ready.
    pub fn test_unit_ready(&self, lun: u8) -> Result<()> {
        self.command(lun, &[opcode::TEST_UNIT_READY, 0, 0, 0, 0, 0], DataPhase::None)?;
        Ok(())
    }

    /// READ CAPACITY: the number and size of blocks. Falls back to the
    /// 16-byte form for units larger than 2 TiB.
    pub fn read_capacity(&self, lun: u8) -> Result<Capacity> {
        let mut b = [0u8; 8];
        let cdb = [opcode::READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if self.command(lun, &cdb, DataPhase::In(&mut b))? < 8 {
            return Err(Error::with_message(ErrorKind::Io, "short READ CAPACITY data"));
        }
        let last = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let block_size = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        let capacity = if last == u32::MAX {
            let mut b = [0u8; 32];
            let mut cdb = [0u8; 16];
            cdb[0] = opcode::SERVICE_ACTION_IN_16;
            cdb[1] = 0x10;
            cdb[10..14].copy_from_slice(&(b.len() as u32).to_be_bytes());
            if self.command(lun, &cdb, DataPhase::In(&mut b))? < 12 {
                return Err(Error::with_message(ErrorKind::Io, "short READ CAPACITY (16) data"));
            }
            Capacity {
                block_count: u64::from_be_bytes(b[0..8].try_into().unwrap_or_default()) + 1,
                block_size: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            }
        } else {
            Capacity {
                block_count: last as u64 + 1,
                block_size,
            }
        };
        if capacity.block_size == 0 {
            return Err(Error::with_message(ErrorKind::Io, "device reports a zero block size"));
        }
        Ok(capacity)
    }

    /// MODE SENSE: whether the medium is write-protected.
    pub fn is_write_protected(&self, lun: u8) -> Result<bool> {
        // Ask for all pages first; some devices only accept a header-sized
        // request.
        let mut b = [0u8; 192];
        for alloc in [192u8, 4] {
            let cdb = [opcode::MODE_SENSE_6, 0, 0x3f, 0, alloc, 0];
            match self.command(lun, &cdb, DataPhase::In(&mut b[..alloc as usize])) {
                Ok(n) if n >= 3 => return Ok(b[2] & 0x80 != 0),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotSupported => {}
                Err(e) => return Err(e),
            }
        }
        Err(Error::with_message(ErrorKind::NotSupported, "MODE SENSE not supported"))
    }

    /// SYNCHRONIZE CACHE: flushes the device's write cache. Devices without
    /// a cache commonly reject the command, which is reported as success.
    pub fn synchronize_cache(&self, lun: u8) -> Result<()> {
        match self.command(lun, &[opcode::SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0], DataPhase::None) {
            Err(e) if e.kind() == ErrorKind::NotSupported => Ok(()),
            r => r.map(drop),
        }
    }

    /// START STOP UNIT. `start: false, load_eject: true` ejects the medium.
    pub fn start_stop_unit(&self, lun: u8, start: bool, load_eject: bool) -> Result<()> {
        let flags = u8::from(start) | u8::from(load_eject) << 1;
        self.command(lun, &[opcode::START_STOP_UNIT, 0, 0, 0, flags, 0], DataPhase::None)?;
        Ok(())
    }

    /// PREVENT ALLOW MEDIUM REMOVAL: locks or unlocks the eject button.
    pub fn prevent_medium_removal(&self, lun: u8, prevent: bool) -> Result<()> {
        let cdb = [opcode::PREVENT_ALLOW_MEDIUM_REMOVAL, 0, 0, 0, u8::from(prevent), 0];
        self.command(lun, &cdb, DataPhase::None)?;
        Ok(())
    }

    /// Reads `buf.len() / block_size` whole blocks starting at `lba`, split
    /// into commands no larger than the maximum transfer size.
    pub fn read_blocks(&self, lun: u8, lba: u64, block_size: u32, buf: &mut [u8]) -> Result<()> {
        let bs = check_blocks(block_size, buf.len())?;
        let chunk = self.chunk_blocks(bs);
        for (i, part) in buf.chunks_mut(chunk * bs).enumerate() {
            let blocks = (part.len() / bs) as u32;
            let (cdb, n) = scsi::rw_cdb(false, lba + (i * chunk) as u64, blocks);
            if self.command(lun, &cdb[..n], DataPhase::In(part))? != part.len() {
                return Err(Error::with_message(ErrorKind::Io, "device returned fewer blocks than requested"));
            }
        }
        Ok(())
    }

    /// Writes whole blocks starting at `lba`.
    pub fn write_blocks(&self, lun: u8, lba: u64, block_size: u32, data: &[u8]) -> Result<()> {
        let bs = check_blocks(block_size, data.len())?;
        let chunk = self.chunk_blocks(bs);
        for (i, part) in data.chunks(chunk * bs).enumerate() {
            let blocks = (part.len() / bs) as u32;
            let (cdb, n) = scsi::rw_cdb(true, lba + (i * chunk) as u64, blocks);
            if self.command(lun, &cdb[..n], DataPhase::Out(part))? != part.len() {
                return Err(Error::with_message(ErrorKind::Io, "device accepted fewer blocks than sent"));
            }
        }
        Ok(())
    }

    fn chunk_blocks(&self, block_size: usize) -> usize {
        (self.lock().max_transfer / block_size).clamp(1, u16::MAX as usize)
    }

    /// Waits for the unit to become ready and wraps it as a byte-addressed
    /// [`BlockDevice`].
    pub fn block_device(&self, lun: u8) -> Result<BlockDevice<'_>> {
        let mut last = None;
        for _ in 0..10 {
            match self.test_unit_ready(lun) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) if e.kind() == ErrorKind::Busy => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(e) = last {
            return Err(e);
        }
        let capacity = self.read_capacity(lun)?;
        Ok(BlockDevice {
            msc: self,
            lun,
            capacity,
            pos: 0,
            scratch: Vec::new(),
        })
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn check_blocks(block_size: u32, len: usize) -> Result<usize> {
    let bs = block_size as usize;
    if bs == 0 || !len.is_multiple_of(bs) {
        return Err(Error::with_message(
            ErrorKind::InvalidParam,
            "buffer is not a whole number of blocks",
        ));
    }
    Ok(bs)
}

/// One logical unit as a byte-addressed `Read + Write + Seek` stream.
///
/// Aligned whole-block I/O goes straight to the device; anything else reads
/// (and for writes, rewrites) the blocks it touches. [`flush`](io::Write::flush)
/// issues SYNCHRONIZE CACHE. Reads and writes stop at the end of the unit.
#[derive(Debug)]
pub struct BlockDevice<'a> {
    msc: &'a MassStorage,
    lun: u8,
    capacity: Capacity,
    pos: u64,
    scratch: Vec<u8>,
}

impl BlockDevice<'_> {
    /// The unit's size, as read when this value was created.
    pub fn capacity(&self) -> Capacity {
        self.capacity
    }

    /// The logical unit number.
    pub fn lun(&self) -> u8 {
        self.lun
    }

    /// Reads whole blocks at `lba` (the stream position is not used or moved).
    pub fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(lba, buf.len())?;
        self.msc.read_blocks(self.lun, lba, self.capacity.block_size, buf)
    }

    /// Writes whole blocks at `lba` (the stream position is not used or moved).
    pub fn write_blocks(&self, lba: u64, data: &[u8]) -> Result<()> {
        self.check_range(lba, data.len())?;
        self.msc.write_blocks(self.lun, lba, self.capacity.block_size, data)
    }

    fn check_range(&self, lba: u64, len: usize) -> Result<()> {
        let blocks = (len / self.capacity.block_size as usize) as u64;
        match lba.checked_add(blocks) {
            Some(end) if end <= self.capacity.block_count => Ok(()),
            _ => Err(Error::with_message(ErrorKind::InvalidParam, "blocks past the end of the unit")),
        }
    }

    fn bs(&self) -> u64 {
        self.capacity.block_size as u64
    }

    fn remaining(&self) -> u64 {
        self.capacity.bytes().saturating_sub(self.pos)
    }

    /// Whole blocks that can be moved directly at the current position, or 0
    /// when the next access has to go through the scratch block.
    fn direct_blocks(&self, len: usize) -> usize {
        let bs = self.bs();
        if !self.pos.is_multiple_of(bs) {
            return 0;
        }
        let len = (len as u64).min(self.remaining());
        (len / bs) as usize
    }

    fn load_scratch(&mut self, lba: u64) -> Result<()> {
        self.scratch.resize(self.bs() as usize, 0);
        self.msc.read_blocks(self.lun, lba, self.capacity.block_size, &mut self.scratch)
    }
}

impl io::Read for BlockDevice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.remaining() == 0 {
            return Ok(0);
        }
        let bs = self.bs();
        let lba = self.pos / bs;
        let n = match self.direct_blocks(buf.len()) {
            0 => {
                self.load_scratch(lba)?;
                let off = (self.pos % bs) as usize;
                let n = (bs as usize - off).min(buf.len()).min(self.remaining() as usize);
                buf[..n].copy_from_slice(&self.scratch[off..off + n]);
                n
            }
            blocks => {
                let n = blocks * bs as usize;
                self.msc.read_blocks(self.lun, lba, self.capacity.block_size, &mut buf[..n])?;
                n
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl io::Write for BlockDevice<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.remaining() == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "end of the unit"));
        }
        let bs = self.bs();
        let lba = self.pos / bs;
        let n = match self.direct_blocks(data.len()) {
            0 => {
                // Read-modify-write the one block this touches.
                self.load_scratch(lba)?;
                let off = (self.pos % bs) as usize;
                let n = (bs as usize - off).min(data.len()).min(self.remaining() as usize);
                self.scratch[off..off + n].copy_from_slice(&data[..n]);
                self.msc.write_blocks(self.lun, lba, self.capacity.block_size, &self.scratch)?;
                n
            }
            blocks => {
                let n = blocks * bs as usize;
                self.msc.write_blocks(self.lun, lba, self.capacity.block_size, &data[..n])?;
                n
            }
        };
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(self.msc.synchronize_cache(self.lun)?)
    }
}

impl io::Seek for BlockDevice<'_> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let size = self.capacity.bytes();
        let new = match pos {
            io::SeekFrom::Start(p) => Some(p),
            io::SeekFrom::End(d) => size.checked_add_signed(d),
            io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match new {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the unit")),
        }
    }
}
