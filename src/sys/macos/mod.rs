//! macOS backend on IOKit's `IOUSBLib` user-client interfaces.
//!
//! Devices are matched in the I/O Registry, wrapped in `IOUSBDeviceInterface`
//! objects, and their interfaces in `IOUSBInterfaceInterface` objects once
//! claimed. Every asynchronous request completes through a CFRunLoop source
//! that the context's event thread runs. When the `182` interface revisions
//! are available (any macOS from the last two decades) transfer timeouts are
//! enforced by the kernel through the `...TO` calls; otherwise the event
//! thread aborts the pipe when the deadline passes.

// IOKit constant names are kept as Apple spells them.
#![allow(non_upper_case_globals)]

mod ffi;

use super::DeviceInfo;
#[cfg(feature = "hotplug")]
use super::Notifier;
use crate::descriptors::DeviceDescriptor;
use crate::transfer::{Inner, State};
use crate::types::{ControlSetup, IsoPacket, Speed, TransferStatus, TransferType, Version, descriptor_type, request};
use crate::{Error, ErrorKind, Result};
use ffi::*;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
#[cfg(feature = "hotplug")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

/// Maps an `IOReturn` to an error kind.
pub(crate) fn errno_kind(code: i32) -> ErrorKind {
    match code {
        kIOReturnSuccess => ErrorKind::Other,
        kIOReturnNoDevice | kIOReturnNotAttached | kIOReturnNotResponding => ErrorKind::NoDevice,
        kIOReturnExclusiveAccess | kIOReturnNotPrivileged => ErrorKind::Access,
        kIOReturnNotOpen | kIOReturnBadArgument => ErrorKind::InvalidParam,
        kIOReturnTimeout | kIOUSBTransactionTimeout => ErrorKind::Timeout,
        kIOUSBPipeStalled => ErrorKind::Pipe,
        kIOReturnNoMemory | kIOReturnNoResources => ErrorKind::NoMem,
        kIOReturnUnsupported => ErrorKind::NotSupported,
        kIOReturnBusy => ErrorKind::Busy,
        kIOReturnAborted => ErrorKind::Interrupted,
        kIOReturnNotFound => ErrorKind::NotFound,
        kIOReturnOverrun => ErrorKind::Overflow,
        _ => ErrorKind::Io,
    }
}

fn io_error(code: IOReturn, op: &'static str) -> Error {
    Error::from_code(errno_kind(code), code).context(op)
}

fn check(code: IOReturn, op: &'static str) -> Result<()> {
    if code == kIOReturnSuccess {
        Ok(())
    } else {
        Err(io_error(code, op))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Calls a method on a COM-style IOKit object.
macro_rules! call {
    ($obj:expr, $method:ident $(, $arg:expr)*) => {
        // SAFETY: `$obj` is a live `T**` whose vtable has `$method` at this slot.
        unsafe { ((**$obj).$method)($obj as This $(, $arg)*) }
    };
}

/// Reads a scalar property through a `Get...` vtable method.
macro_rules! prop {
    ($obj:expr, $method:ident, $ty:ty) => {{
        let mut v: $ty = 0;
        call!($obj, $method, &mut v);
        v
    }};
}

fn run_loop_mode() -> CFStringRef {
    // SAFETY: reading a CoreFoundation constant.
    unsafe { kCFRunLoopDefaultMode }
}

// ----- device objects ------------------------------------------------------------------

/// An `IOUSBDeviceInterface` object shared by every `Device` and handle for
/// one physical device.
pub(crate) struct DeviceRef {
    obj: *mut *mut IOUSBDeviceInterface182,
    has_182: bool,
    rl: CFRunLoopRef,
    open_count: Mutex<u32>,
    source: Mutex<Option<CFRunLoopSourceRef>>,
}

// SAFETY: IOUSBLib objects are usable from any thread; we serialise our own
// bookkeeping with mutexes.
unsafe impl Send for DeviceRef {}
// SAFETY: as above.
unsafe impl Sync for DeviceRef {}

impl Drop for DeviceRef {
    fn drop(&mut self) {
        if *self.open_count.get_mut().unwrap_or_else(|e| e.into_inner()) > 0 {
            call!(self.obj, USBDeviceClose);
        }
        if let Some(src) = self.source.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            // SAFETY: removing and releasing a source we created and added.
            unsafe {
                CFRunLoopRemoveSource(self.rl, src, run_loop_mode());
                CFRelease(src);
            }
        }
        call!(self.obj, Release);
        // SAFETY: balancing the retain taken when this object was created.
        unsafe { CFRelease(self.rl) };
    }
}

impl DeviceRef {
    /// Creates the device object for a registry service.
    fn from_service(service: io_service_t, rl: CFRunLoopRef) -> Result<DeviceRef> {
        let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
        let mut score: i32 = 0;
        // SAFETY: valid service and out-pointers.
        let kr = unsafe {
            IOCreatePlugInInterfaceForService(
                service,
                uuid_ref(&kIOUSBDeviceUserClientTypeID),
                uuid_ref(&kIOCFPlugInInterfaceID),
                &mut plugin,
                &mut score,
            )
        };
        if kr != kIOReturnSuccess || plugin.is_null() {
            return Err(io_error(kr, "IOCreatePlugInInterfaceForService"));
        }
        let mut obj: *mut c_void = std::ptr::null_mut();
        let mut has_182 = true;
        let mut hr = call!(plugin, QueryInterface, kIOUSBDeviceInterfaceID182, &mut obj);
        if hr != 0 || obj.is_null() {
            has_182 = false;
            hr = call!(plugin, QueryInterface, kIOUSBDeviceInterfaceID, &mut obj);
        }
        call!(plugin, Release);
        if hr != 0 || obj.is_null() {
            return Err(Error::with_message(
                ErrorKind::NotSupported,
                "QueryInterface(IOUSBDeviceInterface) failed",
            ));
        }
        // SAFETY: we keep the run loop alive as long as this object.
        unsafe { CFRetain(rl) };
        Ok(DeviceRef {
            obj: obj as *mut *mut IOUSBDeviceInterface182,
            has_182,
            rl,
            open_count: Mutex::new(0),
            source: Mutex::new(None),
        })
    }

    /// Synchronous GET_DESCRIPTOR on an unopened device (IOKit allows it).
    fn get_descriptor(&self, value: u16, index: u16, out: &mut [u8]) -> Result<usize> {
        let mut req = IOUSBDevRequestTO {
            bmRequestType: 0x80,
            bRequest: request::GET_DESCRIPTOR,
            wValue: value,
            wIndex: index,
            wLength: out.len().min(u16::MAX as usize) as u16,
            pData: out.as_mut_ptr() as *mut c_void,
            wLenDone: 0,
            noDataTimeout: 1000,
            completionTimeout: 1000,
        };
        let kr = if self.has_182 {
            call!(self.obj, DeviceRequestTO, &mut req)
        } else {
            call!(self.obj, DeviceRequest, &mut req as *mut IOUSBDevRequestTO as *mut IOUSBDevRequest)
        };
        check(kr, "DeviceRequest")?;
        Ok(req.wLenDone as usize)
    }

    /// Opens the device (exclusively) on the first request; later requests
    /// share the open.
    fn open(&self) -> Result<()> {
        let mut count = lock(&self.open_count);
        if *count == 0 {
            let kr = if self.has_182 {
                call!(self.obj, USBDeviceOpenSeize)
            } else {
                call!(self.obj, USBDeviceOpen)
            };
            check(kr, "USBDeviceOpen")?;
        }
        *count += 1;
        Ok(())
    }

    fn close(&self) {
        let mut count = lock(&self.open_count);
        if *count > 0 {
            *count -= 1;
            if *count == 0 {
                call!(self.obj, USBDeviceClose);
            }
        }
    }

    /// Makes sure the device's async completions are delivered to our run
    /// loop.
    fn ensure_event_source(&self) -> Result<()> {
        let mut src = lock(&self.source);
        if src.is_none() {
            let mut s: CFRunLoopSourceRef = std::ptr::null_mut();
            check(
                call!(self.obj, CreateDeviceAsyncEventSource, &mut s),
                "CreateDeviceAsyncEventSource",
            )?;
            // SAFETY: adding a freshly created source to our run loop.
            unsafe { CFRunLoopAddSource(self.rl, s, run_loop_mode()) };
            *src = Some(s);
        }
        Ok(())
    }
}

/// Where a device lives: its shared IOKit object.
pub(crate) struct Location {
    dev: Arc<DeviceRef>,
    /// The `USB Serial Number` registry property, read at enumeration.
    serial: Option<String>,
}

// ----- context & event thread ---------------------------------------------------------------

pub(crate) struct Context {
    rl: CFRunLoopRef,
    handles: Mutex<Vec<Weak<Handle>>>,
    #[cfg(feature = "hotplug")]
    hotplug: Mutex<Option<Hotplug>>,
    /// Device objects by registry entry ID, so that repeated enumeration and
    /// several handles share one exclusive open.
    cache: Mutex<HashMap<u64, Weak<DeviceRef>>>,
}

// SAFETY: the run loop reference is only used through thread-safe CF calls.
unsafe impl Send for Context {}
// SAFETY: as above.
unsafe impl Sync for Context {}

impl Context {
    pub(crate) fn new() -> Result<Arc<Self>> {
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        let ctx_slot: Arc<Mutex<Option<Weak<Context>>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&ctx_slot);
        std::thread::Builder::new()
            .name("rawusb-events".into())
            .spawn(move || event_loop(slot, tx))
            .map_err(|e| Error::from(e).context("spawn event thread"))?;
        let rl = rx
            .recv()
            .map_err(|_| Error::with_message(ErrorKind::Other, "event thread failed to start"))? as CFRunLoopRef;
        // SAFETY: the thread retained it once for itself; this is our own retain.
        unsafe { CFRetain(rl) };
        let ctx = Arc::new(Context {
            rl,
            handles: Mutex::new(Vec::new()),
            #[cfg(feature = "hotplug")]
            hotplug: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
        });
        *lock(&ctx_slot) = Some(Arc::downgrade(&ctx));
        Ok(ctx)
    }

    fn wake(&self) {
        // SAFETY: CFRunLoopStop may be called from any thread.
        unsafe { CFRunLoopStop(self.rl) };
    }

    /// Starts reporting device changes through `notify`.
    ///
    /// IOKit delivers matched/terminated notifications on the run loop this
    /// context already owns, so no extra thread is needed here.
    #[cfg(feature = "hotplug")]
    pub(crate) fn watch_hotplug(self: &Arc<Self>, notify: Notifier) -> Result<()> {
        let mut slot = lock(&self.hotplug);
        if slot.is_some() {
            return Ok(());
        }
        // SAFETY: creating a notification port on the default main port.
        let port = unsafe { IONotificationPortCreate(kIOMasterPortDefault) };
        if port.is_null() {
            return Err(Error::with_message(ErrorKind::Other, "IONotificationPortCreate failed"));
        }
        // SAFETY: the port is live; the source it owns is valid until the
        // port is destroyed, which `Hotplug::drop` does after removing it.
        let source = unsafe { IONotificationPortGetRunLoopSource(port) };
        // SAFETY: CFRunLoop APIs are thread-safe.
        unsafe { CFRunLoopAddSource(self.rl, source, run_loop_mode()) };

        // The callback reaches the notifier through this token.
        let token = install_notifier(notify);
        let mut iterators = Vec::new();
        for class in [c"IOUSBHostDevice", c"IOUSBDevice"] {
            for kind in [kIOMatchedNotification, kIOTerminatedNotification] {
                // SAFETY: a fresh matching dictionary per call, since
                // IOServiceAddMatchingNotification consumes one reference.
                let dict = unsafe { IOServiceMatching(class.as_ptr()) };
                if dict.is_null() {
                    continue;
                }
                let mut iter: io_iterator_t = 0;
                // SAFETY: valid port, type string, dictionary and out-pointer.
                let kr = unsafe {
                    IOServiceAddMatchingNotification(port, kind.as_ptr(), dict, hotplug_callback, token as *mut c_void, &mut iter)
                };
                if kr == kIOReturnSuccess {
                    // Draining the initial set arms the notification.
                    drain_iterator(iter);
                    iterators.push(iter);
                }
            }
        }
        if iterators.is_empty() {
            remove_notifier(token);
            // SAFETY: undoing exactly what was set up above.
            unsafe {
                CFRunLoopRemoveSource(self.rl, source, run_loop_mode());
                IONotificationPortDestroy(port);
            }
            return Err(Error::with_message(ErrorKind::NotSupported, "no USB device class to watch"));
        }
        // SAFETY: the registration keeps its own reference on the run loop,
        // because the context releases its own before this structure is
        // dropped.
        unsafe { CFRetain(self.rl) };
        *slot = Some(Hotplug {
            port,
            source,
            rl: self.rl,
            iterators,
            token,
        });
        drop(slot);
        // Make the event thread notice the new run loop source.
        self.wake();
        Ok(())
    }

    pub(crate) fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        let mut out = Vec::new();
        let mut seen: Vec<u64> = Vec::new();
        for class in [c"IOUSBHostDevice", c"IOUSBDevice"] {
            for service in matching_services(class) {
                let mut id: u64 = 0;
                // SAFETY: valid service and out-pointer.
                unsafe { IORegistryEntryGetRegistryEntryID(service, &mut id) };
                if !seen.contains(&id)
                    && let Some(info) = self.describe(service, id)
                {
                    seen.push(id);
                    out.push(info);
                }
                // SAFETY: releasing the iterator's reference.
                unsafe { IOObjectRelease(service) };
            }
            if !out.is_empty() {
                break;
            }
        }
        out.sort_by_key(|d| (d.bus_number, d.address));
        Ok(out)
    }

    fn device_ref(&self, service: io_service_t, id: u64) -> Result<Arc<DeviceRef>> {
        let mut cache = lock(&self.cache);
        cache.retain(|_, w| w.strong_count() > 0);
        if let Some(d) = cache.get(&id).and_then(|w| w.upgrade()) {
            return Ok(d);
        }
        let d = Arc::new(DeviceRef::from_service(service, self.rl)?);
        cache.insert(id, Arc::downgrade(&d));
        Ok(d)
    }

    fn describe(&self, service: io_service_t, id: u64) -> Option<DeviceInfo> {
        let dev = self.device_ref(service, id).ok()?;
        let mut location: u32 = 0;
        call!(dev.obj, GetLocationID, &mut location);
        let bus_number = (location >> 24) as u8;
        let mut port_numbers = Vec::new();
        let mut shift = 20;
        loop {
            let port = ((location >> shift) & 0xf) as u8;
            if port == 0 {
                break;
            }
            port_numbers.push(port);
            if shift == 0 {
                break;
            }
            shift -= 4;
        }
        let address = prop!(dev.obj, GetDeviceAddress, u16).min(255) as u8;
        let speed = match prop!(dev.obj, GetDeviceSpeed, u8) {
            0 => Speed::Low,
            1 => Speed::Full,
            2 => Speed::High,
            3 => Speed::Super,
            4 => Speed::SuperPlus,
            5 => Speed::SuperPlusX2,
            _ => Speed::Unknown,
        };

        // The full descriptor comes from the device; fall back to what the
        // registry knows if it does not answer.
        let mut raw = [0u8; 18];
        let device_descriptor = match dev.get_descriptor((descriptor_type::DEVICE as u16) << 8, 0, &mut raw) {
            Ok(18) => DeviceDescriptor::from_bytes(&raw).ok()?,
            _ => DeviceDescriptor {
                usb_version: Version(0x0200),
                class: prop!(dev.obj, GetDeviceClass, u8),
                sub_class: prop!(dev.obj, GetDeviceSubClass, u8),
                protocol: prop!(dev.obj, GetDeviceProtocol, u8),
                max_packet_size_0: 64,
                vendor_id: prop!(dev.obj, GetDeviceVendor, u16),
                product_id: prop!(dev.obj, GetDeviceProduct, u16),
                device_version: Version(prop!(dev.obj, GetDeviceReleaseNumber, u16)),
                manufacturer_string_index: if dev.has_182 {
                    prop!(dev.obj, USBGetManufacturerStringIndex, u8)
                } else {
                    0
                },
                product_string_index: if dev.has_182 {
                    prop!(dev.obj, USBGetProductStringIndex, u8)
                } else {
                    0
                },
                serial_number_string_index: if dev.has_182 {
                    prop!(dev.obj, USBGetSerialNumberStringIndex, u8)
                } else {
                    0
                },
                num_configurations: prop!(dev.obj, GetNumberOfConfigurations, u8),
            },
        };

        let mut configs = Vec::new();
        for i in 0..device_descriptor.num_configurations {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            if call!(dev.obj, GetConfigurationDescriptorPtr, i, &mut ptr) != kIOReturnSuccess || ptr.is_null() {
                break;
            }
            // SAFETY: IOKit returns a pointer to a cached configuration
            // descriptor at least wTotalLength bytes long.
            let raw = unsafe {
                let total = u16::from_le_bytes([*ptr.add(2), *ptr.add(3)]) as usize;
                std::slice::from_raw_parts(ptr, total.max(9)).to_vec()
            };
            configs.push(raw);
        }
        let active_config = Some(prop!(dev.obj, GetConfiguration, u8));
        let serial = registry_string(service, c"USB Serial Number");

        Some(DeviceInfo {
            bus_number,
            address,
            port_numbers,
            speed,
            device_descriptor,
            configs,
            active_config,
            location: Location { dev, serial },
            serial_number: Default::default(),
        })
    }

    pub(crate) fn open(self: &Arc<Self>, dev: &Arc<DeviceInfo>) -> Result<Arc<Handle>> {
        let d = &dev.location.dev;
        // Like libusb, tolerate a device another driver holds: descriptor
        // requests still work; claiming interfaces will not.
        let is_open = match d.open() {
            Ok(()) => true,
            Err(e) if e.kind() == ErrorKind::Access => false,
            Err(e) => return Err(e),
        };
        d.ensure_event_source()?;
        let handle = Arc::new(Handle {
            ctx: Arc::clone(self),
            info: Arc::clone(dev),
            is_open,
            claimed: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_frame: Mutex::new(HashMap::new()),
            disconnected: AtomicBool::new(false),
        });
        lock(&self.handles).push(Arc::downgrade(&handle));
        Ok(handle)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.wake();
        // SAFETY: balancing our retain from `new`.
        unsafe { CFRelease(self.rl) };
    }
}

unsafe extern "C" fn keepalive_timer(_timer: CFRunLoopTimerRef, _info: *mut c_void) {}

fn event_loop(slot: Arc<Mutex<Option<Weak<Context>>>>, tx: std::sync::mpsc::Sender<usize>) {
    // SAFETY: standard run-loop setup on this thread.
    let rl = unsafe {
        let rl = CFRunLoopGetCurrent();
        CFRetain(rl);
        // A run loop with no sources returns immediately; park a timer far
        // in the future so it stays alive between our sources coming and going.
        let timer = CFRunLoopTimerCreate(
            std::ptr::null(),
            CFAbsoluteTimeGetCurrent() + 1.0e9,
            1.0e9,
            0,
            0,
            Some(keepalive_timer),
            std::ptr::null_mut(),
        );
        CFRunLoopAddTimer(rl, timer, run_loop_mode());
        CFRelease(timer);
        rl
    };
    if tx.send(rl as usize).is_err() {
        // SAFETY: balancing the retain above.
        unsafe { CFRelease(rl) };
        return;
    }
    // Wait until the context has registered itself.
    let ctx: Weak<Context> = loop {
        if let Some(w) = lock(&slot).clone() {
            break w;
        }
        std::thread::yield_now();
    };
    loop {
        let handles: Vec<Arc<Handle>> = {
            let Some(ctx) = ctx.upgrade() else { break };
            let mut g = lock(&ctx.handles);
            g.retain(|w| w.strong_count() > 0);
            g.iter().filter_map(|w| w.upgrade()).collect()
        };
        let now = Instant::now();
        let secs = match handles.iter().filter_map(|h| h.next_deadline()).min() {
            Some(d) => d.saturating_duration_since(now).as_secs_f64().max(0.001),
            None => 3600.0,
        };
        // SAFETY: running the current thread's run loop.
        unsafe { CFRunLoopRunInMode(run_loop_mode(), secs, 0) };
        let now = Instant::now();
        for h in &handles {
            h.expire(now);
        }
    }
    // SAFETY: balancing the retain above.
    unsafe { CFRelease(rl) };
}

/// Releases everything an iterator still holds, which also re-arms the
/// matching notification it belongs to.
#[cfg(feature = "hotplug")]
fn drain_iterator(iterator: io_iterator_t) {
    loop {
        // SAFETY: valid iterator; each service returned carries a reference
        // that we hand straight back.
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            break;
        }
        // SAFETY: as above.
        unsafe { IOObjectRelease(service) };
    }
}

/// Notifiers that IOKit callbacks can reach, keyed by an opaque token.
///
/// The callback is handed a token rather than a pointer to the notifier.
/// IOKit gives no way to wait for a callback that is already running on the
/// run loop thread, so a box freed while tearing a context down could be read
/// after it was released; looking a token up under a lock turns that race
/// into a lookup that simply finds nothing.
#[cfg(feature = "hotplug")]
static NOTIFIERS: Mutex<Vec<(usize, Notifier)>> = Mutex::new(Vec::new());

#[cfg(feature = "hotplug")]
static NEXT_NOTIFIER_TOKEN: AtomicUsize = AtomicUsize::new(1);

#[cfg(feature = "hotplug")]
fn install_notifier(notify: Notifier) -> usize {
    let token = NEXT_NOTIFIER_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&NOTIFIERS).push((token, notify));
    token
}

#[cfg(feature = "hotplug")]
fn remove_notifier(token: usize) {
    lock(&NOTIFIERS).retain(|(installed, _)| *installed != token);
}

#[cfg(feature = "hotplug")]
fn find_notifier(token: usize) -> Option<Notifier> {
    lock(&NOTIFIERS)
        .iter()
        .find(|(installed, _)| *installed == token)
        .map(|(_, notify)| Arc::clone(notify))
}

/// Runs on the context's run loop thread whenever IOKit matches or terminates
/// a USB device.
#[cfg(feature = "hotplug")]
unsafe extern "C" fn hotplug_callback(refcon: *mut c_void, iterator: io_iterator_t) {
    drain_iterator(iterator);
    // A token whose registration is gone means the context was torn down
    // while this callback was being delivered; there is nothing to report.
    if let Some(notify) = find_notifier(refcon as usize) {
        notify();
    }
}

/// IOKit notification registration owned by a context.
#[cfg(feature = "hotplug")]
struct Hotplug {
    port: IONotificationPortRef,
    source: CFRunLoopSourceRef,
    /// Retained here in its own right: the context releases its reference in
    /// `Context::drop`, whose body runs before this field is dropped.
    rl: CFRunLoopRef,
    iterators: Vec<io_iterator_t>,
    token: usize,
}

// SAFETY: the IOKit handles below are only touched through thread-safe calls.
#[cfg(feature = "hotplug")]
unsafe impl Send for Hotplug {}
// SAFETY: as above.
#[cfg(feature = "hotplug")]
unsafe impl Sync for Hotplug {}

#[cfg(feature = "hotplug")]
impl Drop for Hotplug {
    fn drop(&mut self) {
        // Take the notifier out of reach first: a callback already running on
        // the run loop thread holds its own clone and finishes harmlessly,
        // and no later one can find anything.
        remove_notifier(self.token);
        // SAFETY: tearing down exactly what `watch_hotplug` created, in the
        // reverse order, including this structure's own reference on the run
        // loop.
        unsafe {
            CFRunLoopRemoveSource(self.rl, self.source, run_loop_mode());
            IONotificationPortDestroy(self.port);
            for iterator in &self.iterators {
                IOObjectRelease(*iterator);
            }
            CFRelease(self.rl);
        }
    }
}

/// Reads a string property from a registry entry.
fn registry_string(entry: io_registry_entry_t, key: &CStr) -> Option<String> {
    // SAFETY: valid NUL-terminated key; every object created here is
    // released before returning.
    unsafe {
        let key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), kCFStringEncodingUTF8);
        if key.is_null() {
            return None;
        }
        let value = IORegistryEntryCreateCFProperty(entry, key, std::ptr::null(), 0);
        CFRelease(key);
        if value.is_null() {
            return None;
        }
        let mut out = None;
        if CFGetTypeID(value) == CFStringGetTypeID() {
            let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), kCFStringEncodingUTF8) + 1;
            let mut buf = vec![0u8; size.max(1) as usize];
            if CFStringGetCString(value, buf.as_mut_ptr() as *mut _, buf.len() as CFIndex, kCFStringEncodingUTF8) != 0 {
                out = CStr::from_bytes_until_nul(&buf).ok().map(|s| s.to_string_lossy().into_owned());
            }
        }
        CFRelease(value);
        out
    }
}

/// The serial number IOKit read when the device arrived.
pub(crate) fn read_serial_number(info: &DeviceInfo) -> Option<String> {
    info.location.serial.clone()
}

fn matching_services(class: &CStr) -> Vec<io_service_t> {
    let mut out = Vec::new();
    // SAFETY: the matching dictionary is consumed by
    // IOServiceGetMatchingServices; the iterator is released below.
    unsafe {
        let dict = IOServiceMatching(class.as_ptr());
        if dict.is_null() {
            return out;
        }
        let mut iter: io_iterator_t = 0;
        if IOServiceGetMatchingServices(kIOMasterPortDefault, dict, &mut iter) != kIOReturnSuccess {
            return out;
        }
        loop {
            let s = IOIteratorNext(iter);
            if s == 0 {
                break;
            }
            out.push(s);
        }
        IOObjectRelease(iter);
    }
    out
}

// ----- interfaces ------------------------------------------------------------------------------

struct Pipe {
    address: u8,
    pipe_ref: u8,
    interval: u8,
}

struct ClaimedIface {
    obj: *mut *mut IOUSBInterfaceInterface182,
    has_182: bool,
    rl: CFRunLoopRef,
    source: CFRunLoopSourceRef,
    pipes: Vec<Pipe>,
}

// SAFETY: see `DeviceRef`.
unsafe impl Send for ClaimedIface {}

impl Drop for ClaimedIface {
    fn drop(&mut self) {
        call!(self.obj, USBInterfaceClose);
        if !self.source.is_null() {
            // SAFETY: removing and releasing the source we added.
            unsafe {
                CFRunLoopRemoveSource(self.rl, self.source, run_loop_mode());
                CFRelease(self.source);
            }
        }
        call!(self.obj, Release);
    }
}

impl ClaimedIface {
    fn refresh_pipes(&mut self) {
        self.pipes.clear();
        let mut n: u8 = 0;
        call!(self.obj, GetNumEndpoints, &mut n);
        for pipe_ref in 1..=n {
            let (mut dir, mut num, mut ty, mut max, mut interval) = (0u8, 0u8, 0u8, 0u16, 0u8);
            if call!(
                self.obj,
                GetPipeProperties,
                pipe_ref,
                &mut dir,
                &mut num,
                &mut ty,
                &mut max,
                &mut interval
            ) != kIOReturnSuccess
            {
                continue;
            }
            self.pipes.push(Pipe {
                address: (num & 0x0f) | if dir == kUSBIn { 0x80 } else { 0 },
                pipe_ref,
                interval,
            });
        }
    }

    fn pipe(&self, address: u8) -> Option<&Pipe> {
        self.pipes.iter().find(|p| p.address == address)
    }
}

/// Iterates the device's interface services, returning the object for the
/// requested interface number (opened or not) and its service.
fn find_interface(dev: &DeviceRef, number: u8) -> Result<Option<(io_service_t, *mut *mut IOUSBInterfaceInterface182, bool)>> {
    let mut req = IOUSBFindInterfaceRequest {
        bInterfaceClass: kIOUSBFindInterfaceDontCare,
        bInterfaceSubClass: kIOUSBFindInterfaceDontCare,
        bInterfaceProtocol: kIOUSBFindInterfaceDontCare,
        bAlternateSetting: kIOUSBFindInterfaceDontCare,
    };
    let mut iter: io_iterator_t = 0;
    check(
        call!(dev.obj, CreateInterfaceIterator, &mut req, &mut iter),
        "CreateInterfaceIterator",
    )?;
    let mut found = None;
    loop {
        // SAFETY: valid iterator; services are released unless returned.
        let service = unsafe { IOIteratorNext(iter) };
        if service == 0 {
            break;
        }
        let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
        let mut score: i32 = 0;
        // SAFETY: valid service and out-pointers.
        let kr = unsafe {
            IOCreatePlugInInterfaceForService(
                service,
                uuid_ref(&kIOUSBInterfaceUserClientTypeID),
                uuid_ref(&kIOCFPlugInInterfaceID),
                &mut plugin,
                &mut score,
            )
        };
        if kr != kIOReturnSuccess || plugin.is_null() {
            // SAFETY: releasing the iterator's reference.
            unsafe { IOObjectRelease(service) };
            continue;
        }
        let mut obj: *mut c_void = std::ptr::null_mut();
        let mut has_182 = true;
        let mut hr = call!(plugin, QueryInterface, kIOUSBInterfaceInterfaceID182, &mut obj);
        if hr != 0 || obj.is_null() {
            has_182 = false;
            hr = call!(plugin, QueryInterface, kIOUSBInterfaceInterfaceID, &mut obj);
        }
        call!(plugin, Release);
        if hr != 0 || obj.is_null() {
            // SAFETY: as above.
            unsafe { IOObjectRelease(service) };
            continue;
        }
        let obj = obj as *mut *mut IOUSBInterfaceInterface182;
        let mut n: u8 = 0;
        call!(obj, GetInterfaceNumber, &mut n);
        if n == number {
            found = Some((service, obj, has_182));
            break;
        }
        call!(obj, Release);
        // SAFETY: as above.
        unsafe { IOObjectRelease(service) };
    }
    // SAFETY: releasing the iterator.
    unsafe { IOObjectRelease(iter) };
    Ok(found)
}

// ----- handle ----------------------------------------------------------------------------------

pub(crate) struct Handle {
    ctx: Arc<Context>,
    info: Arc<DeviceInfo>,
    is_open: bool,
    claimed: Mutex<HashMap<u8, ClaimedIface>>,
    pending: Mutex<HashMap<usize, Arc<Inner>>>,
    /// Next bus frame to schedule for each isochronous endpoint.
    next_frame: Mutex<HashMap<u8, u64>>,
    disconnected: AtomicBool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.claimed.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        if self.is_open {
            self.info.location.dev.close();
        }
        self.ctx.wake();
    }
}

/// Heap block that outlives an asynchronous request.
struct Entry {
    inner: Arc<Inner>,
    req: IOUSBDevRequestTO,
    frames: Vec<IOUSBIsocFrame>,
}

unsafe extern "C" fn completion(refcon: *mut c_void, result: IOReturn, arg0: *mut c_void) {
    // SAFETY: `refcon` is the leaked `Box<Entry>` of exactly one request,
    // recovered exactly once here.
    let entry = unsafe { Box::from_raw(refcon as *mut Entry) };
    let inner = Arc::clone(&entry.inner);
    inner.handle.finish(&inner, refcon as usize, result, arg0 as usize, &entry.frames);
    drop(entry);
}

impl Handle {
    fn dev(&self) -> &DeviceRef {
        &self.info.location.dev
    }

    fn require_open(&self) -> Result<()> {
        if self.is_open {
            Ok(())
        } else {
            Err(Error::with_message(
                ErrorKind::Access,
                "device is held by another driver or process",
            ))
        }
    }

    pub(crate) fn active_configuration(&self) -> Result<Option<u8>> {
        let mut v = 0u8;
        check(call!(self.dev().obj, GetConfiguration, &mut v), "GetConfiguration")?;
        Ok(Some(v))
    }

    pub(crate) fn set_configuration(&self, value: u8) -> Result<()> {
        self.require_open()?;
        check(call!(self.dev().obj, SetConfiguration, value), "SetConfiguration")
    }

    pub(crate) fn claim_interface(&self, interface: u8) -> Result<()> {
        let mut claimed = lock(&self.claimed);
        if claimed.contains_key(&interface) {
            return Ok(());
        }
        let Some((service, obj, has_182)) = find_interface(self.dev(), interface)? else {
            return Err(Error::with_message(ErrorKind::NotFound, "no such interface"));
        };
        // SAFETY: we only needed the service to find the interface.
        unsafe { IOObjectRelease(service) };
        let kr = call!(obj, USBInterfaceOpen);
        if kr != kIOReturnSuccess {
            call!(obj, Release);
            return Err(io_error(kr, "USBInterfaceOpen"));
        }
        let mut source: CFRunLoopSourceRef = std::ptr::null_mut();
        let kr = call!(obj, CreateInterfaceAsyncEventSource, &mut source);
        if kr != kIOReturnSuccess {
            call!(obj, USBInterfaceClose);
            call!(obj, Release);
            return Err(io_error(kr, "CreateInterfaceAsyncEventSource"));
        }
        let rl = self.dev().rl;
        // SAFETY: adding our new source to the event thread's run loop.
        unsafe { CFRunLoopAddSource(rl, source, run_loop_mode()) };
        let mut ci = ClaimedIface {
            obj,
            has_182,
            rl,
            source,
            pipes: Vec::new(),
        };
        ci.refresh_pipes();
        claimed.insert(interface, ci);
        Ok(())
    }

    pub(crate) fn release_interface(&self, interface: u8) -> Result<()> {
        match lock(&self.claimed).remove(&interface) {
            Some(_) => Ok(()),
            None => Err(Error::with_message(ErrorKind::NotFound, "interface not claimed")),
        }
    }

    pub(crate) fn set_alt_setting(&self, interface: u8, alt: u8) -> Result<()> {
        let mut claimed = lock(&self.claimed);
        let ci = claimed
            .get_mut(&interface)
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "interface not claimed"))?;
        check(call!(ci.obj, SetAlternateInterface, alt), "SetAlternateInterface")?;
        ci.refresh_pipes();
        Ok(())
    }

    fn with_pipe<T>(&self, endpoint: u8, f: impl FnOnce(&ClaimedIface, &Pipe) -> T) -> Result<T> {
        let claimed = lock(&self.claimed);
        for ci in claimed.values() {
            if let Some(p) = ci.pipe(endpoint) {
                return Ok(f(ci, p));
            }
        }
        Err(Error::with_message(
            ErrorKind::NotFound,
            "endpoint does not belong to a claimed interface",
        ))
    }

    pub(crate) fn clear_halt(&self, endpoint: u8) -> Result<()> {
        self.with_pipe(endpoint, |ci, p| check(call!(ci.obj, ClearPipeStall, p.pipe_ref), "ClearPipeStall"))?
    }

    pub(crate) fn reset(&self) -> Result<()> {
        self.require_open()?;
        check(call!(self.dev().obj, ResetDevice), "ResetDevice")
    }

    pub(crate) fn kernel_driver_active(&self, interface: u8) -> Result<bool> {
        let Some((service, obj, _)) = find_interface(self.dev(), interface)? else {
            return Err(Error::with_message(ErrorKind::NotFound, "no such interface"));
        };
        call!(obj, Release);
        let mut child: io_registry_entry_t = 0;
        // SAFETY: valid service; a child in the service plane is a driver.
        let kr = unsafe { IORegistryEntryGetChildEntry(service, c"IOService".as_ptr(), &mut child) };
        // SAFETY: releasing what we obtained.
        unsafe {
            if kr == kIOReturnSuccess && child != 0 {
                IOObjectRelease(child);
            }
            IOObjectRelease(service);
        }
        Ok(kr == kIOReturnSuccess && child != 0)
    }

    pub(crate) fn detach_kernel_driver(&self, _interface: u8) -> Result<()> {
        Err(Error::with_message(
            ErrorKind::NotSupported,
            "kernel drivers cannot be detached on macOS",
        ))
    }

    pub(crate) fn attach_kernel_driver(&self, _interface: u8) -> Result<()> {
        Err(Error::with_message(
            ErrorKind::NotSupported,
            "kernel drivers cannot be attached on macOS",
        ))
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
        let buf_ptr = st.buffer.as_mut_ptr();
        let buf_len = st.buffer.len();
        if buf_len > u32::MAX as usize {
            return Err(Error::with_message(ErrorKind::InvalidParam, "buffer too large"));
        }
        let timeout_ms = st.timeout.as_millis().min(u32::MAX as u128) as u32;

        let mut entry = Box::new(Entry {
            inner: Arc::clone(inner),
            // SAFETY: plain POD.
            req: unsafe { std::mem::zeroed() },
            frames: Vec::new(),
        });
        let mut kernel_timeout = false;
        let mut pipe_target: Option<(usize, u8)> = None;

        let kr = match inner.kind {
            TransferType::Control => {
                let setup = ControlSetup::from_bytes(st.buffer[..8].try_into().expect("checked by caller"));
                let data_len = (buf_len - 8).min(setup.length as usize);
                entry.req = IOUSBDevRequestTO {
                    bmRequestType: setup.request_type,
                    bRequest: setup.request,
                    wValue: setup.value,
                    wIndex: setup.index,
                    wLength: data_len as u16,
                    // SAFETY: offset 8 is within the buffer (length checked by caller).
                    pData: unsafe { buf_ptr.add(8) } as *mut c_void,
                    wLenDone: 0,
                    noDataTimeout: timeout_ms,
                    completionTimeout: timeout_ms,
                };
                let refcon = &mut *entry as *mut Entry as *mut c_void;
                let dev = self.dev();
                if dev.has_182 {
                    kernel_timeout = true;
                    call!(
                        dev.obj,
                        DeviceRequestAsyncTO,
                        &mut entry.req,
                        completion as IOAsyncCallback1,
                        refcon
                    )
                } else {
                    call!(
                        dev.obj,
                        DeviceRequestAsync,
                        &mut entry.req as *mut IOUSBDevRequestTO as *mut IOUSBDevRequest,
                        completion as IOAsyncCallback1,
                        refcon
                    )
                }
            }
            TransferType::Bulk | TransferType::Interrupt => {
                let refcon = &mut *entry as *mut Entry as *mut c_void;
                let is_in = inner.endpoint & 0x80 != 0;
                let is_bulk = inner.kind == TransferType::Bulk;
                self.with_pipe(inner.endpoint, |ci, p| {
                    pipe_target = Some((ci.obj as usize, p.pipe_ref));
                    let obj = ci.obj;
                    // The timeout-taking calls reject interrupt pipes.
                    if ci.has_182 && is_bulk {
                        kernel_timeout = true;
                        if is_in {
                            call!(
                                obj,
                                ReadPipeAsyncTO,
                                p.pipe_ref,
                                buf_ptr as *mut c_void,
                                buf_len as u32,
                                timeout_ms,
                                timeout_ms,
                                completion as IOAsyncCallback1,
                                refcon
                            )
                        } else {
                            call!(
                                obj,
                                WritePipeAsyncTO,
                                p.pipe_ref,
                                buf_ptr as *mut c_void,
                                buf_len as u32,
                                timeout_ms,
                                timeout_ms,
                                completion as IOAsyncCallback1,
                                refcon
                            )
                        }
                    } else if is_in {
                        call!(
                            obj,
                            ReadPipeAsync,
                            p.pipe_ref,
                            buf_ptr as *mut c_void,
                            buf_len as u32,
                            completion as IOAsyncCallback1,
                            refcon
                        )
                    } else {
                        call!(
                            obj,
                            WritePipeAsync,
                            p.pipe_ref,
                            buf_ptr as *mut c_void,
                            buf_len as u32,
                            completion as IOAsyncCallback1,
                            refcon
                        )
                    }
                })?
            }
            TransferType::Isochronous => {
                entry.frames = st
                    .iso_packets
                    .iter()
                    .map(|p| IOUSBIsocFrame {
                        frStatus: 0,
                        frReqCount: p.length.min(u16::MAX as u32) as u16,
                        frActCount: 0,
                    })
                    .collect();
                let num = entry.frames.len() as u32;
                let refcon = &mut *entry as *mut Entry as *mut c_void;
                let frames_ptr = entry.frames.as_mut_ptr();
                let is_in = inner.endpoint & 0x80 != 0;
                let speed = self.info.speed;
                let endpoint = inner.endpoint;
                let next_frame = &self.next_frame;
                self.with_pipe(inner.endpoint, |ci, p| {
                    pipe_target = Some((ci.obj as usize, p.pipe_ref));
                    let mut frame: u64 = 0;
                    let mut at = AbsoluteTime { lo: 0, hi: 0 };
                    call!(ci.obj, GetBusFrameNumber, &mut frame, &mut at);
                    let mut nf = lock(next_frame);
                    let start = (*nf.get(&endpoint).unwrap_or(&0)).max(frame + 4);
                    // Packets per millisecond frame: 1 at full speed, 8 >> (interval-1) at high speed and above.
                    let per_frame = if speed >= Speed::High {
                        (8u32 >> p.interval.clamp(1, 4).saturating_sub(1)).max(1)
                    } else {
                        1
                    };
                    nf.insert(endpoint, start + (num.div_ceil(per_frame)) as u64);
                    drop(nf);
                    if is_in {
                        call!(
                            ci.obj,
                            ReadIsochPipeAsync,
                            p.pipe_ref,
                            buf_ptr as *mut c_void,
                            start,
                            num,
                            frames_ptr,
                            completion as IOAsyncCallback1,
                            refcon
                        )
                    } else {
                        call!(
                            ci.obj,
                            WriteIsochPipeAsync,
                            p.pipe_ref,
                            buf_ptr as *mut c_void,
                            start,
                            num,
                            frames_ptr,
                            completion as IOAsyncCallback1,
                            refcon
                        )
                    }
                })?
            }
        };

        if kr != kIOReturnSuccess {
            return Err(io_error(kr, "submit transfer"));
        }
        let key = &*entry as *const Entry as usize;
        std::mem::forget(entry);
        lock(&self.pending).insert(key, Arc::clone(inner));
        *sub = Some(MacSub {
            pipe: pipe_target,
            deadline: (!st.timeout.is_zero() && !kernel_timeout).then(|| Instant::now() + st.timeout),
            cancelled: false,
            timed_out: false,
        });
        drop(sub);
        self.ctx.wake();
        Ok(())
    }

    fn abort(&self, sub: &MacSub) -> Result<()> {
        match sub.pipe {
            Some((obj, pipe_ref)) => {
                let obj = obj as *mut *mut IOUSBInterfaceInterface182;
                check(call!(obj, AbortPipe, pipe_ref), "AbortPipe")
            }
            None => {
                let dev = self.dev();
                if dev.has_182 {
                    check(call!(dev.obj, USBDeviceAbortPipeZero), "USBDeviceAbortPipeZero")
                } else {
                    Err(Error::with_message(
                        ErrorKind::NotSupported,
                        "cannot abort control requests on this IOKit revision",
                    ))
                }
            }
        }
    }

    pub(crate) fn cancel(&self, inner: &Arc<Inner>) -> Result<()> {
        let mut g = lock(&inner.sys.sub);
        let Some(sub) = g.as_mut() else {
            return Err(Error::with_message(ErrorKind::NotFound, "transfer is not in flight"));
        };
        self.abort(sub)?;
        sub.cancelled = true;
        Ok(())
    }

    pub(crate) fn cancel_all(&self) {
        let transfers: Vec<Arc<Inner>> = lock(&self.pending).values().cloned().collect();
        for t in transfers {
            let _ = self.cancel(&t);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let pending = lock(&self.pending);
        pending
            .values()
            .filter_map(|t| lock(&t.sys.sub).as_ref().and_then(|s| s.deadline))
            .min()
    }

    fn expire(&self, now: Instant) {
        let transfers: Vec<Arc<Inner>> = lock(&self.pending).values().cloned().collect();
        for t in transfers {
            let mut g = lock(&t.sys.sub);
            let Some(sub) = g.as_mut() else { continue };
            if sub.timed_out || sub.deadline.is_none_or(|d| d > now) {
                continue;
            }
            if self.abort(sub).is_ok() {
                sub.timed_out = true;
            }
            sub.deadline = None;
        }
    }

    fn finish(&self, inner: &Arc<Inner>, key: usize, result: IOReturn, arg0: usize, frames: &[IOUSBIsocFrame]) {
        lock(&self.pending).remove(&key);
        let Some(sub) = lock(&inner.sys.sub).take() else { return };
        let mapped = match result {
            kIOReturnSuccess | kIOReturnUnderrun => TransferStatus::Completed,
            kIOReturnAborted => TransferStatus::Cancelled,
            kIOReturnTimeout | kIOUSBTransactionTimeout => TransferStatus::TimedOut,
            kIOUSBPipeStalled => TransferStatus::Stall,
            kIOReturnNoDevice | kIOReturnNotAttached | kIOReturnNotResponding => {
                self.disconnected.store(true, Ordering::Relaxed);
                TransferStatus::NoDevice
            }
            kIOReturnOverrun => TransferStatus::Overflow,
            _ => TransferStatus::Error,
        };
        let status = if sub.timed_out && mapped == TransferStatus::Cancelled {
            TransferStatus::TimedOut
        } else if sub.cancelled && mapped == TransferStatus::Cancelled {
            TransferStatus::Cancelled
        } else {
            mapped
        };
        if inner.kind == TransferType::Isochronous {
            let packets: Vec<IsoPacket> = frames
                .iter()
                .map(|f| IsoPacket {
                    length: f.frReqCount as u32,
                    actual_length: f.frActCount as u32,
                    status: match f.frStatus {
                        kIOReturnSuccess | kIOReturnUnderrun => TransferStatus::Completed,
                        kIOReturnAborted => TransferStatus::Cancelled,
                        kIOUSBPipeStalled => TransferStatus::Stall,
                        kIOReturnOverrun => TransferStatus::Overflow,
                        kIOReturnNoDevice | kIOReturnNotAttached => TransferStatus::NoDevice,
                        _ => TransferStatus::Error,
                    },
                })
                .collect();
            let actual = packets.iter().map(|p| p.actual_length as usize).sum();
            inner.complete(status, actual, Some(packets));
        } else {
            inner.complete(status, arg0, None);
        }
    }
}

// ----- per-transfer state ------------------------------------------------------------------------

struct MacSub {
    /// Interface object and pipe reference for aborting, `None` for control.
    pipe: Option<(usize, u8)>,
    deadline: Option<Instant>,
    cancelled: bool,
    timed_out: bool,
}

/// Backend state stored inside every transfer.
#[derive(Default)]
pub(crate) struct TransferData {
    sub: Mutex<Option<MacSub>>,
}
