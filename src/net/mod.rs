//! USB Ethernet adapters: CDC-ECM, CDC-NCM and RNDIS devices (phones sharing
//! their connection, single-board computers in gadget mode, docks, LTE
//! modems, many USB NICs' standard configurations) as one [`NetDevice`] that
//! sends and receives Ethernet frames.
//!
//! This module is behind the `net` cargo feature. The `pktkit` feature adds
//! an implementation of [`pktkit::L2Device`](https://docs.rs/pktkit) for
//! [`NetDevice`], so a USB adapter can be wired straight into a pktkit hub,
//! NAT or virtual TCP/IP stack.
//!
//! ```no_run
//! use rawusb::net::NetDevice;
//! use std::time::Duration;
//!
//! let ctx = rawusb::Context::new()?;
//! let dev = ctx.find_device(0x18d1, 0x4ee3)?.expect("phone not plugged in");
//! let nic = NetDevice::open(&dev)?;
//! println!("{:?} {:02x?}", nic.kind(), nic.mac_address());
//! let frame = nic.recv(Duration::from_secs(5))?;
//! println!("received {} bytes", frame.len());
//! # Ok::<(), rawusb::Error>(())
//! ```
//!
//! # Receiving and sending
//!
//! Frames are received continuously in the background. Without a handler
//! they queue up (up to [`RX_QUEUE`] frames, older ones kept) for
//! [`NetDevice::recv`]; with one set by
//! [`set_receive_handler`](NetDevice::set_receive_handler) each frame is
//! passed to it as it arrives, on the context's event thread, borrowed
//! straight from the transfer buffer. Handlers must not block.
//!
//! [`NetDevice::send`] never blocks either: it queues the frame and returns,
//! failing with [`ErrorKind::Busy`] when [`TX_QUEUE`] frames are already in
//! flight. So a handler may forward frames to another `NetDevice`.
//!
//! # Platform notes
//!
//! On Linux the kernel network driver (`cdc_ether`, `cdc_ncm`,
//! `rndis_host`) is detached while the device is open, which removes its
//! network interface. On macOS and Windows the device must be bound to a
//! generic driver (WinUSB on Windows).

mod cdc;
#[cfg(feature = "pktkit")]
mod pktkit_impl;
mod rndis;

use crate::class::{self, Claim, Repeating};
use crate::descriptors::InterfaceDescriptor;
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::transfer::Transfer;
use crate::types::{ControlType, Direction, Recipient, TransferStatus, TransferType, request_type};
use crate::{Error, ErrorKind, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Frames queued for [`NetDevice::recv`] before newer ones are dropped.
pub const RX_QUEUE: usize = 256;
/// Frames that may be in flight to the device at once.
pub const TX_QUEUE: usize = 32;
/// Receive transfers kept queued on the bulk IN endpoint.
const RX_TRANSFERS: usize = 8;
/// Largest NCM input block we ask for, and the RNDIS transfer size we
/// announce. Linux uses the same order of magnitude.
const RX_BLOCK: u32 = 32 * 1024;
/// Stop receiving after this many consecutive failed transfers, rather than
/// spinning on a device that went bad.
const MAX_RX_ERRORS: u32 = 64;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest Ethernet frame (without FCS) when the device does not say.
const DEFAULT_MAX_FRAME: usize = 1514;

/// The protocol a [`NetDevice`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetKind {
    /// CDC Ethernet Control Model: one frame per transfer.
    Ecm,
    /// CDC Network Control Model: frames batched in transfer blocks.
    Ncm,
    /// Microsoft Remote NDIS.
    Rndis,
}

/// A network function a device offers, as listed by [`interfaces`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetInterface {
    /// The control (communications) interface; pass it to
    /// [`NetDevice::open_interface`].
    pub interface: u8,
    /// The protocol.
    pub kind: NetKind,
}

fn kind_of(a: &InterfaceDescriptor) -> Option<NetKind> {
    use crate::types::class::{COMM, MISCELLANEOUS, WIRELESS};
    match (a.class, a.sub_class, a.protocol) {
        (COMM, cdc::SUBCLASS_ECM, _) => Some(NetKind::Ecm),
        (COMM, cdc::SUBCLASS_NCM, _) => Some(NetKind::Ncm),
        // RNDIS shows up under three different class triples.
        (WIRELESS, 0x01, 0x03) | (MISCELLANEOUS, 0x04, 0x01) | (COMM, 0x02, 0xff) => Some(NetKind::Rndis),
        _ => None,
    }
}

/// The network functions of a device's active configuration. Devices that
/// offer several protocols usually do so in separate configurations; select
/// the one you want with [`DeviceHandle::set_configuration`] first.
pub fn interfaces(device: &Device) -> Result<Vec<NetInterface>> {
    Ok(class::interfaces_where(device, |a| kind_of(a).is_some())?
        .iter()
        .filter_map(|a| {
            Some(NetInterface {
                interface: a.number,
                kind: kind_of(a)?,
            })
        })
        .collect())
}

/// Link state, as last reported by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LinkState {
    /// Whether the link is up; `None` until the device says.
    pub connected: Option<bool>,
    /// Receive (device to host) bit rate, if reported.
    pub down_bps: Option<u64>,
    /// Transmit bit rate, if reported.
    pub up_bps: Option<u64>,
}

/// Traffic counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NetStats {
    /// Frames received.
    pub rx_frames: u64,
    /// Bytes received (Ethernet frames, no USB framing).
    pub rx_bytes: u64,
    /// Frames sent.
    pub tx_frames: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Received frames dropped because the receive queue was full.
    pub rx_dropped: u64,
    /// Frames not sent: queue full, too large, or a failed transfer.
    pub tx_dropped: u64,
    /// Malformed transfers and failed USB transfers.
    pub errors: u64,
}

#[derive(Default)]
struct Counters {
    rx_frames: AtomicU64,
    rx_bytes: AtomicU64,
    tx_frames: AtomicU64,
    tx_bytes: AtomicU64,
    rx_dropped: AtomicU64,
    tx_dropped: AtomicU64,
    errors: AtomicU64,
    /// pktkit's counters, kept in step so `L2Device::stats` can lend them.
    #[cfg(feature = "pktkit")]
    pktkit: pktkit::DeviceStats,
}

impl Counters {
    fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    fn rx(&self, bytes: usize) {
        Self::add(&self.rx_frames, 1);
        Self::add(&self.rx_bytes, bytes as u64);
        #[cfg(feature = "pktkit")]
        self.pktkit.record_rx(bytes);
    }

    fn tx(&self, bytes: usize) {
        Self::add(&self.tx_frames, 1);
        Self::add(&self.tx_bytes, bytes as u64);
        #[cfg(feature = "pktkit")]
        self.pktkit.record_tx(bytes);
    }

    fn rx_drop(&self) {
        Self::add(&self.rx_dropped, 1);
        #[cfg(feature = "pktkit")]
        self.pktkit.record_rx_drop();
    }

    fn tx_drop(&self) {
        Self::add(&self.tx_dropped, 1);
        #[cfg(feature = "pktkit")]
        self.pktkit.record_tx_drop();
    }

    fn error(&self) {
        Self::add(&self.errors, 1);
        #[cfg(feature = "pktkit")]
        self.pktkit.record_error();
    }

    fn snapshot(&self) -> NetStats {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        NetStats {
            rx_frames: g(&self.rx_frames),
            rx_bytes: g(&self.rx_bytes),
            tx_frames: g(&self.tx_frames),
            tx_bytes: g(&self.tx_bytes),
            rx_dropped: g(&self.rx_dropped),
            tx_dropped: g(&self.tx_dropped),
            errors: g(&self.errors),
        }
    }
}

type Handler = Arc<dyn Fn(&[u8]) + Send + Sync + 'static>;

/// Where received frames go.
struct Sink {
    handler: Option<Handler>,
    queue: VecDeque<Vec<u8>>,
    /// Set when receiving stopped for good (closed, unplugged, failing).
    ended: Option<ErrorKind>,
}

/// Everything the transfer callbacks share with the device.
struct Shared {
    sink: Mutex<Sink>,
    arrived: Condvar,
    link: Mutex<LinkState>,
    counters: Counters,
    tx_in_flight: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Hands one received frame to the handler or the queue.
    fn deliver(&self, frame: &[u8]) {
        if frame.len() < 14 {
            self.counters.error();
            return;
        }
        self.counters.rx(frame.len());
        let mut sink = lock(&self.sink);
        if let Some(h) = sink.handler.clone() {
            drop(sink);
            h(frame);
        } else if sink.queue.len() < RX_QUEUE {
            sink.queue.push_back(frame.to_vec());
            drop(sink);
            self.arrived.notify_one();
        } else {
            self.counters.rx_drop();
        }
    }

    fn end(&self, why: ErrorKind) {
        let mut sink = lock(&self.sink);
        if sink.ended.is_none() {
            sink.ended = Some(why);
        }
        drop(sink);
        self.arrived.notify_all();
    }
}

/// How frames are framed on the bulk endpoints.
enum Framing {
    Ecm,
    Ncm { params: cdc::NtbParameters, sequence: AtomicU16 },
    Rndis,
}

/// Protocol-specific control state.
enum ControlState {
    Cdc { interface: u8, filter_supported: bool },
    Rndis(rndis::Control),
}

/// An open USB network function. See the [module documentation](self).
pub struct NetDevice {
    kind: NetKind,
    mac: [u8; 6],
    max_frame: usize,
    ep_out: u8,
    out_packet: usize,
    framing: Framing,
    shared: Arc<Shared>,
    control: Mutex<ControlState>,
    /// Receive transfers and the notification listener; emptied by
    /// [`close`](NetDevice::close). Declared before `claim` so they stop
    /// before the interfaces are released.
    running: Mutex<Vec<Repeating>>,
    claim: Claim,
}

impl std::fmt::Debug for NetDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetDevice")
            .field("kind", &self.kind)
            .field("mac", &format_args!("{:02x?}", self.mac))
            .field("claim", &self.claim)
            .finish_non_exhaustive()
    }
}

impl NetDevice {
    /// Opens the first network function of a device (see [`interfaces`]).
    pub fn open(device: &Device) -> Result<NetDevice> {
        let first = interfaces(device)?.into_iter().next().ok_or_else(|| {
            Error::with_message(
                ErrorKind::NotFound,
                "no CDC-ECM, CDC-NCM or RNDIS function in the active configuration",
            )
        })?;
        Self::open_interface(device.open()?, first.interface)
    }

    /// Opens the network function whose control interface is given, and
    /// starts receiving.
    pub fn open_interface(handle: DeviceHandle, interface: u8) -> Result<NetDevice> {
        let cfg = handle.device().active_config_descriptor()?;
        let comm = class::interface(handle.device(), interface)?;
        let kind = kind_of(&comm).ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "not a network control interface"))?;
        let functional = cdc::Functional::parse(&comm);
        let data_number = functional
            .data_interface(&cfg, interface)
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "network function has no data interface"))?;
        let data = cfg
            .interface(data_number)
            .and_then(|i| {
                i.alt_settings.iter().find(|a| {
                    class::endpoint(a, Direction::In, TransferType::Bulk).is_some()
                        && class::endpoint(a, Direction::Out, TransferType::Bulk).is_some()
                })
            })
            .cloned()
            .ok_or_else(|| Error::with_message(ErrorKind::Io, "network data interface lacks bulk endpoints"))?;
        let bulk = |dir| class::endpoint(&data, dir, TransferType::Bulk).ok_or_else(|| Error::new(ErrorKind::Io));
        let (ep_in, in_packet) = bulk(Direction::In).map(|e| (e.address, e.packet_size().max(1) as usize))?;
        let (ep_out, out_packet) = bulk(Direction::Out).map(|e| (e.address, e.packet_size().max(1) as usize))?;
        let notify_ep = class::endpoint(&comm, Direction::In, TransferType::Interrupt).map(|e| (e.address, e.max_packet_size() as usize));

        let claim = Claim::new(handle, &[interface, data_number])?;
        let h = claim.handle().clone();
        let class_out = request_type(Direction::Out, ControlType::Class, Recipient::Interface);
        let class_in = request_type(Direction::In, ControlType::Class, Recipient::Interface);

        let mut link = LinkState::default();
        let (framing, control, mac, max_frame, rx_size) = match kind {
            NetKind::Ecm | NetKind::Ncm => {
                // Alternate setting 0 has no endpoints and resets the
                // function; NCM parameters may only change there.
                h.set_alternate_setting(data_number, 0)?;
                let mut framing = Framing::Ecm;
                let mut rx_size =
                    (functional.max_segment_size.max(DEFAULT_MAX_FRAME as u16) as usize).div_ceil(in_packet) * in_packet + in_packet;
                if kind == NetKind::Ncm {
                    let mut b = [0u8; 28];
                    let n = h.control_read(class_in, cdc::req::GET_NTB_PARAMETERS, 0, interface as u16, &mut b, CONTROL_TIMEOUT)?;
                    let params = cdc::NtbParameters::parse(&b[..n])
                        .ok_or_else(|| Error::with_message(ErrorKind::NotSupported, "NCM device without 16-bit transfer blocks"))?;
                    let mut in_size = params.in_max_size;
                    if in_size > RX_BLOCK {
                        // Devices with capability bit 5 take the 8-byte form,
                        // which adds a datagram limit (0: none) and padding.
                        let mut size = RX_BLOCK.to_le_bytes().to_vec();
                        if functional.ncm_capabilities.is_some_and(|c| c & 0x20 != 0) {
                            size.extend_from_slice(&[0; 4]);
                        }
                        h.control_write(class_out, cdc::req::SET_NTB_INPUT_SIZE, 0, interface as u16, &size, CONTROL_TIMEOUT)?;
                        in_size = RX_BLOCK;
                    }
                    rx_size = in_size as usize;
                    framing = Framing::Ncm {
                        params,
                        sequence: AtomicU16::new(0),
                    };
                }
                let mac = read_mac(&h, functional.mac_string)?;
                let filter_supported = kind == NetKind::Ecm || functional.ncm_capabilities.is_some_and(|c| c & 0x01 != 0);
                let state = ControlState::Cdc {
                    interface,
                    filter_supported,
                };
                let max_frame = if functional.max_segment_size >= 60 {
                    functional.max_segment_size as usize
                } else {
                    DEFAULT_MAX_FRAME
                };
                (framing, state, mac, max_frame, rx_size)
            }
            NetKind::Rndis => {
                let mut ctl = rndis::Control::new(h.clone(), interface, notify_ep.map(|e| e.0));
                let info = ctl.initialize(RX_BLOCK)?;
                let mac_bytes = ctl.query(rndis::oid::PERMANENT_ADDRESS, 6)?;
                let mac: [u8; 6] = mac_bytes
                    .get(..6)
                    .and_then(|m| m.try_into().ok())
                    .ok_or_else(|| Error::with_message(ErrorKind::Io, "RNDIS device returned no MAC address"))?;
                let max_frame = match ctl.query_u32(rndis::oid::GEN_MAXIMUM_FRAME_SIZE) {
                    Ok(payload) if (46..=9000).contains(&payload) => payload as usize + 14,
                    _ => DEFAULT_MAX_FRAME,
                }
                .min((info.max_transfer_size as usize).saturating_sub(rndis::PACKET_HEADER).max(60));
                if let Ok(status) = ctl.query_u32(rndis::oid::GEN_MEDIA_CONNECT_STATUS) {
                    link.connected = Some(status == 0);
                }
                // An indication seen while waiting for replies is newer.
                if ctl.link.is_some() {
                    link.connected = ctl.link;
                }
                if let Ok(speed) = ctl.query_u32(rndis::oid::GEN_LINK_SPEED) {
                    // Reported in units of 100 bit/s.
                    link.down_bps = Some(speed as u64 * 100);
                    link.up_bps = link.down_bps;
                }
                (Framing::Rndis, ControlState::Rndis(ctl), mac, max_frame, RX_BLOCK as usize)
            }
        };

        let shared = Arc::new(Shared {
            sink: Mutex::new(Sink {
                handler: None,
                queue: VecDeque::new(),
                ended: None,
            }),
            arrived: Condvar::new(),
            link: Mutex::new(link),
            counters: Counters::default(),
            tx_in_flight: AtomicUsize::new(0),
        });
        let nic = NetDevice {
            kind,
            mac,
            max_frame,
            ep_out,
            out_packet,
            framing,
            shared,
            control: Mutex::new(control),
            running: Mutex::new(Vec::new()),
            claim,
        };
        nic.set_filter(false)?;
        if kind != NetKind::Rndis {
            // Streaming starts on the alternate setting with endpoints.
            h.set_alternate_setting(data_number, data.alternate_setting)?;
            if let Some((ep, size)) = notify_ep {
                nic.start_notifications(ep, size)?;
            }
        }
        nic.start_receiving(ep_in, rx_size)?;
        Ok(nic)
    }

    fn start_notifications(&self, ep: u8, size: usize) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let t = Transfer::interrupt(self.handle(), ep, vec![0u8; size.max(16)]);
        let r = Repeating::start(t, move |t| {
            if t.status() != TransferStatus::Completed {
                return false;
            }
            if let Ok(data) = t.data() {
                let mut link = lock(&shared.link);
                match cdc::decode_notification(&data) {
                    Some(cdc::Notification::Connection(up)) => link.connected = Some(up),
                    Some(cdc::Notification::Speed { down, up }) => {
                        link.down_bps = Some(down as u64);
                        link.up_bps = Some(up as u64);
                    }
                    None => {}
                }
            }
            true
        })?;
        lock(&self.running).push(r);
        Ok(())
    }

    fn start_receiving(&self, ep: u8, size: usize) -> Result<()> {
        let mut running = lock(&self.running);
        for _ in 0..RX_TRANSFERS {
            let shared = Arc::clone(&self.shared);
            let kind = self.kind;
            let mut failures = 0u32;
            let t = Transfer::bulk(self.handle(), ep, vec![0u8; size]);
            let r = Repeating::start(t, move |t| {
                match t.status() {
                    TransferStatus::Completed => failures = 0,
                    TransferStatus::Cancelled => return false,
                    TransferStatus::NoDevice => {
                        shared.end(ErrorKind::NoDevice);
                        return false;
                    }
                    _ => {
                        shared.counters.error();
                        failures += 1;
                        if failures >= MAX_RX_ERRORS {
                            shared.end(ErrorKind::Io);
                            return false;
                        }
                        return true;
                    }
                }
                let Ok(data) = t.data() else { return true };
                let ok = match kind {
                    NetKind::Ecm => {
                        if !data.is_empty() {
                            shared.deliver(&data);
                        }
                        true
                    }
                    NetKind::Ncm => data.is_empty() || cdc::parse_ntb16(&data, |f| shared.deliver(f)),
                    NetKind::Rndis => rndis::parse_packets(&data, |f| shared.deliver(f)),
                };
                if !ok {
                    shared.counters.error();
                }
                true
            })?;
            running.push(r);
        }
        Ok(())
    }

    /// The underlying device handle.
    pub fn handle(&self) -> &DeviceHandle {
        self.claim.handle()
    }

    /// The protocol.
    pub fn kind(&self) -> NetKind {
        self.kind
    }

    /// The adapter's MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Largest Ethernet frame (header included, FCS excluded) the device
    /// takes; 1514 for standard Ethernet.
    pub fn max_frame_size(&self) -> usize {
        self.max_frame
    }

    /// The link state as last reported. ECM and NCM devices report changes
    /// on their notification endpoint; RNDIS devices are asked at open.
    pub fn link(&self) -> LinkState {
        *lock(&self.shared.link)
    }

    /// Traffic counters.
    pub fn stats(&self) -> NetStats {
        self.shared.counters.snapshot()
    }

    /// Queues one Ethernet frame (destination MAC first, no FCS) for
    /// transmission and returns without waiting. Fails with
    /// [`ErrorKind::Busy`] when [`TX_QUEUE`] frames are already in flight,
    /// and with [`ErrorKind::InvalidParam`] for frames shorter than an
    /// Ethernet header or longer than [`max_frame_size`](Self::max_frame_size).
    pub fn send(&self, frame: &[u8]) -> Result<()> {
        let counters = &self.shared.counters;
        if frame.len() < 14 || frame.len() > self.max_frame {
            counters.tx_drop();
            return Err(Error::with_message(ErrorKind::InvalidParam, "frame size out of range"));
        }
        if lock(&self.shared.sink).ended.is_some() {
            counters.tx_drop();
            return Err(Error::with_message(ErrorKind::NoDevice, "network device is closed"));
        }
        let wire = match &self.framing {
            Framing::Ecm => {
                // Pad exact packet multiples by a byte instead of relying on a
                // zero-length packet, which not every backend can send.
                let mut w = frame.to_vec();
                if w.len().is_multiple_of(self.out_packet) {
                    w.push(0);
                }
                w
            }
            Framing::Ncm { params, sequence } => cdc::build_ntb16(frame, sequence.fetch_add(1, Ordering::Relaxed), params, self.out_packet)
                .inspect_err(|_| counters.tx_drop())?,
            Framing::Rndis => rndis::build_packet(frame, self.out_packet),
        };
        if self.shared.tx_in_flight.fetch_add(1, Ordering::AcqRel) >= TX_QUEUE {
            self.shared.tx_in_flight.fetch_sub(1, Ordering::AcqRel);
            counters.tx_drop();
            return Err(Error::with_message(ErrorKind::Busy, "transmit queue full"));
        }
        let t = Transfer::bulk(self.handle(), self.ep_out, wire);
        let shared = Arc::clone(&self.shared);
        let len = frame.len();
        let submitted = t
            .set_callback(move |t| {
                shared.tx_in_flight.fetch_sub(1, Ordering::AcqRel);
                if t.status() == TransferStatus::Completed {
                    shared.counters.tx(len);
                } else {
                    shared.counters.tx_drop();
                    shared.counters.error();
                }
            })
            .and_then(|()| t.submit());
        if let Err(e) = submitted {
            self.shared.tx_in_flight.fetch_sub(1, Ordering::AcqRel);
            counters.tx_drop();
            return Err(e);
        }
        Ok(())
    }

    /// Delivers every received frame to `handler`, on the context's event
    /// thread, instead of queueing it for [`recv`](Self::recv). The slice is
    /// only valid during the call. The handler must return quickly and must
    /// not block on USB I/O ([`send`](Self::send) is fine).
    pub fn set_receive_handler(&self, handler: impl Fn(&[u8]) + Send + Sync + 'static) {
        lock(&self.shared.sink).handler = Some(Arc::new(handler));
    }

    /// Goes back to queueing received frames for [`recv`](Self::recv).
    pub fn clear_receive_handler(&self) {
        lock(&self.shared.sink).handler = None;
    }

    /// Takes the next received frame, waiting up to `timeout`
    /// ([`NO_TIMEOUT`](crate::NO_TIMEOUT) waits forever). Fails with
    /// [`ErrorKind::Timeout`], or with [`ErrorKind::NoDevice`] once the
    /// device is closed or gone and the queue is empty.
    pub fn recv(&self, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = (timeout != crate::NO_TIMEOUT).then(|| Instant::now() + timeout);
        let mut sink = lock(&self.shared.sink);
        loop {
            if let Some(f) = sink.queue.pop_front() {
                return Ok(f);
            }
            if let Some(why) = sink.ended {
                return Err(Error::with_message(why, "network device stopped receiving"));
            }
            sink = match deadline {
                None => self.shared.arrived.wait(sink).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Error::new(ErrorKind::Timeout));
                    }
                    self.shared.arrived.wait_timeout(sink, left).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
    }

    /// Enables or disables promiscuous reception (frames for any MAC).
    /// Broadcast, multicast and frames for this adapter are always received.
    pub fn set_promiscuous(&self, on: bool) -> Result<()> {
        self.set_filter(on)
    }

    fn set_filter(&self, promiscuous: bool) -> Result<()> {
        let mut control = lock(&self.control);
        match &mut *control {
            ControlState::Cdc {
                interface,
                filter_supported,
            } => {
                if !*filter_supported {
                    return if promiscuous {
                        Err(Error::with_message(ErrorKind::NotSupported, "device has no packet filter request"))
                    } else {
                        Ok(())
                    };
                }
                let mut bits = cdc::filter::DIRECTED | cdc::filter::BROADCAST | cdc::filter::ALL_MULTICAST;
                if promiscuous {
                    bits |= cdc::filter::PROMISCUOUS;
                }
                let r = self.handle().control_write(
                    request_type(Direction::Out, ControlType::Class, Recipient::Interface),
                    cdc::req::SET_ETHERNET_PACKET_FILTER,
                    bits,
                    *interface as u16,
                    &[],
                    CONTROL_TIMEOUT,
                );
                match r {
                    // Plenty of ECM devices stall this and filter nothing.
                    Err(e) if e.is_stall() && !promiscuous => Ok(()),
                    r => r.map(drop),
                }
            }
            ControlState::Rndis(ctl) => {
                let mut bits = rndis::filter::DIRECTED | rndis::filter::BROADCAST | rndis::filter::ALL_MULTICAST;
                if promiscuous {
                    bits |= rndis::filter::PROMISCUOUS;
                }
                ctl.set(rndis::oid::GEN_CURRENT_PACKET_FILTER, &bits.to_le_bytes())
            }
        }
    }

    /// Stops receiving and, for RNDIS, tells the device the host is gone.
    /// Frames already queued can still be taken with [`recv`](Self::recv).
    /// Idempotent; dropping the device does the same.
    pub fn close(&self) -> Result<()> {
        let running = std::mem::take(&mut *lock(&self.running));
        let was_running = !running.is_empty();
        drop(running);
        self.shared.end(ErrorKind::NoDevice);
        if was_running && let ControlState::Rndis(ctl) = &mut *lock(&self.control) {
            // Clear the filter so the device stops sending, then halt.
            let _ = ctl.set(rndis::oid::GEN_CURRENT_PACKET_FILTER, &0u32.to_le_bytes());
            let _ = ctl.halt();
        }
        Ok(())
    }
}

impl Drop for NetDevice {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Reads an ECM/NCM MAC address from its string descriptor.
fn read_mac(h: &DeviceHandle, index: u8) -> Result<[u8; 6]> {
    if index == 0 {
        return Err(Error::with_message(ErrorKind::Io, "network function declares no MAC address"));
    }
    let s = h.read_string(index)?;
    cdc::parse_mac(&s).ok_or_else(|| Error::with_message(ErrorKind::Io, format!("malformed MAC address string {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(class: u8, sub_class: u8, protocol: u8) -> InterfaceDescriptor {
        InterfaceDescriptor {
            number: 0,
            alternate_setting: 0,
            num_endpoints: 1,
            class,
            sub_class,
            protocol,
            string_index: 0,
            endpoints: Vec::new(),
            extra: Vec::new(),
        }
    }

    #[test]
    fn protocol_detection() {
        assert_eq!(kind_of(&iface(0x02, 0x06, 0x00)), Some(NetKind::Ecm));
        assert_eq!(kind_of(&iface(0x02, 0x0d, 0x00)), Some(NetKind::Ncm));
        assert_eq!(kind_of(&iface(0xe0, 0x01, 0x03)), Some(NetKind::Rndis));
        assert_eq!(kind_of(&iface(0xef, 0x04, 0x01)), Some(NetKind::Rndis));
        assert_eq!(kind_of(&iface(0x02, 0x02, 0xff)), Some(NetKind::Rndis));
        assert_eq!(kind_of(&iface(0x02, 0x02, 0x01)), None, "plain CDC-ACM is a serial port");
        assert_eq!(kind_of(&iface(0x0a, 0x00, 0x00)), None);
    }

    #[test]
    fn delivery_goes_to_handler_or_bounded_queue() {
        let shared = Shared {
            sink: Mutex::new(Sink {
                handler: None,
                queue: VecDeque::new(),
                ended: None,
            }),
            arrived: Condvar::new(),
            link: Mutex::new(LinkState::default()),
            counters: Counters::default(),
            tx_in_flight: AtomicUsize::new(0),
        };
        shared.deliver(&[0u8; 10]); // runt: counted as an error
        for _ in 0..RX_QUEUE + 3 {
            shared.deliver(&[1u8; 60]);
        }
        let s = shared.counters.snapshot();
        assert_eq!((s.errors, s.rx_frames, s.rx_dropped), (1, RX_QUEUE as u64 + 3, 3));
        assert_eq!(lock(&shared.sink).queue.len(), RX_QUEUE);

        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        lock(&shared.sink).handler = Some(Arc::new(move |f: &[u8]| {
            counter.fetch_add(f.len(), Ordering::Relaxed);
        }));
        shared.deliver(&[2u8; 64]);
        assert_eq!(seen.load(Ordering::Relaxed), 64);
        assert_eq!(lock(&shared.sink).queue.len(), RX_QUEUE, "handler frames bypass the queue");
    }
}
