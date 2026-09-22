//! Linux backend: sysfs for enumeration, usbfs (`/dev/bus/usb`) for I/O.
//!
//! Every transfer is one or more usbfs URBs. A single event thread per
//! context polls the open device nodes (usbfs signals `POLLOUT` when a URB is
//! ready to be reaped), reaps completed URBs, and enforces transfer timeouts
//! by discarding URBs whose deadline has passed.

mod ffi;
mod usbfs;

use super::{DeviceInfo, split_config_descriptors};
use crate::descriptors::DeviceDescriptor;
use crate::transfer::{Inner, State};
use crate::types::{IsoPacket, Speed, TransferStatus, TransferType};
use crate::{Error, ErrorKind, Result};
use std::collections::HashMap;
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::File;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const SYSFS_DEVICES: &str = "/sys/bus/usb/devices";
const DEVFS_ROOT: &str = "/dev/bus/usb";

/// Maps an errno value to an error kind.
pub(crate) fn errno_kind(code: i32) -> ErrorKind {
    match code {
        ffi::EACCES | ffi::EPERM => ErrorKind::Access,
        ffi::ENOENT => ErrorKind::NotFound,
        ffi::ENODEV | ffi::ESHUTDOWN => ErrorKind::NoDevice,
        ffi::EBUSY => ErrorKind::Busy,
        ffi::ETIMEDOUT => ErrorKind::Timeout,
        ffi::EPIPE => ErrorKind::Pipe,
        ffi::EOVERFLOW => ErrorKind::Overflow,
        ffi::EINVAL => ErrorKind::InvalidParam,
        ffi::ENOMEM => ErrorKind::NoMem,
        ffi::EINTR => ErrorKind::Interrupted,
        ffi::ENOSYS | ffi::ENOTTY | ffi::EOPNOTSUPP => ErrorKind::NotSupported,
        _ => ErrorKind::Io,
    }
}

fn os_error(op: &'static str) -> Error {
    let code = ffi::errno();
    Error::from_code(errno_kind(code), code).context(op)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a device lives: its sysfs entry and its usbfs node.
pub(crate) struct Location {
    sysfs: PathBuf,
    devnode: PathBuf,
}

// ----- context & event thread ---------------------------------------------------

pub(crate) struct Context {
    handles: Mutex<Vec<Weak<Handle>>>,
    wake: PipeWriter,
}

impl Context {
    pub(crate) fn new() -> Result<Arc<Self>> {
        let (reader, writer) = std::io::pipe().map_err(|e| Error::from(e).context("pipe"))?;
        ffi::set_nonblocking(reader.as_raw_fd())?;
        ffi::set_nonblocking(writer.as_raw_fd())?;
        let ctx = Arc::new(Context {
            handles: Mutex::new(Vec::new()),
            wake: writer,
        });
        let weak = Arc::downgrade(&ctx);
        std::thread::Builder::new()
            .name("rawusb-events".into())
            .spawn(move || event_loop(weak, reader))
            .map_err(|e| Error::from(e).context("spawn event thread"))?;
        Ok(ctx)
    }

    fn wake(&self) {
        // A full pipe means the thread already has plenty to wake up for.
        let _ = (&self.wake).write(&[1u8]);
    }

    pub(crate) fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        let dir = std::fs::read_dir(SYSFS_DEVICES).map_err(|e| Error::from(e).context("read /sys/bus/usb/devices"))?;
        let mut out = Vec::new();
        for entry in dir {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(info) = read_sysfs_device(name, entry.path()) {
                out.push(info);
            }
        }
        out.sort_by_key(|d| (d.bus_number, d.address));
        Ok(out)
    }

    pub(crate) fn open(self: &Arc<Self>, dev: &DeviceInfo) -> Result<Arc<Handle>> {
        let file = File::options().read(true).write(true).open(&dev.location.devnode).map_err(|e| {
            let err = Error::from(e);
            let kind = if err.kind() == ErrorKind::NotFound {
                ErrorKind::NoDevice
            } else {
                err.kind()
            };
            Error::from_code(kind, err.os_code().unwrap_or(0)).context("open device node")
        })?;
        let mut caps: u32 = 0;
        // SAFETY: GET_CAPABILITIES writes one u32 through the pointer.
        if unsafe { ffi::ioctl(file.as_raw_fd(), usbfs::USBDEVFS_GET_CAPABILITIES, &mut caps as *mut u32) } < 0 {
            caps = 0;
        }
        let handle = Arc::new(Handle {
            file,
            ctx: Arc::clone(self),
            sysfs: dev.location.sysfs.clone(),
            caps,
            pending: Mutex::new(HashMap::new()),
            disconnected: AtomicBool::new(false),
        });
        lock(&self.handles).push(Arc::downgrade(&handle));
        self.wake();
        Ok(handle)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // Closing the write end hangs up the reader and ends the thread.
    }
}

fn event_loop(ctx: Weak<Context>, mut reader: PipeReader) {
    let wake_fd = reader.as_raw_fd();
    let mut scratch = [0u8; 256];
    loop {
        let handles: Vec<Arc<Handle>> = {
            let Some(ctx) = ctx.upgrade() else { break };
            let mut g = lock(&ctx.handles);
            g.retain(|w| w.strong_count() > 0);
            g.iter()
                .filter_map(|w| w.upgrade())
                .filter(|h| !h.disconnected.load(Ordering::Relaxed))
                .collect()
        };

        let mut fds: Vec<ffi::pollfd> = Vec::with_capacity(handles.len() + 1);
        fds.push(ffi::pollfd {
            fd: wake_fd,
            events: ffi::POLLIN,
            revents: 0,
        });
        for h in &handles {
            fds.push(ffi::pollfd {
                fd: h.file.as_raw_fd(),
                events: ffi::POLLOUT,
                revents: 0,
            });
        }

        let now = Instant::now();
        let timeout_ms: c_int = match handles.iter().filter_map(|h| h.next_deadline()).min() {
            Some(deadline) => {
                let d = deadline.saturating_duration_since(now);
                // Round up so we never spin on a deadline a few microseconds away.
                d.as_millis().saturating_add(1).min(c_int::MAX as u128) as c_int
            }
            None => -1,
        };

        // SAFETY: `fds` is a valid array of `fds.len()` pollfd entries.
        let n = unsafe { ffi::poll(fds.as_mut_ptr(), fds.len() as c_ulong, timeout_ms) };
        if n < 0 {
            if ffi::errno() == ffi::EINTR {
                continue;
            }
            // Should not happen; avoid a hot loop if it does.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        let wake = fds[0].revents;
        if wake & ffi::POLLIN != 0 {
            while let Ok(n) = reader.read(&mut scratch) {
                if n < scratch.len() {
                    break;
                }
            }
        }
        if wake & (ffi::POLLHUP | ffi::POLLERR | ffi::POLLNVAL) != 0 && ctx.upgrade().is_none() {
            break;
        }

        let now = Instant::now();
        for (h, pfd) in handles.iter().zip(fds.iter().skip(1)) {
            let re = pfd.revents;
            if re & (ffi::POLLOUT | ffi::POLLERR | ffi::POLLHUP) != 0 {
                h.reap();
            }
            if re & (ffi::POLLERR | ffi::POLLHUP | ffi::POLLNVAL) != 0 {
                h.disconnect();
            }
            h.expire(now);
        }
    }
}

// ----- enumeration ---------------------------------------------------------------

fn read_sysfs_device(name: &str, path: PathBuf) -> Option<DeviceInfo> {
    // Interfaces look like "1-2:1.0"; devices like "usb1" or "1-2.3".
    if name.contains(':') {
        return None;
    }
    let port_numbers: Vec<u8> = if name.starts_with("usb") {
        Vec::new()
    } else {
        let (_, ports) = name.split_once('-')?;
        ports.split('.').map(|p| p.parse().ok()).collect::<Option<Vec<u8>>>()?
    };
    let read_num = |f: &str| -> Option<u32> { std::fs::read_to_string(path.join(f)).ok()?.trim().parse().ok() };
    let bus_number = read_num("busnum")? as u8;
    let address = read_num("devnum")? as u8;
    let speed = std::fs::read_to_string(path.join("speed"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(Speed::from_mbps)
        .unwrap_or(Speed::Unknown);
    let descriptors = std::fs::read(path.join("descriptors")).ok()?;
    let device_descriptor = DeviceDescriptor::from_bytes(&descriptors).ok()?;
    let configs = split_config_descriptors(&descriptors[DeviceDescriptor::SIZE..]);
    let active_config = std::fs::read_to_string(path.join("bConfigurationValue"))
        .ok()
        .map(|s| s.trim().parse::<u8>().unwrap_or(0));
    Some(DeviceInfo {
        bus_number,
        address,
        port_numbers,
        speed,
        device_descriptor,
        configs,
        active_config,
        location: Location {
            sysfs: path,
            devnode: PathBuf::from(format!("{DEVFS_ROOT}/{bus_number:03}/{address:03}")),
        },
    })
}

// ----- handle --------------------------------------------------------------------

pub(crate) struct Handle {
    file: File,
    ctx: Arc<Context>,
    sysfs: PathBuf,
    caps: u32,
    /// Every URB currently owned by the kernel, keyed by its address.
    pending: Mutex<HashMap<usize, (Arc<Inner>, usize)>>,
    disconnected: AtomicBool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // The file closes itself; tell the thread to forget the fd.
        self.ctx.wake();
    }
}

impl Handle {
    fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }

    /// Runs an ioctl that takes a pointer argument.
    fn ioctl<T>(&self, request: c_ulong, arg: *mut T, op: &'static str) -> Result<c_int> {
        // SAFETY: the caller passes a pointer valid for the request's layout.
        let r = unsafe { ffi::ioctl(self.fd(), request, arg) };
        if r < 0 { Err(os_error(op)) } else { Ok(r) }
    }

    pub(crate) fn active_configuration(&self) -> Result<Option<u8>> {
        match std::fs::read_to_string(self.sysfs.join("bConfigurationValue")) {
            Ok(s) => Ok(Some(s.trim().parse::<u8>().unwrap_or(0))),
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn set_configuration(&self, value: u8) -> Result<()> {
        let mut v = value as u32;
        self.ioctl(usbfs::USBDEVFS_SETCONFIGURATION, &mut v, "set configuration").map(drop)
    }

    pub(crate) fn claim_interface(&self, interface: u8) -> Result<()> {
        let mut v = interface as u32;
        self.ioctl(usbfs::USBDEVFS_CLAIMINTERFACE, &mut v, "claim interface").map(drop)
    }

    pub(crate) fn release_interface(&self, interface: u8) -> Result<()> {
        let mut v = interface as u32;
        self.ioctl(usbfs::USBDEVFS_RELEASEINTERFACE, &mut v, "release interface").map(drop)
    }

    pub(crate) fn set_alt_setting(&self, interface: u8, alt: u8) -> Result<()> {
        let mut v = usbfs::usbdevfs_setinterface {
            interface: interface as u32,
            altsetting: alt as u32,
        };
        self.ioctl(usbfs::USBDEVFS_SETINTERFACE, &mut v, "set alternate setting").map(drop)
    }

    pub(crate) fn clear_halt(&self, endpoint: u8) -> Result<()> {
        let mut v = endpoint as u32;
        self.ioctl(usbfs::USBDEVFS_CLEAR_HALT, &mut v, "clear halt").map(drop)
    }

    pub(crate) fn reset(&self) -> Result<()> {
        self.ioctl(usbfs::USBDEVFS_RESET, std::ptr::null_mut::<c_void>(), "reset device")
            .map(drop)
    }

    pub(crate) fn kernel_driver_active(&self, interface: u8) -> Result<bool> {
        let mut gd = usbfs::usbdevfs_getdriver {
            interface: interface as u32,
            driver: [0; usbfs::USBDEVFS_MAXDRIVERNAME + 1],
        };
        match self.ioctl(usbfs::USBDEVFS_GETDRIVER, &mut gd, "get driver") {
            Ok(_) => {
                let end = gd.driver.iter().position(|&b| b == 0).unwrap_or(gd.driver.len());
                Ok(&gd.driver[..end] != b"usbfs")
            }
            Err(e) if e.os_code() == Some(ffi::ENODATA) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn driver_ioctl(&self, interface: u8, code: c_ulong, op: &'static str) -> Result<()> {
        let mut cmd = usbfs::usbdevfs_ioctl {
            ifno: interface as i32,
            ioctl_code: code as i32,
            data: std::ptr::null_mut(),
        };
        match self.ioctl(usbfs::USBDEVFS_IOCTL, &mut cmd, op) {
            Ok(_) => Ok(()),
            Err(e) if e.os_code() == Some(ffi::ENODATA) => Err(Error::from_code(ErrorKind::NotFound, ffi::ENODATA).context(op)),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn detach_kernel_driver(&self, interface: u8) -> Result<()> {
        self.driver_ioctl(interface, usbfs::USBDEVFS_DISCONNECT, "detach kernel driver")
    }

    pub(crate) fn attach_kernel_driver(&self, interface: u8) -> Result<()> {
        self.driver_ioctl(interface, usbfs::USBDEVFS_CONNECT, "attach kernel driver")
    }

    // ----- transfers -----

    pub(crate) fn submit(&self, inner: &Arc<Inner>, st: &mut State) -> Result<()> {
        if self.disconnected.load(Ordering::Relaxed) {
            return Err(Error::with_message(ErrorKind::NoDevice, "device disconnected"));
        }
        let mut sub = lock(&inner.sys.sub);
        if sub.is_some() {
            return Err(Error::with_message(ErrorKind::Busy, "transfer already submitted"));
        }
        let submission = Submission::build(self, inner, st)?;
        let urb_count = submission.urbs.len();

        {
            let mut pending = lock(&self.pending);
            for (i, urb) in submission.urbs.iter().enumerate() {
                pending.insert(urb.ptr() as usize, (Arc::clone(inner), i));
            }
        }
        *sub = Some(submission);
        let submission = sub.as_mut().expect("just set");

        for i in 0..urb_count {
            // SAFETY: the URB and its buffer stay allocated until reaped; they
            // are owned by `Submission`, which lives in the transfer.
            let r = unsafe { ffi::ioctl(self.fd(), usbfs::USBDEVFS_SUBMITURB, submission.urbs[i].ptr()) };
            if r < 0 {
                let err = os_error("submit urb");
                let mut pending = lock(&self.pending);
                for urb in &submission.urbs[i..] {
                    pending.remove(&(urb.ptr() as usize));
                }
                drop(pending);
                if i == 0 {
                    *sub = None;
                    return Err(err);
                }
                // Some URBs are already with the kernel: let them drain and
                // report the failure through the completion path, as libusb
                // does.
                submission.discarded_from = i;
                submission.urbs.truncate(i);
                submission.status = Some(TransferStatus::Error);
                for urb in &submission.urbs {
                    // SAFETY: discarding a URB we submitted on this fd.
                    unsafe { ffi::ioctl(self.fd(), usbfs::USBDEVFS_DISCARDURB, urb.ptr()) };
                }
                break;
            }
        }
        drop(sub);
        self.ctx.wake();
        Ok(())
    }

    /// Discards every URB of a submission. Returns how many were actually
    /// still pending in the kernel.
    fn discard(&self, sub: &Submission, from: usize) -> usize {
        let mut n = 0;
        for urb in sub.urbs.iter().skip(from) {
            // SAFETY: discarding a URB we submitted on this fd; EINVAL for
            // already-completed ones is expected and ignored.
            if unsafe { ffi::ioctl(self.fd(), usbfs::USBDEVFS_DISCARDURB, urb.ptr()) } == 0 {
                n += 1;
            }
        }
        n
    }

    pub(crate) fn cancel(&self, inner: &Arc<Inner>) -> Result<()> {
        let mut g = lock(&inner.sys.sub);
        let Some(sub) = g.as_mut() else {
            return Err(Error::with_message(ErrorKind::NotFound, "transfer is not in flight"));
        };
        if self.discard(sub, 0) > 0 {
            sub.cancelled = true;
        }
        Ok(())
    }

    pub(crate) fn cancel_all(&self) {
        for inner in self.pending_transfers() {
            let _ = self.cancel(&inner);
        }
    }

    fn pending_transfers(&self) -> Vec<Arc<Inner>> {
        let pending = lock(&self.pending);
        let mut out: Vec<Arc<Inner>> = Vec::new();
        for (t, _) in pending.values() {
            if !out.iter().any(|o| Arc::ptr_eq(o, t)) {
                out.push(Arc::clone(t));
            }
        }
        out
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending_transfers()
            .iter()
            .filter_map(|t| lock(&t.sys.sub).as_ref().and_then(|s| s.deadline))
            .min()
    }

    /// Discards the URBs of every transfer whose deadline has passed.
    fn expire(&self, now: Instant) {
        for inner in self.pending_transfers() {
            let mut g = lock(&inner.sys.sub);
            let Some(sub) = g.as_mut() else { continue };
            if sub.timed_out || sub.deadline.is_none_or(|d| d > now) {
                continue;
            }
            if self.discard(sub, 0) > 0 {
                sub.timed_out = true;
            }
            // Otherwise every URB had already completed; the reaps will
            // report the real outcome.
            sub.deadline = None;
        }
    }

    /// Reaps completed URBs until the kernel has none left.
    fn reap(&self) {
        loop {
            let mut ptr: *mut usbfs::usbdevfs_urb = std::ptr::null_mut();
            // SAFETY: REAPURBNDELAY stores one pointer through the argument.
            let r = unsafe { ffi::ioctl(self.fd(), usbfs::USBDEVFS_REAPURBNDELAY, &mut ptr as *mut *mut usbfs::usbdevfs_urb) };
            if r < 0 {
                match ffi::errno() {
                    ffi::EINTR => continue,
                    ffi::EAGAIN => break,
                    _ => {
                        // ENODEV: the device is gone and so are its URBs.
                        self.disconnect();
                        break;
                    }
                }
            }
            let entry = lock(&self.pending).remove(&(ptr as usize));
            if let Some((inner, index)) = entry {
                self.finish_urb(&inner, index);
            }
        }
    }

    /// Accounts for one reaped URB and completes the transfer if it was the
    /// last one.
    fn finish_urb(&self, inner: &Arc<Inner>, index: usize) {
        let done = {
            let mut g = lock(&inner.sys.sub);
            let Some(sub) = g.as_mut() else { return };
            // SAFETY: the kernel has handed the URB back; nothing else
            // touches it now.
            let urb = unsafe { &*sub.urbs[index].ptr() };
            sub.reaped += 1;
            let ours = index >= sub.discarded_from || sub.cancelled || sub.timed_out;

            if inner.kind == TransferType::Isochronous {
                let base = sub.iso_offsets[index];
                let count = urb.number_of_packets as usize;
                for i in 0..count {
                    // SAFETY: `count` descriptors follow the URB header.
                    let d = unsafe { *sub.urbs[index].iso_desc(i) };
                    sub.iso_results[base + i] = IsoPacket {
                        length: d.length,
                        actual_length: d.actual_length,
                        status: iso_status(d.status as i32),
                    };
                    sub.actual += d.actual_length as usize;
                }
            } else {
                sub.actual += urb.actual_length.max(0) as usize;
            }

            if urb.status != 0
                && !ours
                && let Some(s) = urb_status(urb.status)
                && sub.status.is_none()
            {
                sub.status = Some(s);
            }

            // A short or failed URB in the middle: the kernel already
            // cancelled the continuation URBs; make sure by discarding them.
            let short = inner.kind != TransferType::Isochronous && urb.actual_length < urb.buffer_length;
            if (short || urb.status != 0) && index + 1 < sub.urbs.len() && sub.discarded_from > index + 1 {
                sub.discarded_from = index + 1;
                self.discard(sub, index + 1);
            }

            if sub.reaped == sub.urbs.len() { g.take() } else { None }
        };
        if let Some(sub) = done {
            let status = if sub.timed_out {
                TransferStatus::TimedOut
            } else if sub.cancelled {
                TransferStatus::Cancelled
            } else {
                sub.status.unwrap_or(TransferStatus::Completed)
            };
            let iso = if inner.kind == TransferType::Isochronous {
                Some(sub.iso_results)
            } else {
                None
            };
            inner.complete(status, sub.actual, iso);
        }
    }

    /// The device is gone: fail everything still outstanding.
    fn disconnect(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        let drained: Vec<Arc<Inner>> = {
            let mut pending = lock(&self.pending);
            let mut out: Vec<Arc<Inner>> = Vec::new();
            for (_, (t, _)) in pending.drain() {
                if !out.iter().any(|o| Arc::ptr_eq(o, &t)) {
                    out.push(t);
                }
            }
            out
        };
        for inner in drained {
            let taken = lock(&inner.sys.sub).take();
            if let Some(sub) = taken {
                let iso = if inner.kind == TransferType::Isochronous {
                    Some(sub.iso_results)
                } else {
                    None
                };
                inner.complete(TransferStatus::NoDevice, sub.actual, iso);
            }
        }
    }
}

fn urb_status(status: i32) -> Option<TransferStatus> {
    Some(match -status {
        0 => TransferStatus::Completed,
        ffi::ENOENT | ffi::ECONNRESET => return None,
        ffi::ENODEV | ffi::ESHUTDOWN => TransferStatus::NoDevice,
        ffi::EPIPE => TransferStatus::Stall,
        ffi::EOVERFLOW => TransferStatus::Overflow,
        ffi::ETIME | ffi::ETIMEDOUT => TransferStatus::TimedOut,
        _ => TransferStatus::Error,
    })
}

fn iso_status(status: i32) -> TransferStatus {
    match -status {
        0 => TransferStatus::Completed,
        ffi::ENOENT | ffi::ECONNRESET => TransferStatus::Cancelled,
        ffi::ENODEV | ffi::ESHUTDOWN => TransferStatus::NoDevice,
        ffi::EPIPE => TransferStatus::Stall,
        ffi::EOVERFLOW => TransferStatus::Overflow,
        ffi::ETIME | ffi::ETIMEDOUT => TransferStatus::TimedOut,
        _ => TransferStatus::Error,
    }
}

// ----- per-transfer state --------------------------------------------------------

/// Backend state stored inside every transfer.
#[derive(Default)]
pub(crate) struct TransferData {
    sub: Mutex<Option<Submission>>,
}

/// Heap storage for one URB plus its isochronous packet descriptors, aligned
/// for both.
struct UrbBox(Box<[u64]>);

impl UrbBox {
    fn new(packets: usize) -> UrbBox {
        let bytes = std::mem::size_of::<usbfs::usbdevfs_urb>() + packets * std::mem::size_of::<usbfs::usbdevfs_iso_packet_desc>();
        UrbBox(vec![0u64; bytes.div_ceil(8)].into_boxed_slice())
    }

    fn ptr(&self) -> *mut usbfs::usbdevfs_urb {
        self.0.as_ptr() as *mut usbfs::usbdevfs_urb
    }

    /// Pointer to the `i`-th isochronous packet descriptor.
    fn iso_desc(&self, i: usize) -> *mut usbfs::usbdevfs_iso_packet_desc {
        // SAFETY: the box was sized for this many descriptors after the header.
        unsafe {
            (self.ptr() as *mut u8)
                .add(std::mem::size_of::<usbfs::usbdevfs_urb>())
                .cast::<usbfs::usbdevfs_iso_packet_desc>()
                .add(i)
        }
    }
}

struct Submission {
    urbs: Vec<UrbBox>,
    reaped: usize,
    actual: usize,
    status: Option<TransferStatus>,
    deadline: Option<Instant>,
    timed_out: bool,
    cancelled: bool,
    /// URBs at this index and above were discarded by us (after a short or
    /// failed earlier URB); their status is not the transfer's status.
    discarded_from: usize,
    iso_results: Vec<IsoPacket>,
    /// First packet index of each URB (isochronous only).
    iso_offsets: Vec<usize>,
}

impl Submission {
    fn build(handle: &Handle, inner: &Arc<Inner>, st: &mut State) -> Result<Submission> {
        let buf_ptr = st.buffer.as_mut_ptr();
        let buf_len = st.buffer.len();
        if buf_len > i32::MAX as usize {
            return Err(Error::with_message(ErrorKind::InvalidParam, "buffer too large"));
        }
        let mut urbs = Vec::new();
        let mut iso_offsets = Vec::new();
        let mut iso_results = Vec::new();

        let new_urb = |packets: usize, ty: u8, offset: usize, len: usize, flags: u32| {
            let b = UrbBox::new(packets);
            // SAFETY: the box holds at least one URB header, zeroed.
            unsafe {
                b.ptr().write(usbfs::usbdevfs_urb {
                    type_: ty,
                    endpoint: inner.endpoint,
                    status: 0,
                    flags,
                    buffer: buf_ptr.add(offset) as *mut c_void,
                    buffer_length: len as i32,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets: packets as i32,
                    error_count: 0,
                    signr: 0,
                    usercontext: std::ptr::null_mut(),
                });
            }
            b
        };

        match inner.kind {
            TransferType::Control => {
                urbs.push(new_urb(0, usbfs::USBDEVFS_URB_TYPE_CONTROL, 0, buf_len, 0));
            }
            TransferType::Interrupt => {
                let mut flags = 0;
                if st.flags.zero_packet {
                    flags |= usbfs::USBDEVFS_URB_ZERO_PACKET;
                }
                if st.flags.short_not_ok {
                    flags |= usbfs::USBDEVFS_URB_SHORT_NOT_OK;
                }
                urbs.push(new_urb(0, usbfs::USBDEVFS_URB_TYPE_INTERRUPT, 0, buf_len, flags));
            }
            TransferType::Bulk => {
                let chunk = if handle.caps & usbfs::USBDEVFS_CAP_BULK_SCATTER_GATHER != 0 {
                    buf_len.max(1)
                } else {
                    usbfs::MAX_BULK_BUFFER_LENGTH
                };
                let count = buf_len.div_ceil(chunk).max(1);
                if count > 1 && handle.caps & usbfs::USBDEVFS_CAP_BULK_CONTINUATION == 0 {
                    return Err(Error::with_message(
                        ErrorKind::NotSupported,
                        "kernel too old for large bulk transfers",
                    ));
                }
                for i in 0..count {
                    let offset = i * chunk;
                    let len = (buf_len - offset).min(chunk);
                    let mut flags = 0;
                    if i > 0 {
                        flags |= usbfs::USBDEVFS_URB_BULK_CONTINUATION;
                    }
                    if st.flags.short_not_ok {
                        flags |= usbfs::USBDEVFS_URB_SHORT_NOT_OK;
                    }
                    if st.flags.zero_packet && i + 1 == count {
                        flags |= usbfs::USBDEVFS_URB_ZERO_PACKET;
                    }
                    urbs.push(new_urb(0, usbfs::USBDEVFS_URB_TYPE_BULK, offset, len, flags));
                }
            }
            TransferType::Isochronous => {
                let packets = &st.iso_packets;
                iso_results = packets.iter().map(|p| IsoPacket::new(p.length)).collect();
                let mut offset = 0usize;
                for (ci, chunk) in packets.chunks(usbfs::MAX_ISO_PACKETS_PER_URB).enumerate() {
                    let len: usize = chunk.iter().map(|p| p.length as usize).sum();
                    let b = new_urb(chunk.len(), usbfs::USBDEVFS_URB_TYPE_ISO, offset, len, usbfs::USBDEVFS_URB_ISO_ASAP);
                    for (i, p) in chunk.iter().enumerate() {
                        // SAFETY: the box was sized for `chunk.len()` descriptors.
                        unsafe {
                            b.iso_desc(i).write(usbfs::usbdevfs_iso_packet_desc {
                                length: p.length,
                                actual_length: 0,
                                status: 0,
                            });
                        }
                    }
                    iso_offsets.push(ci * usbfs::MAX_ISO_PACKETS_PER_URB);
                    urbs.push(b);
                    offset += len;
                }
            }
        }

        let deadline = if st.timeout.is_zero() {
            None
        } else {
            Some(Instant::now() + st.timeout)
        };
        Ok(Submission {
            urbs,
            reaped: 0,
            actual: 0,
            status: None,
            deadline,
            timed_out: false,
            cancelled: false,
            discarded_from: usize::MAX,
            iso_results,
            iso_offsets,
        })
    }
}
