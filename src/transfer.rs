//! Asynchronous transfers: the low-level building block every I/O operation
//! in this crate is made of.
//!
//! A [`Transfer`] is allocated once, configured, and then submitted as many
//! times as needed. While it is in flight its buffer belongs to the operating
//! system; once it completes (successfully or not) its status, actual length
//! and buffer are readable again and it may be resubmitted.
//!
//! Completion can be observed in four ways, which may be combined:
//!
//! - blocking with [`Transfer::wait`];
//! - polling with [`Transfer::is_pending`];
//! - a callback set with [`Transfer::set_callback`], run on the context's
//!   event thread;
//! - awaiting [`Transfer::completion`] from any async runtime.

use crate::handle::{DeviceHandle, HandleShared};
use crate::sys;
use crate::types::{ControlSetup, IsoPacket, TransferStatus, TransferType};
use crate::{Error, ErrorKind, Result};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::Duration;

type Callback = Box<dyn FnMut(&Transfer) + Send + 'static>;

/// Options that change how the host controller driver treats a transfer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferFlags {
    /// Report a short read (fewer bytes than requested) as an error instead of
    /// a successful completion. Linux only; ignored elsewhere.
    pub short_not_ok: bool,
    /// For OUT transfers whose length is a multiple of the endpoint's packet
    /// size, send a trailing zero-length packet so the device sees the end of
    /// the transfer.
    pub zero_packet: bool,
}

pub(crate) struct State {
    pub(crate) in_flight: bool,
    pub(crate) buffer: Vec<u8>,
    pub(crate) timeout: Duration,
    pub(crate) iso_packets: Vec<IsoPacket>,
    pub(crate) flags: TransferFlags,
    pub(crate) status: TransferStatus,
    pub(crate) actual_length: usize,
    callback: Option<Callback>,
    wakers: Vec<Waker>,
}

pub(crate) struct Inner {
    pub(crate) handle: Arc<sys::Handle>,
    owner: Weak<HandleShared>,
    pub(crate) kind: TransferType,
    pub(crate) endpoint: u8,
    pub(crate) state: Mutex<State>,
    cond: Condvar,
    pub(crate) sys: sys::TransferData,
}

impl Inner {
    /// Locks the state, tolerating a poisoned mutex (a panicking callback must
    /// not wedge the transfer forever).
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called by the backend exactly once per submission, from any thread,
    /// with no locks held. Publishes the result, wakes waiters and runs the
    /// completion callback.
    pub(crate) fn complete(self: &Arc<Self>, status: TransferStatus, actual_length: usize, iso: Option<Vec<IsoPacket>>) {
        let (callback, wakers) = {
            let mut st = self.lock();
            st.in_flight = false;
            st.status = status;
            st.actual_length = actual_length;
            if let Some(iso) = iso {
                st.iso_packets = iso;
            }
            (st.callback.take(), std::mem::take(&mut st.wakers))
        };
        self.cond.notify_all();
        for w in wakers {
            w.wake();
        }
        if let Some(mut cb) = callback {
            let t = Transfer { inner: Arc::clone(self) };
            cb(&t);
            // Put the callback back unless the callback installed another one.
            let mut st = self.lock();
            if st.callback.is_none() {
                st.callback = Some(cb);
            }
        }
    }
}

/// An asynchronous USB transfer. See the [module documentation](self).
///
/// `Transfer` is a cheap handle: cloning it yields another handle to the same
/// transfer. Dropping every handle while the transfer is in flight does not
/// cancel it; the transfer runs to completion (or timeout) in the background
/// and its resources are released then. Cancel explicitly if that is not what
/// you want.
#[derive(Clone)]
pub struct Transfer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.inner.lock();
        f.debug_struct("Transfer")
            .field("kind", &self.inner.kind)
            .field("endpoint", &format_args!("{:#04x}", self.inner.endpoint))
            .field("in_flight", &st.in_flight)
            .field("buffer_len", &st.buffer.len())
            .field("status", &st.status)
            .field("actual_length", &st.actual_length)
            .finish()
    }
}

impl Transfer {
    /// Allocates a transfer of any kind on the given endpoint with the given
    /// buffer. Prefer the typed constructors ([`bulk`](Self::bulk),
    /// [`interrupt`](Self::interrupt), [`control`](Self::control),
    /// [`isochronous`](Self::isochronous)) unless you are building on top of
    /// the raw layer.
    ///
    /// For a control transfer the buffer must start with the 8-byte setup
    /// packet. For an isochronous transfer, set the packet layout with
    /// [`set_iso_packet_lengths`](Self::set_iso_packet_lengths) before
    /// submitting.
    pub fn new(handle: &DeviceHandle, kind: TransferType, endpoint: u8, buffer: Vec<u8>) -> Transfer {
        let shared = handle.shared();
        Transfer {
            inner: Arc::new(Inner {
                handle: Arc::clone(&shared.sys),
                owner: Arc::downgrade(shared),
                kind,
                endpoint,
                state: Mutex::new(State {
                    in_flight: false,
                    buffer,
                    timeout: Duration::ZERO,
                    iso_packets: Vec::new(),
                    flags: TransferFlags::default(),
                    status: TransferStatus::Completed,
                    actual_length: 0,
                    callback: None,
                    wakers: Vec::new(),
                }),
                cond: Condvar::new(),
                sys: sys::TransferData::default(),
            }),
        }
    }

    /// Allocates a bulk transfer. The direction comes from bit 7 of
    /// `endpoint`; for IN endpoints the buffer's length is the amount to read.
    pub fn bulk(handle: &DeviceHandle, endpoint: u8, buffer: Vec<u8>) -> Transfer {
        Self::new(handle, TransferType::Bulk, endpoint, buffer)
    }

    /// Allocates an interrupt transfer.
    pub fn interrupt(handle: &DeviceHandle, endpoint: u8, buffer: Vec<u8>) -> Transfer {
        Self::new(handle, TransferType::Interrupt, endpoint, buffer)
    }

    /// Allocates a control transfer on endpoint 0.
    ///
    /// For an OUT request, `data` is the payload and `setup.length` is
    /// overwritten with its length. For an IN request, `data` is ignored and
    /// `setup.length` bytes are reserved for the response.
    pub fn control(handle: &DeviceHandle, mut setup: ControlSetup, data: &[u8]) -> Transfer {
        let mut buffer = Vec::with_capacity(ControlSetup::SIZE + setup.length.max(data.len() as u16) as usize);
        if setup.direction() == crate::types::Direction::Out {
            setup.length = data.len() as u16;
            buffer.extend_from_slice(&setup.to_bytes());
            buffer.extend_from_slice(data);
        } else {
            buffer.extend_from_slice(&setup.to_bytes());
            buffer.resize(ControlSetup::SIZE + setup.length as usize, 0);
        }
        Self::new(handle, TransferType::Control, 0, buffer)
    }

    /// Allocates an isochronous transfer of `num_packets` packets of
    /// `packet_length` bytes each, laid out back to back in the buffer.
    ///
    /// For a portable transfer, make `packet_length` the endpoint's maximum
    /// packet size: WinUSB slices the buffer at that size itself rather than
    /// following the packet table, and rejects any other layout (the last
    /// packet of an OUT transfer may be shorter). Linux and macOS accept
    /// arbitrary per-packet lengths.
    pub fn isochronous(handle: &DeviceHandle, endpoint: u8, packet_length: usize, num_packets: usize) -> Transfer {
        let t = Self::new(handle, TransferType::Isochronous, endpoint, vec![0u8; packet_length * num_packets]);
        t.inner.lock().iso_packets = vec![IsoPacket::new(packet_length as u32); num_packets];
        t
    }

    /// The device handle this transfer was created on, if it is still open.
    pub fn handle(&self) -> Option<DeviceHandle> {
        self.inner.owner.upgrade().map(DeviceHandle::from_shared)
    }

    /// The transfer type.
    pub fn kind(&self) -> TransferType {
        self.inner.kind
    }

    /// The endpoint address (direction bit included).
    pub fn endpoint(&self) -> u8 {
        self.inner.endpoint
    }

    /// `true` while the transfer is submitted and not yet completed.
    pub fn is_pending(&self) -> bool {
        self.inner.lock().in_flight
    }

    fn idle(&self) -> Result<MutexGuard<'_, State>> {
        let st = self.inner.lock();
        if st.in_flight {
            Err(Error::with_message(ErrorKind::Busy, "transfer is in flight"))
        } else {
            Ok(st)
        }
    }

    /// Sets the timeout applied to each submission. [`Duration::ZERO`] (the
    /// default) means no timeout.
    pub fn set_timeout(&self, timeout: Duration) -> Result<()> {
        self.idle()?.timeout = timeout;
        Ok(())
    }

    /// The configured timeout.
    pub fn timeout(&self) -> Duration {
        self.inner.lock().timeout
    }

    /// Sets the transfer flags.
    pub fn set_flags(&self, flags: TransferFlags) -> Result<()> {
        self.idle()?.flags = flags;
        Ok(())
    }

    /// The transfer flags.
    pub fn flags(&self) -> TransferFlags {
        self.inner.lock().flags
    }

    /// Installs a completion callback, replacing any previous one. It runs on
    /// the context's event thread after every completion, so keep it short and
    /// never block on another transfer from inside it. Resubmitting the same
    /// transfer from the callback is allowed.
    pub fn set_callback<F>(&self, callback: F) -> Result<()>
    where
        F: FnMut(&Transfer) + Send + 'static,
    {
        self.idle()?.callback = Some(Box::new(callback));
        Ok(())
    }

    /// Removes the completion callback.
    pub fn clear_callback(&self) -> Result<()> {
        self.idle()?.callback = None;
        Ok(())
    }

    /// Replaces the buffer. For a control transfer the new buffer must again
    /// start with the setup packet.
    pub fn set_buffer(&self, buffer: Vec<u8>) -> Result<()> {
        self.idle()?.buffer = buffer;
        Ok(())
    }

    /// Takes the buffer out, leaving an empty one behind.
    pub fn take_buffer(&self) -> Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.idle()?.buffer))
    }

    /// Mutable access to the whole buffer (setup packet included for control
    /// transfers). Fails with [`ErrorKind::Busy`] while in flight.
    pub fn buffer(&self) -> Result<BufferGuard<'_>> {
        self.idle().map(|guard| BufferGuard { guard })
    }

    /// The bytes actually transferred by the last submission: the received
    /// data for IN transfers, the consumed data for OUT transfers. For control
    /// transfers this excludes the setup packet.
    pub fn data(&self) -> Result<DataGuard<'_>> {
        let guard = self.idle()?;
        let start = if self.inner.kind == TransferType::Control {
            ControlSetup::SIZE
        } else {
            0
        };
        let end = (start + guard.actual_length).min(guard.buffer.len());
        Ok(DataGuard {
            guard,
            range: start.min(end)..end,
        })
    }

    /// Rewrites the setup packet of a control transfer and resizes the data
    /// area to `setup.length`.
    pub fn set_control_setup(&self, setup: ControlSetup) -> Result<()> {
        if self.inner.kind != TransferType::Control {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a control transfer"));
        }
        let mut st = self.idle()?;
        let mut buffer = std::mem::take(&mut st.buffer);
        buffer.resize(ControlSetup::SIZE + setup.length as usize, 0);
        buffer[..ControlSetup::SIZE].copy_from_slice(&setup.to_bytes());
        st.buffer = buffer;
        Ok(())
    }

    /// The setup packet of a control transfer.
    pub fn control_setup(&self) -> Result<ControlSetup> {
        if self.inner.kind != TransferType::Control {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a control transfer"));
        }
        let st = self.inner.lock();
        let b: [u8; 8] = st
            .buffer
            .get(..ControlSetup::SIZE)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "buffer shorter than a setup packet"))?;
        Ok(ControlSetup::from_bytes(b))
    }

    /// Sets the per-packet lengths of an isochronous transfer. The buffer is
    /// grown if the packets need more room than it has.
    ///
    /// See [`isochronous`](Self::isochronous) for what Windows accepts here.
    pub fn set_iso_packet_lengths(&self, lengths: &[u32]) -> Result<()> {
        if self.inner.kind != TransferType::Isochronous {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not an isochronous transfer"));
        }
        let mut st = self.idle()?;
        let total: usize = lengths.iter().map(|&l| l as usize).sum();
        if st.buffer.len() < total {
            st.buffer.resize(total, 0);
        }
        st.iso_packets = lengths.iter().map(|&l| IsoPacket::new(l)).collect();
        Ok(())
    }

    /// Per-packet results of an isochronous transfer, filled in on completion.
    pub fn iso_packets(&self) -> Vec<IsoPacket> {
        self.inner.lock().iso_packets.clone()
    }

    /// The result of the last completed submission. Meaningless before the
    /// first submission (it reads as `Completed`).
    pub fn status(&self) -> TransferStatus {
        self.inner.lock().status
    }

    /// Bytes transferred by the last completed submission.
    pub fn actual_length(&self) -> usize {
        self.inner.lock().actual_length
    }

    /// Submits the transfer. Fails with [`ErrorKind::Busy`] if it is already
    /// in flight. On success the transfer belongs to the OS until it completes.
    pub fn submit(&self) -> Result<()> {
        let mut st = self.idle()?;
        if self.inner.kind == TransferType::Control && st.buffer.len() < ControlSetup::SIZE {
            return Err(Error::with_message(
                ErrorKind::InvalidParam,
                "control buffer shorter than a setup packet",
            ));
        }
        if self.inner.kind == TransferType::Isochronous {
            let total: usize = st.iso_packets.iter().map(|p| p.length as usize).sum();
            if st.iso_packets.is_empty() || total > st.buffer.len() {
                return Err(Error::with_message(
                    ErrorKind::InvalidParam,
                    "isochronous packet layout does not fit the buffer",
                ));
            }
        }
        st.in_flight = true;
        st.actual_length = 0;
        match self.inner.handle.submit(&self.inner, &mut st) {
            Ok(()) => Ok(()),
            Err(e) => {
                st.in_flight = false;
                Err(e)
            }
        }
    }

    /// Asks the OS to abort an in-flight transfer. The transfer completes
    /// asynchronously with [`TransferStatus::Cancelled`] (or another status if
    /// it finished first); wait for it as usual. Fails with
    /// [`ErrorKind::NotFound`] if the transfer is not in flight.
    pub fn cancel(&self) -> Result<()> {
        self.inner.handle.cancel(&self.inner)
    }

    /// Blocks until the transfer completes and returns its status. With a
    /// timeout, fails with [`ErrorKind::Timeout`] if it elapses first; the
    /// transfer then stays in flight. Returns immediately if the transfer is
    /// not in flight.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<TransferStatus> {
        let mut st = self.inner.lock();
        match timeout {
            None => {
                while st.in_flight {
                    st = self.inner.cond.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            Some(limit) => {
                let deadline = std::time::Instant::now() + limit;
                while st.in_flight {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Err(Error::new(ErrorKind::Timeout));
                    }
                    st = self
                        .inner
                        .cond
                        .wait_timeout(st, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        Ok(st.status)
    }

    /// Submits the transfer, blocks until it completes, and converts its
    /// status into a result carrying the actual length.
    pub fn submit_and_wait(&self) -> Result<usize> {
        self.submit()?;
        let status = self.wait(None)?;
        status.into_result()?;
        Ok(self.actual_length())
    }

    /// A future that resolves with the status once the transfer completes.
    /// Resolves immediately if it is not in flight.
    pub fn completion(&self) -> Completion<'_> {
        Completion { transfer: self }
    }
}

/// Mutable access to a transfer's buffer, held while the guard lives.
pub struct BufferGuard<'a> {
    guard: MutexGuard<'a, State>,
}

impl std::fmt::Debug for BufferGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferGuard").field("len", &self.guard.buffer.len()).finish()
    }
}

impl Deref for BufferGuard<'_> {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.guard.buffer
    }
}

impl DerefMut for BufferGuard<'_> {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.guard.buffer
    }
}

/// Read access to the bytes a transfer actually moved.
pub struct DataGuard<'a> {
    guard: MutexGuard<'a, State>,
    range: std::ops::Range<usize>,
}

impl std::fmt::Debug for DataGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataGuard").field("len", &self.range.len()).finish()
    }
}

impl Deref for DataGuard<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.guard.buffer[self.range.clone()]
    }
}

/// Future returned by [`Transfer::completion`].
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct Completion<'a> {
    transfer: &'a Transfer,
}

impl Future for Completion<'_> {
    type Output = TransferStatus;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<TransferStatus> {
        let mut st = self.transfer.inner.lock();
        if !st.in_flight {
            return Poll::Ready(st.status);
        }
        if !st.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            st.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Nothing can be in flight here: the backend holds a strong reference
        // for every submitted transfer until it completes.
        debug_assert!(!self.state.get_mut().map(|s| s.in_flight).unwrap_or(false));
    }
}
