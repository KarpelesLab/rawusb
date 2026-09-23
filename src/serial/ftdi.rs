//! FTDI USB-to-serial chips: chip identification, baud-rate divisors and the
//! vendor request set, following the Linux `ftdi_sio` driver and FTDI's
//! application note AN232B-05.

use crate::{Error, ErrorKind, Result};

/// FTDI's USB vendor ID. Rebranded devices may use another one; open them
/// with [`SerialPort::open_ftdi`](super::SerialPort::open_ftdi).
pub const VENDOR_ID: u16 = 0x0403;

/// Vendor requests (`bRequest`).
pub(crate) mod req {
    pub(crate) const RESET: u8 = 0x00;
    pub(crate) const SET_MODEM_CTRL: u8 = 0x01;
    pub(crate) const SET_FLOW_CTRL: u8 = 0x02;
    pub(crate) const SET_BAUD_RATE: u8 = 0x03;
    pub(crate) const SET_DATA: u8 = 0x04;
    pub(crate) const GET_MODEM_STATUS: u8 = 0x05;
    pub(crate) const SET_LATENCY_TIMER: u8 = 0x09;
    pub(crate) const GET_LATENCY_TIMER: u8 = 0x0a;
    pub(crate) const SET_BITMODE: u8 = 0x0b;
    pub(crate) const READ_PINS: u8 = 0x0c;
}

/// `wValue` of RESET: reset the port, or discard buffered data. The
/// historical "purge RX/TX" names are inverted from the host's point of
/// view; these follow libftdi 1.5's corrected naming.
pub(crate) mod reset {
    pub(crate) const SIO: u16 = 0;
    /// Discard data written by the host but not yet sent on the line.
    pub(crate) const FLUSH_OUTPUT: u16 = 1;
    /// Discard data received from the line but not yet read by the host.
    pub(crate) const FLUSH_INPUT: u16 = 2;
}

/// Pin modes for [`SerialPort::set_bitmode`](super::SerialPort::set_bitmode).
pub mod bitmode {
    /// Back to the normal UART (or FIFO) function.
    pub const RESET: u8 = 0x00;
    /// Asynchronous bit-bang.
    pub const BITBANG: u8 = 0x01;
    /// MPSSE: the SPI/I2C/JTAG engine (2232C/D, H-series).
    pub const MPSSE: u8 = 0x02;
    /// Synchronous bit-bang.
    pub const SYNC_BITBANG: u8 = 0x04;
    /// MCU host bus emulation.
    pub const MCU: u8 = 0x08;
    /// Fast opto-isolated serial.
    pub const OPTO: u8 = 0x10;
    /// CBUS bit-bang (232R, 232H, FT-X).
    pub const CBUS: u8 = 0x20;
    /// Synchronous 245 FIFO (2232H, 232H).
    pub const SYNC_FIFO: u8 = 0x40;
}

/// The FTDI chip family, identified from `bcdDevice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FtdiChip {
    /// FT8U232AM.
    Am,
    /// FT232BM / FT245BM.
    Bm,
    /// FT2232C/D (dual port).
    Ft2232C,
    /// FT232R / FT245R.
    Ft232R,
    /// FT2232H (dual port, high speed).
    Ft2232H,
    /// FT4232H (quad port, high speed).
    Ft4232H,
    /// FT232H (high speed).
    Ft232H,
    /// FT-X series (FT230X, FT231X, FT234XD, ...).
    FtX,
    /// FT2233HP.
    Ft2233HP,
    /// FT4233HP.
    Ft4233HP,
    /// FT2232HP.
    Ft2232HP,
    /// FT4232HP.
    Ft4232HP,
    /// FT233HP.
    Ft233HP,
    /// FT232HP.
    Ft232HP,
    /// FT4232HA.
    Ft4232HA,
    /// An unrecognised `bcdDevice`; treated like a high-speed chip.
    Unknown(u16),
}

impl FtdiChip {
    /// Identifies the chip. `None` for the original SIO (bcdDevice below
    /// 0x200), whose different protocol is not supported. An AM-looking
    /// device without a serial number may be a BM (a known silicon quirk),
    /// which [`SerialPort::open_ftdi`](super::SerialPort::open_ftdi) resolves.
    pub const fn from_bcd_device(bcd: u16) -> Option<FtdiChip> {
        Some(match bcd {
            0x0200 => FtdiChip::Am,
            0x0400 => FtdiChip::Bm,
            0x0500 => FtdiChip::Ft2232C,
            0x0600 => FtdiChip::Ft232R,
            0x0700 => FtdiChip::Ft2232H,
            0x0800 => FtdiChip::Ft4232H,
            0x0900 => FtdiChip::Ft232H,
            0x1000 => FtdiChip::FtX,
            0x2800 => FtdiChip::Ft2233HP,
            0x2900 => FtdiChip::Ft4233HP,
            0x3000 => FtdiChip::Ft2232HP,
            0x3100 => FtdiChip::Ft4232HP,
            0x3200 => FtdiChip::Ft233HP,
            0x3300 => FtdiChip::Ft232HP,
            0x3600 => FtdiChip::Ft4232HA,
            b if b < 0x0200 => return None,
            b => FtdiChip::Unknown(b),
        })
    }

    /// Identifies the chip of an open device, resolving the AM/BM ambiguity
    /// the way Linux does: an FT232BM without a serial number reports 0x200,
    /// but unlike an AM it has a readable latency timer.
    pub(crate) fn probe(handle: &crate::DeviceHandle, interface: u8) -> Result<FtdiChip> {
        let d = handle.device().device_descriptor();
        let chip = FtdiChip::from_bcd_device(d.device_version.0)
            .ok_or_else(|| Error::with_message(ErrorKind::NotSupported, "original FTDI SIO chips are not supported"))?;
        if chip == FtdiChip::Am && d.serial_number_string_index == 0 {
            let mut b = [0u8; 1];
            let rt = crate::request_type(crate::Direction::In, crate::ControlType::Vendor, crate::Recipient::Device);
            let probe = handle.control_read(
                rt,
                req::GET_LATENCY_TIMER,
                0,
                interface as u16,
                &mut b,
                std::time::Duration::from_secs(1),
            );
            if probe.is_ok() {
                return Ok(FtdiChip::Bm);
            }
        }
        Ok(chip)
    }

    /// `true` for the high-speed parts, whose baud generator runs from a
    /// 120 MHz clock.
    pub const fn is_high_speed(self) -> bool {
        !matches!(
            self,
            FtdiChip::Am | FtdiChip::Bm | FtdiChip::Ft2232C | FtdiChip::Ft232R | FtdiChip::FtX
        )
    }

    /// The port index used in `wIndex`: 0 for chips that only ever had one
    /// port (whose firmware predates port selection), otherwise the 1-based
    /// port (interface number + 1).
    pub(crate) const fn channel(self, interface: u8) -> u16 {
        match self {
            FtdiChip::Am | FtdiChip::Bm | FtdiChip::Ft232R => 0,
            _ => interface as u16 + 1,
        }
    }

    /// Fastest supported baud rate.
    pub const fn max_baud_rate(self) -> u32 {
        if self.is_high_speed() { 12_000_000 } else { 3_000_000 }
    }
}

/// Maps the three fractional bits of a divisor to the chip's encoding.
const DIVFRAC: [u32; 8] = [0, 3, 2, 4, 1, 5, 6, 7];

/// Divisor for the 232AM, which only supports fractions of 1/8, 1/4 and 1/2.
fn am_divisor(baud: u32) -> Option<u32> {
    let mut divisor3 = (48_000_000 + baud) / (2 * baud);
    if divisor3 & 7 == 7 {
        divisor3 += 1; // round x.875 up to x+1
    }
    let mut divisor = divisor3 >> 3;
    if divisor > 0x3fff {
        return None;
    }
    divisor |= match divisor3 & 7 {
        0 if divisor == 1 => return Some(0), // 3 Mbaud
        0 => 0,
        1 => 0xc000,   // +0.125
        4.. => 0x4000, // +0.5
        _ => 0x8000,   // +0.25
    };
    Some(divisor)
}

/// Divisor for the 232BM and later: `base / baud` in 1/8 steps.
fn bm_divisor(divisor3: u32, high_speed: bool) -> Option<u32> {
    let whole = divisor3 >> 3;
    if whole > 0x3fff {
        return None;
    }
    let mut divisor = whole | DIVFRAC[(divisor3 & 7) as usize] << 14;
    // The two highest rates have dedicated encodings.
    if divisor == 1 {
        divisor = 0;
    } else if divisor == 0x4001 {
        divisor = 1;
    }
    // Bit 17 switches the H-series generator from 48 MHz to 120 MHz.
    Some(if high_speed { divisor | 0x2_0000 } else { divisor })
}

/// The (`wValue`, `wIndex`) pair of SET_BAUD_RATE for a chip, port and rate.
pub(crate) fn baud_request(chip: FtdiChip, channel: u16, baud: u32) -> Result<(u16, u16)> {
    let bad = || Error::with_message(ErrorKind::InvalidParam, "baud rate out of range for this FTDI chip");
    if baud == 0 || baud > chip.max_baud_rate() {
        return Err(bad());
    }
    let divisor = match chip {
        FtdiChip::Am => am_divisor(baud),
        c if c.is_high_speed() && baud >= 1200 => {
            // 120 MHz clock, 10x oversampling: divisor3 = 8 * 12 MHz / baud.
            let divisor3 = ((8 * 120_000_000u64 + 5 * baud as u64) / (10 * baud as u64)) as u32;
            bm_divisor(divisor3, true)
        }
        _ => bm_divisor((48_000_000 + baud) / (2 * baud), false),
    }
    .ok_or_else(bad)?;
    let value = divisor as u16;
    let mut index = (divisor >> 16) as u16;
    if channel != 0 {
        index = index << 8 | channel;
    }
    Ok((value, index))
}

/// Decodes the two status bytes FTDI chips prefix to every IN packet (and
/// return from GET_MODEM_STATUS).
pub(crate) fn decode_status(b0: u8, b1: u8) -> super::ModemStatus {
    super::ModemStatus {
        cts: b0 & 0x10 != 0,
        dsr: b0 & 0x20 != 0,
        ring: b0 & 0x40 != 0,
        dcd: b0 & 0x80 != 0,
        overrun: b1 & 0x02 != 0,
        parity_error: b1 & 0x04 != 0,
        framing_error: b1 & 0x08 != 0,
        break_received: b1 & 0x10 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_identification() {
        assert_eq!(FtdiChip::from_bcd_device(0x0700), Some(FtdiChip::Ft2232H));
        assert_eq!(FtdiChip::from_bcd_device(0x0100), None);
        assert_eq!(FtdiChip::from_bcd_device(0x4200), Some(FtdiChip::Unknown(0x4200)));
        assert!(FtdiChip::Ft232H.is_high_speed());
        assert!(!FtdiChip::FtX.is_high_speed());
        assert_eq!(FtdiChip::Ft232R.channel(0), 0);
        assert_eq!(FtdiChip::Ft2232H.channel(1), 2);
        assert_eq!(FtdiChip::FtX.channel(0), 1);
    }

    #[test]
    fn divisors_match_an232b_05() {
        // Values from FTDI's AN232B-05 and the Linux driver.
        assert_eq!(baud_request(FtdiChip::Bm, 0, 9600).unwrap(), (0x4138, 0));
        assert_eq!(baud_request(FtdiChip::Bm, 0, 115_200).unwrap(), (0x001a, 0));
        assert_eq!(baud_request(FtdiChip::Bm, 0, 3_000_000).unwrap(), (0, 0));
        assert_eq!(baud_request(FtdiChip::Bm, 0, 2_000_000).unwrap(), (1, 0));
        assert_eq!(baud_request(FtdiChip::Am, 0, 9600).unwrap(), (0x4138, 0));
        assert_eq!(baud_request(FtdiChip::Am, 0, 3_000_000).unwrap(), (0, 0));
        // 300 baud: divisor 10000 exactly.
        assert_eq!(baud_request(FtdiChip::Ft232R, 0, 300).unwrap(), (10000, 0));
        // The fraction's top bit lands in wIndex; on multi-port chips it
        // moves to the high byte and the port goes in the low byte.
        let (v, i) = baud_request(FtdiChip::Ft2232C, 1, 1_000_000).unwrap();
        assert_eq!((v, i), (3, 0x0001));
        let (v, i) = baud_request(FtdiChip::Ft2232C, 2, 921_600).unwrap();
        assert_eq!(v, 0x8003); // 3.25 -> fraction code 2 (0.25)
        assert_eq!(i, 0x0002);
    }

    #[test]
    fn high_speed_divisors() {
        // 120 MHz / 10 / 115200 = 104.1666 -> 104 + 1/8 (code 3, 0xc000).
        assert_eq!(baud_request(FtdiChip::Ft2232H, 1, 115_200).unwrap(), (0xc068, 0x0201));
        assert_eq!(baud_request(FtdiChip::Ft232H, 1, 12_000_000).unwrap(), (0, 0x0201));
        // Below 1200 baud the 48 MHz path takes over (no bit 17).
        assert_eq!(baud_request(FtdiChip::Ft2232H, 1, 300).unwrap(), (10000, 0x0001));
        assert!(baud_request(FtdiChip::Ft2232H, 1, 12_000_001).is_err());
        assert!(baud_request(FtdiChip::Bm, 0, 3_000_001).is_err());
        assert!(baud_request(FtdiChip::Bm, 0, 150).is_err(), "divisor above 0x3fff");
        assert!(baud_request(FtdiChip::Bm, 0, 0).is_err());
    }

    #[test]
    fn status_bytes() {
        let s = decode_status(0x31, 0x60);
        assert!(s.cts && s.dsr && !s.ring && !s.dcd);
        assert!(!s.overrun && !s.framing_error);
        let s = decode_status(0x01, 0x1e);
        assert!(s.overrun && s.parity_error && s.framing_error && s.break_received);
    }
}
