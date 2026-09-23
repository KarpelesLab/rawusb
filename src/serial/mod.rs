//! USB serial adapters: CDC-ACM devices (Arduino-style boards, modems,
//! most microcontroller USB stacks) and FTDI chips, behind one
//! [`SerialPort`] type.
//!
//! This module is behind the `serial` cargo feature.
//!
//! ```no_run
//! use rawusb::serial::{LineConfig, SerialPort};
//! use std::io::{Read, Write};
//! use std::time::Duration;
//!
//! let ctx = rawusb::Context::new()?;
//! let dev = ctx.find_device(0x0403, 0x6001)?.expect("adapter not plugged in");
//! let mut port = SerialPort::open(&dev)?;
//! port.set_line_config(&LineConfig::new(115_200))?;
//! port.set_dtr(true)?;
//! port.set_read_timeout(Duration::from_millis(500));
//! port.write_all(b"AT\r")?;
//! let mut reply = [0u8; 64];
//! let n = port.read(&mut reply)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Opening a port does not change its line settings or modem lines; set what
//! you need. Many CDC-ACM devices only start talking once DTR is asserted.
//!
//! # Platform notes
//!
//! The operating system's serial driver normally owns these interfaces. On
//! Linux it is detached while the port is open (the `/dev/ttyUSB*` or
//! `/dev/ttyACM*` node disappears) and re-attached when it is dropped. On
//! macOS the Apple CDC and FTDI drivers cannot be displaced, and on Windows
//! the device must be bound to WinUSB; prefer the OS serial port there.

mod cdc;
pub mod ftdi;

pub use ftdi::FtdiChip;

use crate::class::{self, Claim};
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::transfer::Transfer;
use crate::types::{ControlType, Direction, Recipient, TransferStatus, TransferType, request_type};
use crate::{Error, ErrorKind, NO_TIMEOUT, Result};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Timeout for configuration requests.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

/// Number of data bits per character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataBits {
    /// 5 bits.
    Five,
    /// 6 bits.
    Six,
    /// 7 bits.
    Seven,
    /// 8 bits.
    Eight,
}

impl DataBits {
    const fn count(self) -> u8 {
        match self {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        }
    }
}

/// Parity bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Parity {
    /// No parity bit.
    None,
    /// Odd parity.
    Odd,
    /// Even parity.
    Even,
    /// Parity bit always 1.
    Mark,
    /// Parity bit always 0.
    Space,
}

impl Parity {
    /// The encoding shared by CDC `bParityType` and FTDI SET_DATA.
    const fn code(self) -> u8 {
        match self {
            Parity::None => 0,
            Parity::Odd => 1,
            Parity::Even => 2,
            Parity::Mark => 3,
            Parity::Space => 4,
        }
    }
}

/// Number of stop bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopBits {
    /// 1 stop bit.
    One,
    /// 1.5 stop bits.
    OnePointFive,
    /// 2 stop bits.
    Two,
}

impl StopBits {
    /// The encoding shared by CDC `bCharFormat` and FTDI SET_DATA.
    const fn code(self) -> u8 {
        match self {
            StopBits::One => 0,
            StopBits::OnePointFive => 1,
            StopBits::Two => 2,
        }
    }
}

/// Line settings: baud rate and character framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineConfig {
    /// Bits per second.
    pub baud_rate: u32,
    /// Data bits per character.
    pub data_bits: DataBits,
    /// Parity.
    pub parity: Parity,
    /// Stop bits.
    pub stop_bits: StopBits,
}

impl LineConfig {
    /// `baud_rate` with 8 data bits, no parity, 1 stop bit.
    pub const fn new(baud_rate: u32) -> LineConfig {
        LineConfig {
            baud_rate,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
        }
    }
}

impl Default for LineConfig {
    /// 9600 8N1, the traditional default.
    fn default() -> Self {
        LineConfig::new(9600)
    }
}

/// Hardware or software flow control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlowControl {
    /// None.
    None,
    /// RTS/CTS hardware handshake.
    RtsCts,
    /// DTR/DSR hardware handshake.
    DtrDsr,
    /// XON/XOFF software flow control with the given characters
    /// (conventionally 0x11 and 0x13).
    XonXoff {
        /// Resume character.
        xon: u8,
        /// Pause character.
        xoff: u8,
    },
}

/// Modem input lines and line errors.
///
/// The error and break flags latch: they report whether the condition
/// occurred since the previous [`SerialPort::modem_status`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ModemStatus {
    /// Clear To Send. CDC-ACM has no way to report it and always reads false.
    pub cts: bool,
    /// Data Set Ready.
    pub dsr: bool,
    /// Ring Indicator.
    pub ring: bool,
    /// Data Carrier Detect.
    pub dcd: bool,
    /// Received data was lost.
    pub overrun: bool,
    /// A character arrived with bad parity.
    pub parity_error: bool,
    /// A character arrived with a bad stop bit.
    pub framing_error: bool,
    /// A break condition was received.
    pub break_received: bool,
}

impl ModemStatus {
    /// Merges fresh line state into the latched status: lines are replaced,
    /// events accumulate.
    fn update(&mut self, new: ModemStatus) {
        let events = ModemStatus {
            overrun: self.overrun | new.overrun,
            parity_error: self.parity_error | new.parity_error,
            framing_error: self.framing_error | new.framing_error,
            break_received: self.break_received | new.break_received,
            ..new
        };
        *self = events;
    }

    fn take_events(&mut self) -> ModemStatus {
        let out = *self;
        self.overrun = false;
        self.parity_error = false;
        self.framing_error = false;
        self.break_received = false;
        out
    }
}

/// A serial port a device offers, as listed by [`ports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortInfo {
    /// The interface to pass to [`SerialPort::open_interface`]: the
    /// communications interface of a CDC-ACM function, or an FTDI port's
    /// interface.
    pub interface: u8,
    /// Which protocol the port speaks.
    pub kind: PortKind,
}

/// The protocol of a [`PortInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortKind {
    /// CDC Abstract Control Model.
    CdcAcm,
    /// An FTDI chip's port.
    Ftdi,
}

/// The serial ports of a device, in descriptor order: every CDC-ACM
/// function, and on FTDI devices (vendor ID 0x0403) every port. Composite
/// devices often have several (a debug probe with two UARTs, a modem with
/// AT and diagnostic ports).
pub fn ports(device: &Device) -> Result<Vec<PortInfo>> {
    let ftdi = device.vendor_id() == ftdi::VENDOR_ID;
    let is_ftdi_port = |a: &crate::InterfaceDescriptor| {
        class::endpoint(a, Direction::In, TransferType::Bulk).is_some() && class::endpoint(a, Direction::Out, TransferType::Bulk).is_some()
    };
    Ok(class::interfaces_where(device, |a| cdc::is_acm(a) || ftdi && is_ftdi_port(a))?
        .into_iter()
        .map(|a| PortInfo {
            interface: a.number,
            kind: if cdc::is_acm(&a) { PortKind::CdcAcm } else { PortKind::Ftdi },
        })
        .collect())
}

/// Which kind of adapter a [`SerialPort`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SerialKind {
    /// A CDC Abstract Control Model device.
    CdcAcm,
    /// An FTDI chip.
    Ftdi(FtdiChip),
}

enum Backend {
    Acm {
        comm: u8,
        /// The interrupt transfer that listens for SERIAL_STATE, resubmitted
        /// from its own callback until dropped.
        _notify: Option<Notifications>,
    },
    Ftdi {
        chip: FtdiChip,
        channel: u16,
        /// Current SET_DATA word, kept to toggle the break bit.
        data_word: u16,
    },
}

struct State {
    backend: Backend,
    line: Option<LineConfig>,
    dtr: bool,
    rts: bool,
}

/// A USB serial port. See the [module documentation](self).
///
/// Methods take `&self`: one thread may block in
/// [`read_with_timeout`](Self::read_with_timeout) while
/// another writes or changes modem lines. `std::io::Read` and `Write` are
/// implemented for both `SerialPort` and `&SerialPort`, using the timeouts set
/// with [`set_read_timeout`](Self::set_read_timeout) and
/// [`set_write_timeout`](Self::set_write_timeout).
pub struct SerialPort {
    // Declared before `claim` so that it drops first: the CDC notification
    // transfer is stopped before the interfaces are released.
    state: Mutex<State>,
    kind: SerialKind,
    ep_in: u8,
    ep_in_packet: usize,
    ep_out: u8,
    rx: Mutex<VecDeque<u8>>,
    status: Arc<Mutex<ModemStatus>>,
    timeouts: Mutex<(Duration, Duration)>,
    claim: Claim,
}

impl std::fmt::Debug for SerialPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialPort")
            .field("kind", &self.kind)
            .field("claim", &self.claim)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SerialPort {
    /// Opens the first serial port of a device (see [`ports`]).
    pub fn open(device: &Device) -> Result<SerialPort> {
        let first = ports(device)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::with_message(ErrorKind::NotSupported, "not a recognised USB serial adapter (CDC-ACM or FTDI)"))?;
        Self::open_port(device.open()?, first)
    }

    /// Opens the port on the given interface, whichever kind it is.
    pub fn open_interface(handle: DeviceHandle, interface: u8) -> Result<SerialPort> {
        let port = ports(handle.device())?
            .into_iter()
            .find(|p| p.interface == interface)
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "no serial port on that interface"))?;
        Self::open_port(handle, port)
    }

    /// Opens every serial port of a device through one handle. Fails (and
    /// opens none) if any of them cannot be opened.
    pub fn open_all(handle: &DeviceHandle) -> Result<Vec<SerialPort>> {
        ports(handle.device())?
            .into_iter()
            .map(|p| Self::open_port(handle.clone(), p))
            .collect()
    }

    fn open_port(handle: DeviceHandle, port: PortInfo) -> Result<SerialPort> {
        match port.kind {
            PortKind::CdcAcm => Self::open_cdc_acm(handle, port.interface),
            PortKind::Ftdi => Self::open_ftdi(handle, port.interface),
        }
    }

    /// Opens a CDC-ACM port given its communications interface. The matching
    /// data interface is found from the class descriptors and claimed too.
    pub fn open_cdc_acm(handle: DeviceHandle, comm_interface: u8) -> Result<SerialPort> {
        let cfg = handle.device().active_config_descriptor()?;
        let comm = class::interface(handle.device(), comm_interface)?;
        if !cdc::is_acm(&comm) {
            return Err(Error::with_message(
                ErrorKind::InvalidParam,
                "not a CDC-ACM communications interface",
            ));
        }
        let data_number =
            cdc::data_interface(&cfg, &comm).ok_or_else(|| Error::with_message(ErrorKind::Io, "CDC-ACM function has no data interface"))?;
        // Some devices put the data endpoints on a non-zero alternate setting.
        let data = cfg
            .interface(data_number)
            .and_then(|i| {
                i.alt_settings.iter().find(|a| {
                    class::endpoint(a, Direction::In, TransferType::Bulk).is_some()
                        && class::endpoint(a, Direction::Out, TransferType::Bulk).is_some()
                })
            })
            .cloned()
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "CDC data interface lacks bulk endpoints"))?;
        let notify_ep = class::endpoint(&comm, Direction::In, TransferType::Interrupt).map(|e| (e.address, e.max_packet_size()));

        let claim = Claim::new(handle, &[comm_interface, data_number])?;
        if data.alternate_setting != 0 {
            claim.handle().set_alternate_setting(data_number, data.alternate_setting)?;
        }
        let bulk = |dir| class::endpoint(&data, dir, TransferType::Bulk).ok_or_else(|| Error::new(ErrorKind::Io));
        let (ep_in, packet) = bulk(Direction::In).map(|e| (e.address, e.max_packet_size()))?;
        let ep_out = bulk(Direction::Out)?.address;

        let status = Arc::new(Mutex::new(ModemStatus::default()));
        let notify = match notify_ep {
            Some((ep, size)) => start_notifications(claim.handle(), ep, size as usize, Arc::clone(&status)),
            None => None,
        };
        Ok(SerialPort {
            claim,
            kind: SerialKind::CdcAcm,
            ep_in,
            ep_in_packet: packet as usize,
            ep_out,
            state: Mutex::new(State {
                backend: Backend::Acm {
                    comm: comm_interface,
                    _notify: notify,
                },
                line: None,
                dtr: false,
                rts: false,
            }),
            rx: Mutex::new(VecDeque::new()),
            status,
            timeouts: Mutex::new((NO_TIMEOUT, NO_TIMEOUT)),
        })
    }

    /// Opens one port of an FTDI chip. Multi-port chips (FT2232, FT4232)
    /// expose one interface per port. Works for any vendor ID, for rebranded
    /// FTDI devices.
    pub fn open_ftdi(handle: DeviceHandle, interface: u8) -> Result<SerialPort> {
        let iface = class::interface(handle.device(), interface)?;
        let ep_in = class::endpoint(&iface, Direction::In, TransferType::Bulk)
            .map(|e| (e.address, e.max_packet_size() as usize))
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "FTDI interface lacks a bulk IN endpoint"))?;
        let ep_out = class::endpoint(&iface, Direction::Out, TransferType::Bulk)
            .map(|e| e.address)
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "FTDI interface lacks a bulk OUT endpoint"))?;
        let claim = Claim::new(handle, &[interface])?;
        let chip = ftdi::FtdiChip::probe(claim.handle(), interface)?;
        let port = SerialPort {
            claim,
            kind: SerialKind::Ftdi(chip),
            ep_in: ep_in.0,
            ep_in_packet: ep_in.1.max(3),
            ep_out,
            state: Mutex::new(State {
                backend: Backend::Ftdi {
                    chip,
                    channel: chip.channel(interface),
                    data_word: 8,
                },
                line: None,
                dtr: false,
                rts: false,
            }),
            rx: Mutex::new(VecDeque::new()),
            status: Arc::new(Mutex::new(ModemStatus::default())),
            timeouts: Mutex::new((NO_TIMEOUT, NO_TIMEOUT)),
        };
        // Reset the port's state machine, as the Linux driver does on open.
        port.ftdi_request(ftdi::req::RESET, ftdi::reset::SIO, 0)?;
        Ok(port)
    }

    /// The underlying device handle.
    pub fn handle(&self) -> &DeviceHandle {
        self.claim.handle()
    }

    /// Which kind of adapter this is.
    pub fn kind(&self) -> SerialKind {
        self.kind
    }

    // ----- control ---------------------------------------------------------

    fn acm_request(&self, comm: u8, request: u8, value: u16, data: &[u8]) -> Result<()> {
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            request,
            value,
            comm as u16,
            data,
            CONTROL_TIMEOUT,
        )?;
        Ok(())
    }

    /// A vendor OUT request to an FTDI port. `index_high` is combined with
    /// the port's channel number in `wIndex`.
    fn ftdi_request(&self, request: u8, value: u16, index_high: u16) -> Result<()> {
        let channel = match lock(&self.state).backend {
            Backend::Ftdi { channel, .. } => channel,
            Backend::Acm { .. } => return Err(not_ftdi()),
        };
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Vendor, Recipient::Device),
            request,
            value,
            index_high | channel,
            &[],
            CONTROL_TIMEOUT,
        )?;
        Ok(())
    }

    fn ftdi_read(&self, request: u8, buf: &mut [u8]) -> Result<()> {
        let channel = match lock(&self.state).backend {
            Backend::Ftdi { channel, .. } => channel,
            Backend::Acm { .. } => return Err(not_ftdi()),
        };
        let n = self.handle().control_read(
            request_type(Direction::In, ControlType::Vendor, Recipient::Device),
            request,
            0,
            channel,
            buf,
            CONTROL_TIMEOUT,
        )?;
        if n < buf.len() {
            return Err(Error::with_message(ErrorKind::Io, "short response from FTDI chip"));
        }
        Ok(())
    }

    /// Sets baud rate and framing.
    pub fn set_line_config(&self, config: &LineConfig) -> Result<()> {
        let mut st = lock(&self.state);
        match &mut st.backend {
            Backend::Acm { comm, .. } => {
                let mut coding = [0u8; 7];
                coding[..4].copy_from_slice(&config.baud_rate.to_le_bytes());
                coding[4] = config.stop_bits.code();
                coding[5] = config.parity.code();
                coding[6] = config.data_bits.count();
                let comm = *comm;
                drop(st);
                self.acm_request(comm, cdc::req::SET_LINE_CODING, 0, &coding)?;
            }
            Backend::Ftdi { chip, channel, data_word } => {
                if !matches!(config.data_bits, DataBits::Seven | DataBits::Eight) {
                    return Err(Error::with_message(
                        ErrorKind::NotSupported,
                        "FTDI chips only support 7 or 8 data bits",
                    ));
                }
                let (value, index) = ftdi::baud_request(*chip, *channel, config.baud_rate)?;
                let word = config.data_bits.count() as u16 | (config.parity.code() as u16) << 8 | (config.stop_bits.code() as u16) << 11;
                *data_word = word;
                drop(st);
                let rt = request_type(Direction::Out, ControlType::Vendor, Recipient::Device);
                self.handle()
                    .control_write(rt, ftdi::req::SET_BAUD_RATE, value, index, &[], CONTROL_TIMEOUT)?;
                self.ftdi_request(ftdi::req::SET_DATA, word, 0)?;
            }
        }
        lock(&self.state).line = Some(*config);
        Ok(())
    }

    /// Shortcut for [`set_line_config`](Self::set_line_config) with 8N1.
    pub fn set_baud_rate(&self, baud_rate: u32) -> Result<()> {
        let mut config = lock(&self.state).line.unwrap_or_default();
        config.baud_rate = baud_rate;
        self.set_line_config(&config)
    }

    /// The line settings. For CDC-ACM this asks the device; for FTDI (which
    /// cannot report them) it is the last configuration set through this
    /// port, or `None` if there was none.
    pub fn line_config(&self) -> Result<Option<LineConfig>> {
        let comm = {
            let st = lock(&self.state);
            match st.backend {
                Backend::Acm { comm, .. } => comm,
                Backend::Ftdi { .. } => return Ok(st.line),
            }
        };
        let mut b = [0u8; 7];
        let n = self.handle().control_read(
            request_type(Direction::In, ControlType::Class, Recipient::Interface),
            cdc::req::GET_LINE_CODING,
            0,
            comm as u16,
            &mut b,
            CONTROL_TIMEOUT,
        )?;
        if n < 7 {
            return Err(Error::with_message(ErrorKind::Io, "short GET_LINE_CODING response"));
        }
        let data_bits = match b[6] {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        };
        let parity = match b[5] {
            1 => Parity::Odd,
            2 => Parity::Even,
            3 => Parity::Mark,
            4 => Parity::Space,
            _ => Parity::None,
        };
        let stop_bits = match b[4] {
            1 => StopBits::OnePointFive,
            2 => StopBits::Two,
            _ => StopBits::One,
        };
        Ok(Some(LineConfig {
            baud_rate: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data_bits,
            parity,
            stop_bits,
        }))
    }

    /// Sets flow control. CDC-ACM has no flow-control request, so anything
    /// but [`FlowControl::None`] fails with [`ErrorKind::NotSupported`] there.
    pub fn set_flow_control(&self, flow: FlowControl) -> Result<()> {
        match self.kind {
            SerialKind::CdcAcm if flow == FlowControl::None => Ok(()),
            SerialKind::CdcAcm => Err(Error::with_message(ErrorKind::NotSupported, "CDC-ACM has no flow-control request")),
            SerialKind::Ftdi(_) => {
                let (value, mode) = match flow {
                    FlowControl::None => (0, 0),
                    FlowControl::RtsCts => (0, 0x01),
                    FlowControl::DtrDsr => (0, 0x02),
                    FlowControl::XonXoff { xon, xoff } => ((xoff as u16) << 8 | xon as u16, 0x04),
                };
                self.ftdi_request(ftdi::req::SET_FLOW_CTRL, value, mode << 8)
            }
        }
    }

    /// Asserts or clears DTR (Data Terminal Ready).
    pub fn set_dtr(&self, on: bool) -> Result<()> {
        self.set_lines(Some(on), None)
    }

    /// Asserts or clears RTS (Request To Send).
    pub fn set_rts(&self, on: bool) -> Result<()> {
        self.set_lines(None, Some(on))
    }

    fn set_lines(&self, dtr: Option<bool>, rts: Option<bool>) -> Result<()> {
        let mut st = lock(&self.state);
        let new_dtr = dtr.unwrap_or(st.dtr);
        let new_rts = rts.unwrap_or(st.rts);
        match &st.backend {
            Backend::Acm { comm, .. } => {
                let comm = *comm;
                drop(st);
                let value = u16::from(new_dtr) | u16::from(new_rts) << 1;
                self.acm_request(comm, cdc::req::SET_CONTROL_LINE_STATE, value, &[])?;
            }
            Backend::Ftdi { .. } => {
                drop(st);
                // High byte: which lines to change; low byte: their values.
                let mask = u16::from(dtr.is_some()) | u16::from(rts.is_some()) << 1;
                let value = mask << 8 | u16::from(new_dtr) | u16::from(new_rts) << 1;
                self.ftdi_request(ftdi::req::SET_MODEM_CTRL, value, 0)?;
            }
        }
        st = lock(&self.state);
        st.dtr = new_dtr;
        st.rts = new_rts;
        Ok(())
    }

    /// Starts (`true`) or stops sending a break condition.
    pub fn set_break(&self, on: bool) -> Result<()> {
        let st = lock(&self.state);
        match st.backend {
            Backend::Acm { comm, .. } => {
                drop(st);
                self.acm_request(comm, cdc::req::SEND_BREAK, if on { 0xffff } else { 0 }, &[])
            }
            Backend::Ftdi { data_word, .. } => {
                drop(st);
                let word = if on { data_word | 1 << 14 } else { data_word };
                self.ftdi_request(ftdi::req::SET_DATA, word, 0)
            }
        }
    }

    /// Sends a break of the given length, blocking for its duration.
    pub fn send_break(&self, duration: Duration) -> Result<()> {
        self.set_break(true)?;
        std::thread::sleep(duration);
        self.set_break(false)
    }

    /// The modem input lines, plus line errors and breaks seen since the last
    /// call. FTDI chips are asked directly; CDC-ACM devices report changes
    /// through their notification endpoint, which is watched in the
    /// background (all false if the device has none).
    pub fn modem_status(&self) -> Result<ModemStatus> {
        if let SerialKind::Ftdi(_) = self.kind {
            let mut b = [0u8; 2];
            self.ftdi_read(ftdi::req::GET_MODEM_STATUS, &mut b)?;
            lock(&self.status).update(ftdi::decode_status(b[0], b[1]));
        }
        Ok(lock(&self.status).take_events())
    }

    /// Discards buffered data: `input` drops data received but not yet read,
    /// `output` data written but not yet sent. CDC-ACM has no such request,
    /// so there only this side's receive buffer is dropped.
    pub fn purge(&self, input: bool, output: bool) -> Result<()> {
        if input {
            lock(&self.rx).clear();
        }
        if let SerialKind::Ftdi(_) = self.kind {
            if input {
                self.ftdi_request(ftdi::req::RESET, ftdi::reset::FLUSH_INPUT, 0)?;
            }
            if output {
                self.ftdi_request(ftdi::req::RESET, ftdi::reset::FLUSH_OUTPUT, 0)?;
            }
        }
        Ok(())
    }

    // ----- FTDI-specific ---------------------------------------------------

    /// FTDI only: sets how long the chip holds a partly filled packet before
    /// sending it (1-255 ms, 16 by default). Lower means less latency for
    /// small messages at the cost of more USB traffic.
    pub fn set_latency_timer(&self, ms: u8) -> Result<()> {
        if ms == 0 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "latency timer must be at least 1 ms"));
        }
        self.ftdi_request(ftdi::req::SET_LATENCY_TIMER, ms as u16, 0)
    }

    /// FTDI only: the latency timer in milliseconds.
    pub fn latency_timer(&self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.ftdi_read(ftdi::req::GET_LATENCY_TIMER, &mut b)?;
        Ok(b[0])
    }

    /// FTDI only: switches the pins to another mode (see [`ftdi::bitmode`]);
    /// `mask` selects outputs for the bit-bang modes. Use
    /// [`ftdi::bitmode::RESET`] to return to UART mode.
    pub fn set_bitmode(&self, mask: u8, mode: u8) -> Result<()> {
        self.ftdi_request(ftdi::req::SET_BITMODE, (mode as u16) << 8 | mask as u16, 0)
    }

    /// FTDI only: the instantaneous state of the data pins (bit-bang modes).
    pub fn read_pins(&self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.ftdi_read(ftdi::req::READ_PINS, &mut b)?;
        Ok(b[0])
    }

    // ----- data ------------------------------------------------------------

    /// Sets the timeout used by the `std::io::Read` implementation.
    /// [`NO_TIMEOUT`] (the default) blocks until data
    /// arrives.
    pub fn set_read_timeout(&self, timeout: Duration) {
        lock(&self.timeouts).0 = timeout;
    }

    /// Sets the timeout used by the `std::io::Write` implementation.
    pub fn set_write_timeout(&self, timeout: Duration) {
        lock(&self.timeouts).1 = timeout;
    }

    /// Reads whatever is available, waiting up to `timeout` for the first
    /// byte ([`NO_TIMEOUT`] waits forever). Returns the
    /// number of bytes read, never 0 for a non-empty `buf`; fails with
    /// [`ErrorKind::Timeout`] if nothing arrived.
    pub fn read_with_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut rx = lock(&self.rx);
        if !rx.is_empty() {
            return Ok(drain(&mut rx, buf));
        }
        let deadline = (timeout != NO_TIMEOUT).then(|| Instant::now() + timeout);
        let packet = self.ep_in_packet;
        let ftdi = matches!(self.kind, SerialKind::Ftdi(_));
        // FTDI packets carry two status bytes each; read whole packets so the
        // device never overruns the buffer.
        let payload = if ftdi { packet - 2 } else { packet };
        let packets = buf.len().div_ceil(payload).clamp(1, 64);
        let mut raw = vec![0u8; packets * packet];
        loop {
            let wait = match deadline {
                None => NO_TIMEOUT,
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(left) if left >= Duration::from_millis(1) => left,
                    _ => return Err(Error::new(ErrorKind::Timeout)),
                },
            };
            let n = self.handle().bulk_read(self.ep_in, &mut raw, wait)?;
            if ftdi {
                // An FTDI chip sends a status-only packet every latency
                // period even when idle: strip, record, and keep waiting.
                let mut status = lock(&self.status);
                for p in raw[..n].chunks(packet) {
                    if p.len() >= 2 {
                        status.update(ftdi::decode_status(p[0], p[1]));
                        rx.extend(&p[2..]);
                    }
                }
            } else {
                rx.extend(&raw[..n]);
            }
            if !rx.is_empty() {
                return Ok(drain(&mut rx, buf));
            }
        }
    }

    /// Writes `data`, blocking until the device accepted all of it or the
    /// timeout expired. Returns the number of bytes sent.
    pub fn write_with_timeout(&self, data: &[u8], timeout: Duration) -> Result<usize> {
        self.handle().bulk_write(self.ep_out, data, timeout)
    }
}

fn not_ftdi() -> Error {
    Error::with_message(ErrorKind::NotSupported, "only FTDI chips support this request")
}

fn drain(rx: &mut VecDeque<u8>, buf: &mut [u8]) -> usize {
    let n = rx.len().min(buf.len());
    for (dst, src) in buf.iter_mut().zip(rx.drain(..n)) {
        *dst = src;
    }
    n
}

/// A self-resubmitting interrupt transfer on the CDC notification endpoint.
struct Notifications {
    transfer: Transfer,
    /// Set on drop. The callback checks it and resubmits under this lock, so
    /// once the flag is set any resubmission already happened (and can be
    /// cancelled) or never will.
    stop: Arc<Mutex<bool>>,
}

/// Keeps an interrupt transfer queued on the CDC notification endpoint,
/// folding SERIAL_STATE notifications into `status`.
fn start_notifications(handle: &DeviceHandle, ep: u8, size: usize, status: Arc<Mutex<ModemStatus>>) -> Option<Notifications> {
    let transfer = Transfer::interrupt(handle, ep, vec![0u8; size.max(16)]);
    let stop = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&stop);
    transfer
        .set_callback(move |t| {
            if t.status() != TransferStatus::Completed {
                return; // cancelled, unplugged, or stalled: stop listening
            }
            if let Ok(data) = t.data()
                && let Some(s) = cdc::decode_serial_state(&data)
            {
                lock(&status).update(s);
            }
            let stopped = lock(&flag);
            if !*stopped {
                let _ = t.submit();
            }
        })
        .ok()?;
    transfer.submit().ok()?;
    Some(Notifications { transfer, stop })
}

impl Drop for Notifications {
    fn drop(&mut self) {
        *lock(&self.stop) = true;
        let _ = self.transfer.cancel();
        let _ = self.transfer.wait(Some(Duration::from_secs(1)));
    }
}

impl io::Read for &SerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = lock(&self.timeouts).0;
        Ok(self.read_with_timeout(buf, timeout)?)
    }
}

impl io::Write for &SerialPort {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let timeout = lock(&self.timeouts).1;
        Ok(self.write_with_timeout(data, timeout)?)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes complete only once the device has taken the data.
        Ok(())
    }
}

impl io::Read for SerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl io::Write for SerialPort {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        (&*self).write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_events_latch() {
        let mut s = ModemStatus::default();
        s.update(ModemStatus {
            cts: true,
            overrun: true,
            ..Default::default()
        });
        s.update(ModemStatus {
            dsr: true,
            ..Default::default()
        });
        let got = s.take_events();
        assert!(!got.cts && got.dsr && got.overrun, "lines replace, events accumulate");
        assert!(!s.overrun && s.dsr, "events clear once reported, lines stay");
    }

    #[test]
    fn drain_partial() {
        let mut rx: VecDeque<u8> = (1..=5).collect();
        let mut buf = [0u8; 3];
        assert_eq!(drain(&mut rx, &mut buf), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(rx.len(), 2);
    }
}
