//! CDC-ACM (USB CDC 1.2 PSTN subclass) descriptors, requests and
//! notifications.

use crate::class;
use crate::descriptors::{ConfigDescriptor, InterfaceDescriptor};

/// Class requests.
pub(crate) mod req {
    pub(crate) const SET_LINE_CODING: u8 = 0x20;
    pub(crate) const GET_LINE_CODING: u8 = 0x21;
    pub(crate) const SET_CONTROL_LINE_STATE: u8 = 0x22;
    pub(crate) const SEND_BREAK: u8 = 0x23;
}

const CS_INTERFACE: u8 = 0x24;
const SUBTYPE_CALL_MANAGEMENT: u8 = 0x01;
const SUBTYPE_UNION: u8 = 0x06;
const NOTIFY_SERIAL_STATE: u8 = 0x20;

/// `true` for a CDC Abstract Control Model communications interface.
pub(crate) fn is_acm(a: &InterfaceDescriptor) -> bool {
    a.class == crate::types::class::COMM && a.sub_class == 0x02
}

/// Finds the data interface that belongs to an ACM communications interface:
/// the union descriptor's first subordinate, else the call management
/// descriptor's data interface, else the next interface if it is a CDC data
/// interface (as Linux does for devices with broken descriptors).
pub(crate) fn data_interface(cfg: &ConfigDescriptor, comm: &InterfaceDescriptor) -> Option<u8> {
    let is_data = |n: u8| {
        cfg.interface(n).is_some_and(|i| {
            i.alt_settings
                .iter()
                .any(|a| a.class == crate::types::class::DATA || !a.endpoints.is_empty())
        })
    };
    let mut union = None;
    let mut call_mgmt = None;
    for (ty, d) in class::descriptors(&comm.extra) {
        if ty != CS_INTERFACE || d.len() < 3 {
            continue;
        }
        match d[2] {
            SUBTYPE_UNION if d.len() >= 5 => union = Some(d[4]),
            SUBTYPE_CALL_MANAGEMENT if d.len() >= 5 => call_mgmt = Some(d[4]),
            _ => {}
        }
    }
    [union, call_mgmt, comm.number.checked_add(1)]
        .into_iter()
        .flatten()
        .find(|&n| n != comm.number && is_data(n))
}

/// Parses a SERIAL_STATE notification. `None` for other notifications.
pub(crate) fn decode_serial_state(n: &[u8]) -> Option<super::ModemStatus> {
    if n.len() < 10 || n[1] != NOTIFY_SERIAL_STATE {
        return None;
    }
    let bits = class::le16(n, 8);
    Some(super::ModemStatus {
        dcd: bits & 0x01 != 0,
        dsr: bits & 0x02 != 0,
        break_received: bits & 0x04 != 0,
        ring: bits & 0x08 != 0,
        framing_error: bits & 0x10 != 0,
        parity_error: bits & 0x20 != 0,
        overrun: bits & 0x40 != 0,
        cts: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_state() {
        let s = decode_serial_state(&[0xa1, 0x20, 0, 0, 0, 0, 2, 0, 0x43, 0]).unwrap();
        assert!(s.dcd && s.dsr && s.overrun && !s.ring && !s.break_received);
        assert!(decode_serial_state(&[0xa1, 0x2a, 0, 0, 0, 0, 2, 0, 3, 0]).is_none());
        assert!(decode_serial_state(&[0xa1, 0x20, 0]).is_none());
    }

    #[test]
    fn finds_data_interface() {
        // Config: comm interface 0 (with union 0 -> 1) + data interface 1.
        let raw = [
            9, 2, 63, 0, 2, 1, 0, 0x80, 50, // config
            9, 4, 0, 0, 1, 2, 2, 1, 0, // comm interface
            5, 0x24, 0, 0x10, 1, // header
            5, 0x24, 1, 0, 1, // call management -> 1
            5, 0x24, 6, 0, 1, // union 0 -> 1
            7, 5, 0x83, 3, 8, 0, 16, // notification endpoint
            9, 4, 1, 0, 2, 0x0a, 0, 0, 0, // data interface
            7, 5, 0x81, 2, 64, 0, 0, 7, 5, 0x02, 2, 64, 0, 0,
        ];
        let cfg = ConfigDescriptor::from_bytes(&raw).unwrap();
        let comm = cfg.interface(0).unwrap().first();
        assert!(is_acm(comm));
        assert_eq!(data_interface(&cfg, comm), Some(1));
    }
}
