//! Video streaming: probe/commit parameters, payload parsing and frame
//! reassembly, and the transfer loop that feeds them.

use crate::class::le16;
use crate::transfer::Transfer;
use crate::types::{TransferStatus, TransferType};
use crate::{Error, ErrorKind, Result};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Transfers kept in flight while streaming.
const TRANSFERS: usize = 5;
/// Completed frames buffered for the reader before newer ones are dropped.
const FRAME_QUEUE: usize = 4;

fn le32(b: &[u8], at: usize) -> u32 {
    le16(b, at) as u32 | (le16(b, at + 2) as u32) << 16
}

/// The video probe and commit control block (UVC 1.5 §4.3.1.1): the
/// parameters host and device agree on before streaming.
///
/// Its wire size depends on the UVC version: 26 bytes for 1.0, 34 for 1.1,
/// 48 for 1.5. Fields past the negotiated size are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamControl {
    /// `bmHint`: which fields the device should keep fixed (bit 0 = frame
    /// interval).
    pub hint: u16,
    /// `bFormatIndex`.
    pub format_index: u8,
    /// `bFrameIndex`.
    pub frame_index: u8,
    /// `dwFrameInterval`, 100 ns units.
    pub frame_interval: u32,
    /// `wKeyFrameRate`.
    pub key_frame_rate: u16,
    /// `wPFrameRate`.
    pub p_frame_rate: u16,
    /// `wCompQuality`.
    pub comp_quality: u16,
    /// `wCompWindowSize`.
    pub comp_window_size: u16,
    /// `wDelay`, milliseconds.
    pub delay: u16,
    /// `dwMaxVideoFrameSize`: the largest frame the device will send.
    pub max_video_frame_size: u32,
    /// `dwMaxPayloadTransferSize`: the largest payload per (micro)frame for
    /// isochronous endpoints, per transfer for bulk ones.
    pub max_payload_transfer_size: u32,
    /// `dwClockFrequency` (UVC 1.1+).
    pub clock_frequency: u32,
    /// `bmFramingInfo` (UVC 1.1+).
    pub framing_info: u8,
    /// `bPreferedVersion` (UVC 1.1+).
    pub preferred_version: u8,
    /// `bMinVersion` (UVC 1.1+).
    pub min_version: u8,
    /// `bMaxVersion` (UVC 1.1+).
    pub max_version: u8,
    /// The UVC 1.5 tail (usage, bit depth, settings, reference frames, rate
    /// control modes, layout), kept verbatim.
    pub uvc15_tail: [u8; 14],
}

impl StreamControl {
    /// The control block size for a `bcdUVC` version, as Linux computes it.
    pub const fn wire_size(uvc_version: u16) -> usize {
        if uvc_version < 0x0110 {
            26
        } else if uvc_version < 0x0150 {
            34
        } else {
            48
        }
    }

    /// A request for a format, frame and interval, with the interval fixed.
    pub fn new(format_index: u8, frame_index: u8, frame_interval: u32) -> StreamControl {
        let mut c = StreamControl::from_bytes(&[]);
        c.hint = 1;
        c.format_index = format_index;
        c.frame_index = frame_index;
        c.frame_interval = frame_interval;
        c
    }

    /// Decodes a control block; missing trailing bytes read as zero.
    pub fn from_bytes(b: &[u8]) -> StreamControl {
        let byte = |i: usize| b.get(i).copied().unwrap_or(0);
        let mut uvc15_tail = [0u8; 14];
        for (i, v) in uvc15_tail.iter_mut().enumerate() {
            *v = byte(34 + i);
        }
        StreamControl {
            hint: le16(b, 0),
            format_index: byte(2),
            frame_index: byte(3),
            frame_interval: le32(b, 4),
            key_frame_rate: le16(b, 8),
            p_frame_rate: le16(b, 10),
            comp_quality: le16(b, 12),
            comp_window_size: le16(b, 14),
            delay: le16(b, 16),
            max_video_frame_size: le32(b, 18),
            max_payload_transfer_size: le32(b, 22),
            clock_frequency: le32(b, 26),
            framing_info: byte(30),
            preferred_version: byte(31),
            min_version: byte(32),
            max_version: byte(33),
            uvc15_tail,
        }
    }

    /// Encodes the full 48-byte block; send the first
    /// [`wire_size`](Self::wire_size) bytes.
    pub fn to_bytes(&self) -> [u8; 48] {
        let mut b = [0u8; 48];
        b[0..2].copy_from_slice(&self.hint.to_le_bytes());
        b[2] = self.format_index;
        b[3] = self.frame_index;
        b[4..8].copy_from_slice(&self.frame_interval.to_le_bytes());
        b[8..10].copy_from_slice(&self.key_frame_rate.to_le_bytes());
        b[10..12].copy_from_slice(&self.p_frame_rate.to_le_bytes());
        b[12..14].copy_from_slice(&self.comp_quality.to_le_bytes());
        b[14..16].copy_from_slice(&self.comp_window_size.to_le_bytes());
        b[16..18].copy_from_slice(&self.delay.to_le_bytes());
        b[18..22].copy_from_slice(&self.max_video_frame_size.to_le_bytes());
        b[22..26].copy_from_slice(&self.max_payload_transfer_size.to_le_bytes());
        b[26..30].copy_from_slice(&self.clock_frequency.to_le_bytes());
        b[30] = self.framing_info;
        b[31] = self.preferred_version;
        b[32] = self.min_version;
        b[33] = self.max_version;
        b[34..48].copy_from_slice(&self.uvc15_tail);
        b
    }
}

/// A complete video frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The frame's bytes: raw pixels, a JPEG image, or a compressed access
    /// unit, depending on the format.
    pub data: Vec<u8>,
    /// Counts frames delivered by this stream, starting at 0. A gap means
    /// frames were dropped because they were not read in time.
    pub sequence: u64,
    /// Presentation timestamp from the payload header, in device clock
    /// ticks ([`ControlInterface::clock_frequency`](super::ControlInterface::clock_frequency)).
    pub pts: Option<u32>,
    /// The device flagged an error in one of the frame's payloads, or a
    /// packet was lost: the data may be corrupt.
    pub error: bool,
}

/// Rebuilds frames from payloads (UVC 1.5 §2.4.3.3), the way Linux does:
/// every payload starts with a header whose FID bit toggles between frames,
/// and whose EOF bit may mark a frame's last payload. After an EOF, payloads
/// with the same FID are ignored until it toggles. A frame whose start was
/// not seen (the one in progress when streaming began) is dropped.
pub(crate) struct Assembler {
    /// The frame being built, and whether its first payload was seen.
    current: Option<(Frame, bool)>,
    last_fid: Option<u8>,
    /// The next frame to start will be complete from its first payload.
    at_boundary: bool,
    /// An EOF closed the last frame; wait for the FID to toggle.
    after_eof: bool,
    sequence: u64,
    max_frame_size: usize,
}

impl Assembler {
    pub(crate) fn new(max_frame_size: usize) -> Assembler {
        Assembler {
            current: None,
            last_fid: None,
            at_boundary: false,
            after_eof: false,
            sequence: 0,
            max_frame_size,
        }
    }

    /// Marks the frame being built as damaged (a lost packet).
    pub(crate) fn lost(&mut self) {
        if let Some((f, _)) = &mut self.current {
            f.error = true;
        }
    }

    /// Feeds one payload, emitting any frame it completes.
    pub(crate) fn payload(&mut self, p: &[u8], mut emit: impl FnMut(Frame)) {
        if p.len() < 2 {
            return; // empty packet: nothing was sent this interval
        }
        let hlen = p[0] as usize;
        let flags = p[1];
        if hlen < 2 || hlen > p.len() {
            self.lost();
            return;
        }
        let fid = flags & 0x01;
        let eof = flags & 0x02 != 0;
        let pts = (flags & 0x04 != 0 && hlen >= 6).then(|| le32(p, 2));
        let err = flags & 0x40 != 0;
        let data = &p[hlen..];

        if self.last_fid.is_some_and(|last| last != fid) {
            self.finish(&mut emit);
            self.at_boundary = true;
            self.after_eof = false;
        }
        self.last_fid = Some(fid);
        if self.after_eof {
            return;
        }
        let at_boundary = self.at_boundary;
        let capacity = self.max_frame_size;
        let (frame, _) = self.current.get_or_insert_with(|| {
            (
                Frame {
                    data: Vec::with_capacity(capacity),
                    sequence: 0,
                    pts: None,
                    error: false,
                },
                at_boundary,
            )
        });
        frame.error |= err;
        frame.pts = frame.pts.or(pts);
        if frame.data.len() + data.len() > capacity.max(1) * 2 {
            frame.error = true; // runaway frame: stop growing it
        } else {
            frame.data.extend_from_slice(data);
        }
        if eof {
            self.finish(&mut emit);
            self.at_boundary = true;
            self.after_eof = true;
        }
    }

    fn finish(&mut self, emit: &mut impl FnMut(Frame)) {
        if let Some((mut frame, whole)) = self.current.take()
            && whole
            && !frame.data.is_empty()
        {
            frame.sequence = self.sequence;
            self.sequence += 1;
            emit(frame);
        }
    }
}

/// Everything the transfer callbacks share.
struct Shared {
    assembler: Assembler,
    frames: SyncSender<Frame>,
    dropped: u64,
    stopped: bool,
    failure: Option<TransferStatus>,
}

/// The running transfer loop behind a [`Stream`](super::Stream).
pub(crate) struct Pump {
    transfers: Vec<Transfer>,
    shared: Arc<Mutex<Shared>>,
    /// Behind a mutex so a `Stream` can be shared between threads.
    frames: Mutex<Receiver<Frame>>,
}

fn lock(m: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Pump {
    /// Allocates and submits the transfers. For isochronous endpoints each
    /// transfer carries `packets` packets of `packet_size` bytes; for bulk
    /// ones each carries one payload of `packet_size` bytes.
    pub(crate) fn start(
        handle: &crate::DeviceHandle,
        kind: TransferType,
        endpoint: u8,
        packet_size: usize,
        packets: usize,
        max_frame_size: usize,
    ) -> Result<Pump> {
        let (tx, rx) = std::sync::mpsc::sync_channel(FRAME_QUEUE);
        let shared = Arc::new(Mutex::new(Shared {
            assembler: Assembler::new(max_frame_size),
            frames: tx,
            dropped: 0,
            stopped: false,
            failure: None,
        }));
        let mut transfers = Vec::with_capacity(TRANSFERS);
        for _ in 0..TRANSFERS {
            let t = match kind {
                TransferType::Isochronous => Transfer::isochronous(handle, endpoint, packet_size, packets),
                _ => Transfer::bulk(handle, endpoint, vec![0u8; packet_size]),
            };
            let s = Arc::clone(&shared);
            t.set_callback(move |t| on_complete(t, &s, packet_size))?;
            transfers.push(t);
        }
        let pump = Pump {
            transfers,
            shared,
            frames: Mutex::new(rx),
        };
        for t in &pump.transfers {
            t.submit()?;
        }
        Ok(pump)
    }

    pub(crate) fn dropped(&self) -> u64 {
        lock(&self.shared).dropped
    }

    /// Waits for the next frame, reporting why streaming ended if it did.
    pub(crate) fn next(&self, timeout: Duration) -> Result<Frame> {
        let frames = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        match frames.recv_timeout(timeout) {
            Ok(f) => Ok(f),
            Err(RecvTimeoutError::Timeout) => match lock(&self.shared).failure {
                Some(s) => Err(stream_error(s)),
                None => Err(Error::new(ErrorKind::Timeout)),
            },
            Err(RecvTimeoutError::Disconnected) => Err(Error::new(ErrorKind::Interrupted)),
        }
    }

    /// Stops resubmitting and cancels everything in flight.
    pub(crate) fn stop(&self) {
        lock(&self.shared).stopped = true;
        for t in &self.transfers {
            let _ = t.cancel();
        }
        for t in &self.transfers {
            let _ = t.wait(Some(Duration::from_secs(1)));
        }
    }
}

fn stream_error(status: TransferStatus) -> Error {
    let e = status.into_result().err().unwrap_or_else(|| Error::new(ErrorKind::Io));
    e.context("video stream stopped")
}

fn on_complete(t: &Transfer, shared: &Mutex<Shared>, packet_size: usize) {
    let status = t.status();
    let mut s = lock(shared);
    match status {
        TransferStatus::Completed => {}
        TransferStatus::Cancelled => return,
        // A transient error on one transfer loses data but not the stream.
        TransferStatus::Error | TransferStatus::Overflow | TransferStatus::TimedOut => s.assembler.lost(),
        other => {
            s.failure = Some(other);
            return;
        }
    }
    if status == TransferStatus::Completed {
        let Shared {
            assembler,
            frames,
            dropped,
            ..
        } = &mut *s;
        let mut emit = |f: Frame| {
            if let Err(TrySendError::Full(_)) = frames.try_send(f) {
                *dropped += 1;
            }
        };
        if t.kind() == TransferType::Isochronous {
            if let Ok(buf) = t.buffer() {
                for (i, p) in t.iso_packets().iter().enumerate() {
                    if p.status != TransferStatus::Completed {
                        assembler.lost();
                        continue;
                    }
                    let start = i * packet_size;
                    let end = (start + p.actual_length as usize).min(buf.len());
                    assembler.payload(&buf[start.min(end)..end], &mut emit);
                }
            }
        } else if let Ok(data) = t.data() {
            assembler.payload(&data, &mut emit);
        }
    }
    if !s.stopped
        && let Err(e) = t.submit()
    {
        s.failure = Some(if e.is_no_device() {
            TransferStatus::NoDevice
        } else {
            TransferStatus::Error
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(flags: u8, data: &[u8]) -> Vec<u8> {
        let mut p = vec![2, flags];
        p.extend_from_slice(data);
        p
    }

    fn feed(a: &mut Assembler, p: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        a.payload(p, |f| out.push(f));
        out
    }

    #[test]
    fn control_block_roundtrip() {
        let c = StreamControl::new(2, 1, 333_333);
        let b = c.to_bytes();
        assert_eq!(&b[..8], &[1, 0, 2, 1, 0x15, 0x16, 0x05, 0x00]);
        assert_eq!(StreamControl::from_bytes(&b), c);
        // A 26-byte (UVC 1.0) reply leaves the 1.1 fields at zero.
        let short = StreamControl::from_bytes(&b[..26]);
        assert_eq!(short.frame_interval, 333_333);
        assert_eq!(short.clock_frequency, 0);
        assert_eq!(StreamControl::wire_size(0x0100), 26);
        assert_eq!(StreamControl::wire_size(0x0110), 34);
        assert_eq!(StreamControl::wire_size(0x0150), 48);
    }

    #[test]
    fn frames_split_on_fid_toggle() {
        let mut a = Assembler::new(16);
        // Joined mid-frame: the partial frame (FID 0) is discarded.
        assert!(feed(&mut a, &payload(0, b"tail")).is_empty());
        assert!(feed(&mut a, &payload(1, b"ab")).is_empty());
        assert!(feed(&mut a, &[2, 1]).is_empty(), "header-only payload");
        assert!(feed(&mut a, &payload(1, b"cd")).is_empty());
        let out = feed(&mut a, &payload(0, b"ef"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, b"abcd");
        assert_eq!(out[0].sequence, 0);
        let out = feed(&mut a, &payload(1, b"gh"));
        assert_eq!(out[0].data, b"ef");
        assert_eq!(out[0].sequence, 1);
    }

    #[test]
    fn frames_end_on_eof_and_carry_pts_and_errors() {
        let mut a = Assembler::new(16);
        // An EOF establishes sync without emitting the partial frame.
        assert!(feed(&mut a, &payload(0x02, b"xx")).is_empty());
        // A stray empty payload with the same FID after EOF is ignored.
        assert!(feed(&mut a, &[2, 0]).is_empty());
        let mut p = vec![6, 0x05];
        p.extend(1234u32.to_le_bytes());
        p.extend(b"12");
        assert!(feed(&mut a, &p).is_empty());
        let out = feed(&mut a, &payload(0x43, b"34"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, b"1234");
        assert_eq!(out[0].pts, Some(1234));
        assert!(out[0].error);
        // Next frame (FID 0) ends with EOF too.
        assert!(feed(&mut a, &payload(0, b"5")).is_empty());
        let out = feed(&mut a, &payload(0x02, b"6"));
        assert_eq!(out[0].data, b"56");
        assert!(!out[0].error);
        // A lost packet marks the frame in progress.
        assert!(feed(&mut a, &payload(1, b"7")).is_empty());
        a.lost();
        let out = feed(&mut a, &payload(3, b"8"));
        assert!(out[0].error);
        // Garbage headers are rejected without panicking.
        assert!(feed(&mut a, &[9, 0, 1]).is_empty());
        assert!(feed(&mut a, &[1]).is_empty());
    }
}
