//! Remote NDIS (Microsoft RNDIS 1.0): control messages carried by CDC
//! encapsulated commands, and the packet framing of the data interface.

use crate::class::le16;
use crate::handle::DeviceHandle;
use crate::types::{ControlType, Direction, Recipient, request_type};
use crate::{Error, ErrorKind, Result};
use std::time::Duration;

const SEND_ENCAPSULATED_COMMAND: u8 = 0x00;
const GET_ENCAPSULATED_RESPONSE: u8 = 0x01;
/// Largest control response we accept, as Linux sizes it.
const CONTROL_BUFFER: usize = 1025;

const MSG_PACKET: u32 = 0x0000_0001;
const MSG_INIT: u32 = 0x0000_0002;
const MSG_HALT: u32 = 0x0000_0003;
const MSG_QUERY: u32 = 0x0000_0004;
const MSG_SET: u32 = 0x0000_0005;
const MSG_INDICATE: u32 = 0x0000_0007;
const MSG_KEEPALIVE: u32 = 0x0000_0008;
const COMPLETION: u32 = 0x8000_0000;

/// Header of a PACKET message; the data follows it.
pub(crate) const PACKET_HEADER: usize = 44;

/// OIDs used here.
pub(crate) mod oid {
    pub(crate) const GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010e;
    pub(crate) const GEN_MAXIMUM_FRAME_SIZE: u32 = 0x0001_0106;
    pub(crate) const GEN_LINK_SPEED: u32 = 0x0001_0107;
    pub(crate) const GEN_MEDIA_CONNECT_STATUS: u32 = 0x0001_0114;
    pub(crate) const PERMANENT_ADDRESS: u32 = 0x0101_0101;
}

/// NDIS packet filter bits.
pub(crate) mod filter {
    pub(crate) const DIRECTED: u32 = 0x01;
    pub(crate) const ALL_MULTICAST: u32 = 0x04;
    pub(crate) const BROADCAST: u32 = 0x08;
    pub(crate) const PROMISCUOUS: u32 = 0x20;
}

fn le32(b: &[u8], at: usize) -> u32 {
    le16(b, at) as u32 | (le16(b, at + 2) as u32) << 16
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// The INITIALIZE_CMPLT fields we use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InitInfo {
    /// Largest transfer the device accepts from the host.
    pub(crate) max_transfer_size: u32,
}

/// Wraps one Ethernet frame in a PACKET message. `pad_multiple` works as for
/// NCM: an exact multiple of the packet size gets one trailing byte, which
/// the message length does not cover.
pub(crate) fn build_packet(frame: &[u8], pad_multiple: usize) -> Vec<u8> {
    let total = PACKET_HEADER + frame.len();
    let mut b = Vec::with_capacity(total + 1);
    put32(&mut b, MSG_PACKET);
    put32(&mut b, total as u32);
    put32(&mut b, (PACKET_HEADER - 8) as u32); // DataOffset, from byte 8
    put32(&mut b, frame.len() as u32);
    b.resize(PACKET_HEADER, 0); // no out-of-band or per-packet data
    b.extend_from_slice(frame);
    if pad_multiple > 0 && total.is_multiple_of(pad_multiple) {
        b.push(0);
    }
    b
}

/// Splits a data transfer into the frames of its PACKET messages. Returns
/// `false` on a malformed message (frames before it are still emitted).
pub(crate) fn parse_packets(mut b: &[u8], mut emit: impl FnMut(&[u8])) -> bool {
    while b.len() >= 8 {
        let (ty, len) = (le32(b, 0), le32(b, 4) as usize);
        if ty == 0 && len == 0 {
            return true; // zero padding at the end of a transfer
        }
        if ty != MSG_PACKET || len < PACKET_HEADER || len > b.len() {
            return false;
        }
        let offset = 8 + le32(b, 8) as usize;
        let data_len = le32(b, 12) as usize;
        match offset.checked_add(data_len) {
            Some(end) if offset >= PACKET_HEADER && end <= len => emit(&b[offset..end]),
            _ => return false,
        }
        b = &b[len..];
    }
    true
}

/// Media status carried by an INDICATE_STATUS message.
pub(crate) fn indicated_link(msg: &[u8]) -> Option<bool> {
    if msg.len() < 12 || le32(msg, 0) != MSG_INDICATE {
        return None;
    }
    match le32(msg, 8) {
        0x4001_000b => Some(true),
        0x4001_000c => Some(false),
        _ => None,
    }
}

/// The control channel of one RNDIS function.
#[derive(Debug)]
pub(crate) struct Control {
    handle: DeviceHandle,
    interface: u8,
    notify_ep: Option<u8>,
    next_id: u32,
    /// Media status seen in INDICATE_STATUS messages while waiting for
    /// replies.
    pub(crate) link: Option<bool>,
}

impl Control {
    pub(crate) fn new(handle: DeviceHandle, interface: u8, notify_ep: Option<u8>) -> Control {
        Control {
            handle,
            interface,
            notify_ep,
            next_id: 1,
            link: None,
        }
    }

    fn send(&self, msg: &[u8]) -> Result<()> {
        self.handle.control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            SEND_ENCAPSULATED_COMMAND,
            0,
            self.interface as u16,
            msg,
            Duration::from_secs(5),
        )?;
        Ok(())
    }

    /// Sends a request and waits for its completion, handling the device's
    /// own status indications and keepalives in between.
    fn exchange(&mut self, msg_type: u32, body: &[u8]) -> Result<Vec<u8>> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut msg = Vec::with_capacity(12 + body.len());
        put32(&mut msg, msg_type);
        put32(&mut msg, (12 + body.len()) as u32);
        put32(&mut msg, id);
        msg.extend_from_slice(body);
        self.send(&msg)?;

        let mut buf = vec![0u8; CONTROL_BUFFER];
        for _ in 0..20 {
            // The device announces a response on the interrupt endpoint.
            // Some never do, so a timeout there only means "try reading".
            match self.notify_ep {
                Some(ep) => {
                    // RESPONSE_AVAILABLE is 8 bytes; leave room for devices
                    // with larger interrupt packets.
                    let mut n = [0u8; 64];
                    match self.handle.interrupt_read(ep, &mut n, Duration::from_millis(500)) {
                        Ok(_) => {}
                        Err(e) if e.is_timeout() => {}
                        Err(e) => return Err(e),
                    }
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
            let n = match self.handle.control_read(
                request_type(Direction::In, ControlType::Class, Recipient::Interface),
                GET_ENCAPSULATED_RESPONSE,
                0,
                self.interface as u16,
                &mut buf,
                Duration::from_secs(5),
            ) {
                Ok(n) => n,
                Err(e) if e.is_stall() => continue, // nothing queued yet
                Err(e) => return Err(e),
            };
            let reply = &buf[..n];
            if reply.len() < 12 {
                continue;
            }
            let ty = le32(reply, 0);
            if ty == msg_type | COMPLETION && le32(reply, 8) == id {
                if reply.len() >= 16 && le32(reply, 12) != 0 {
                    return Err(Error::with_message(
                        ErrorKind::Io,
                        format!("RNDIS request failed with status {:#010x}", le32(reply, 12)),
                    ));
                }
                return Ok(reply.to_vec());
            }
            if let Some(up) = indicated_link(reply) {
                self.link = Some(up);
            } else if ty == MSG_KEEPALIVE {
                let mut ack = Vec::with_capacity(16);
                put32(&mut ack, MSG_KEEPALIVE | COMPLETION);
                put32(&mut ack, 16);
                put32(&mut ack, le32(reply, 8));
                put32(&mut ack, 0);
                self.send(&ack)?;
            }
        }
        Err(Error::with_message(ErrorKind::Timeout, "no reply to RNDIS request"))
    }

    /// INITIALIZE, announcing the largest transfer we will read.
    pub(crate) fn initialize(&mut self, host_max_transfer: u32) -> Result<InitInfo> {
        let mut body = Vec::with_capacity(12);
        put32(&mut body, 1); // major version
        put32(&mut body, 0); // minor version
        put32(&mut body, host_max_transfer);
        let r = self.exchange(MSG_INIT, &body)?;
        if r.len() < 40 {
            return Err(Error::with_message(ErrorKind::Io, "short RNDIS INITIALIZE reply"));
        }
        Ok(InitInfo {
            max_transfer_size: le32(&r, 36),
        })
    }

    /// QUERY an OID; returns its value.
    pub(crate) fn query(&mut self, oid: u32, expect: usize) -> Result<Vec<u8>> {
        let mut body = Vec::with_capacity(16 + expect);
        put32(&mut body, oid);
        put32(&mut body, expect as u32);
        put32(&mut body, 20); // buffer offset, from byte 8
        put32(&mut body, 0); // device VC handle
        body.resize(16 + expect, 0);
        let r = self.exchange(MSG_QUERY, &body)?;
        let len = le32(&r, 16) as usize;
        let off = 8 + le32(&r, 20) as usize;
        r.get(off..off.saturating_add(len))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "malformed RNDIS QUERY reply"))
    }

    /// SET an OID.
    pub(crate) fn set(&mut self, oid: u32, value: &[u8]) -> Result<()> {
        let mut body = Vec::with_capacity(16 + value.len());
        put32(&mut body, oid);
        put32(&mut body, value.len() as u32);
        put32(&mut body, 20);
        put32(&mut body, 0);
        body.extend_from_slice(value);
        self.exchange(MSG_SET, &body)?;
        Ok(())
    }

    pub(crate) fn query_u32(&mut self, oid: u32) -> Result<u32> {
        let v = self.query(oid, 4)?;
        if v.len() < 4 {
            return Err(Error::with_message(ErrorKind::Io, "short RNDIS OID value"));
        }
        Ok(le32(&v, 0))
    }

    /// HALT: tells the device the host is done (no reply is sent).
    pub(crate) fn halt(&mut self) -> Result<()> {
        let mut msg = Vec::with_capacity(12);
        put32(&mut msg, MSG_HALT);
        put32(&mut msg, 12);
        put32(&mut msg, self.next_id);
        self.send(&msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip() {
        let frame: Vec<u8> = (0..60).collect();
        let b = build_packet(&frame, 512);
        assert_eq!(b.len(), 44 + 60);
        assert_eq!(le32(&b, 0), 1);
        assert_eq!(le32(&b, 4), 104);
        assert_eq!(le32(&b, 8), 36);
        assert_eq!(le32(&b, 12), 60);
        // Two messages in one transfer, then zero padding.
        let mut t = b.clone();
        t.extend(build_packet(&frame[..20], 512));
        t.extend([0u8; 8]);
        let mut got = Vec::new();
        assert!(parse_packets(&t, |f| got.push(f.len())));
        assert_eq!(got, vec![60, 20]);
    }

    #[test]
    fn packet_padding_and_malformed_input() {
        // 44 + 468 = 512: one trailing byte outside the message.
        let b = build_packet(&[0u8; 468], 512);
        assert_eq!(b.len(), 513);
        assert_eq!(le32(&b, 4), 512);
        let mut n = 0;
        assert!(parse_packets(&b[..512], |_| n += 1));
        assert_eq!(n, 1);

        let mut bad = build_packet(&[1u8; 60], 0);
        bad[12..16].copy_from_slice(&1000u32.to_le_bytes()); // data past the message
        assert!(!parse_packets(&bad, |_| panic!("no frame expected")));
        assert!(!parse_packets(&[7, 0, 0, 0, 44, 0, 0, 0], |_| {}));
        let mut short = build_packet(&[1u8; 60], 0);
        short[4..8].copy_from_slice(&500u32.to_le_bytes());
        assert!(!parse_packets(&short, |_| {}));
    }

    #[test]
    fn indications() {
        let mut m = Vec::new();
        put32(&mut m, MSG_INDICATE);
        put32(&mut m, 20);
        put32(&mut m, 0x4001_000b);
        assert_eq!(indicated_link(&m), Some(true));
        m[8..12].copy_from_slice(&0x4001_000cu32.to_le_bytes());
        assert_eq!(indicated_link(&m), Some(false));
        assert_eq!(indicated_link(&m[..8]), None);
    }
}
