//! Hotplug notifications: learning when devices are plugged in or unplugged.
//!
//! This module is behind the `hotplug` cargo feature. Enable it with:
//!
//! ```toml
//! rawusb = { version = "0.1", features = ["hotplug"] }
//! ```
//!
//! Start from [`Context::hotplug`](crate::Context::hotplug), narrow the events
//! you care about, then either take a [`HotplugWatcher`] and pull events from
//! it, or [`register`](HotplugBuilder::register) a callback:
//!
//! ```no_run
//! # let ctx = rawusb::Context::new()?;
//! let watcher = ctx.hotplug().vendor_id(0x046d).watch()?;
//! while let Ok(event) = watcher.recv() {
//!     match event {
//!         rawusb::HotplugEvent::Arrived(dev) => println!("plugged in: {dev:?}"),
//!         rawusb::HotplugEvent::Left(dev) => println!("unplugged: {dev:?}"),
//!     }
//! }
//! # Ok::<(), rawusb::Error>(())
//! ```
//!
//! # How it works
//!
//! The first watcher registered on a context starts one background thread and
//! asks the operating system to report device changes: netlink uevents on
//! Linux, `CM_Register_Notification` on Windows, IOKit matching notifications
//! on macOS. Every notification makes that thread re-enumerate and compare
//! against the previous list, so events are derived from a real device list
//! rather than trusted from the OS message. Bursts are coalesced, and
//! callbacks never run on the transfer-completion thread.
//!
//! # Caveats
//!
//! - A device is identified by its port chain, address and vendor/product
//!   pair. Replacing a device with an identical one on the same port between
//!   two scans can go unnoticed.
//! - On Linux the notification arrives when the kernel creates the device,
//!   which can be *before* udev has applied permissions to
//!   `/dev/bus/usb/...`. Opening a just-arrived device may fail with
//!   [`ErrorKind::Access`] for a few milliseconds;
//!   retry if that matters to you.
//! - A callback registered with [`register`](HotplugBuilder::register) is
//!   owned by the context. Storing the [`Device`] it receives inside that
//!   callback keeps the context alive for as long as the registration lives.

use crate::context::{Context, ContextInner};
use crate::device::Device;
use crate::sys;
use crate::{Error, ErrorKind, Result};
use std::fmt;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// How long to wait after a notification before scanning, so that a burst
/// (a hub with several devices behind it) turns into a single rescan.
const COALESCE: Duration = Duration::from_millis(50);

/// A change in the set of attached devices.
#[derive(Debug, Clone)]
pub enum HotplugEvent {
    /// The device was plugged in (or was already present, for a watcher built
    /// with [`enumerate_existing`](HotplugBuilder::enumerate_existing)).
    Arrived(Device),
    /// The device was unplugged. Its descriptors are the ones read while it
    /// was still present; it can no longer be opened.
    Left(Device),
}

impl HotplugEvent {
    /// The device the event is about.
    pub fn device(&self) -> &Device {
        match self {
            HotplugEvent::Arrived(d) | HotplugEvent::Left(d) => d,
        }
    }

    /// Consumes the event and returns its device.
    pub fn into_device(self) -> Device {
        match self {
            HotplugEvent::Arrived(d) | HotplugEvent::Left(d) => d,
        }
    }

    /// `true` for [`HotplugEvent::Arrived`].
    pub fn is_arrival(&self) -> bool {
        matches!(self, HotplugEvent::Arrived(_))
    }

    /// `true` for [`HotplugEvent::Left`].
    pub fn is_departure(&self) -> bool {
        matches!(self, HotplugEvent::Left(_))
    }
}

/// Which devices a watcher wants to hear about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Filter {
    vendor_id: Option<u16>,
    product_id: Option<u16>,
    class: Option<u8>,
}

impl Filter {
    fn matches(&self, dev: &Device) -> bool {
        let desc = dev.device_descriptor();
        if self.vendor_id.is_some_and(|v| v != desc.vendor_id) {
            return false;
        }
        if self.product_id.is_some_and(|p| p != desc.product_id) {
            return false;
        }
        if let Some(class) = self.class
            && desc.class != class
        {
            // A composite device declares its class per interface, so look
            // there too before giving up.
            let matches_interface = dev
                .active_config_descriptor()
                .map(|cfg| cfg.all_alt_settings().any(|alt| alt.class == class))
                .unwrap_or(false);
            if !matches_interface {
                return false;
            }
        }
        true
    }
}

/// Describes a hotplug watcher before it is started.
///
/// Created by [`Context::hotplug`](crate::Context::hotplug). With no filter
/// set, every device change is reported.
#[derive(Debug)]
pub struct HotplugBuilder<'a> {
    ctx: &'a Context,
    filter: Filter,
    enumerate_existing: bool,
}

impl<'a> HotplugBuilder<'a> {
    pub(crate) fn new(ctx: &'a Context) -> Self {
        HotplugBuilder {
            ctx,
            filter: Filter::default(),
            enumerate_existing: false,
        }
    }

    /// Only report devices with this `idVendor`.
    pub fn vendor_id(mut self, vendor_id: u16) -> Self {
        self.filter.vendor_id = Some(vendor_id);
        self
    }

    /// Only report devices with this `idProduct`.
    pub fn product_id(mut self, product_id: u16) -> Self {
        self.filter.product_id = Some(product_id);
        self
    }

    /// Only report devices whose `bDeviceClass`, or any interface class of
    /// whose active configuration, equals `class`.
    pub fn class(mut self, class: u8) -> Self {
        self.filter.class = Some(class);
        self
    }

    /// Also deliver an [`Arrived`](HotplugEvent::Arrived) event for every
    /// matching device that is already attached when the watcher starts.
    ///
    /// This is the race-free way to say "work with every matching device, now
    /// and later": without it, devices plugged in between your own
    /// enumeration and the watcher starting would be missed.
    ///
    /// These events are delivered before the watcher is handed back, so a
    /// callback sees them on the registering thread and a
    /// [`HotplugWatcher`] already has them queued.
    pub fn enumerate_existing(mut self, yes: bool) -> Self {
        self.enumerate_existing = yes;
        self
    }

    /// Registers a callback, called once per matching event until the
    /// returned [`HotplugRegistration`] is dropped.
    ///
    /// The callback runs on the context's hotplug thread, never on the
    /// transfer-completion thread, so it may take its time; it must not be
    /// blocked on something that itself waits for hotplug events. It may drop
    /// watchers, including its own registration, but it must not start a new
    /// one: watchers are started and torn down against the same lock that
    /// serialises dispatch.
    ///
    /// The one exception is
    /// [`enumerate_existing`](Self::enumerate_existing): that initial batch
    /// is delivered on the calling thread, before this function returns.
    pub fn register<F>(self, callback: F) -> Result<HotplugRegistration>
    where
        F: FnMut(&HotplugEvent) + Send + 'static,
    {
        Registry::add(self.ctx, self.filter, self.enumerate_existing, Box::new(callback))
    }

    /// Starts a watcher that queues events for you to pull out with
    /// [`recv`](HotplugWatcher::recv) and friends.
    pub fn watch(self) -> Result<HotplugWatcher> {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let registration = Registry::add(
            self.ctx,
            self.filter,
            self.enumerate_existing,
            Box::new(move |event: &HotplugEvent| {
                // Only the identity travels through the channel: a queued
                // `Device` would hold a `Context` and keep the session (and
                // this very registry) alive forever.
                let raw = match event {
                    HotplugEvent::Arrived(d) => RawEvent::Arrived(Arc::clone(d.info())),
                    HotplugEvent::Left(d) => RawEvent::Left(Arc::clone(d.info())),
                };
                let _ = tx.send(raw);
            }),
        )?;
        Ok(HotplugWatcher { ctx, rx, registration })
    }
}

/// A live hotplug callback registration. Dropping it stops the callbacks.
pub struct HotplugRegistration {
    ctx: Weak<ContextInner>,
    id: u64,
}

impl fmt::Debug for HotplugRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotplugRegistration").field("id", &self.id).finish()
    }
}

impl HotplugRegistration {
    /// Stops the callbacks. Identical to dropping the registration, but says
    /// so at the call site.
    pub fn unregister(self) {}
}

impl Drop for HotplugRegistration {
    fn drop(&mut self) {
        if let Some(inner) = self.ctx.upgrade() {
            inner.hotplug.remove(self.id);
        }
    }
}

/// What travels through a [`HotplugWatcher`]'s queue.
enum RawEvent {
    Arrived(Arc<sys::DeviceInfo>),
    Left(Arc<sys::DeviceInfo>),
}

/// A started watcher holding a queue of events.
///
/// Dropping it deregisters the watcher; events already queued are lost.
pub struct HotplugWatcher {
    ctx: Context,
    rx: Receiver<RawEvent>,
    registration: HotplugRegistration,
}

impl fmt::Debug for HotplugWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotplugWatcher").field("registration", &self.registration).finish()
    }
}

impl HotplugWatcher {
    fn convert(&self, raw: RawEvent) -> HotplugEvent {
        match raw {
            RawEvent::Arrived(info) => HotplugEvent::Arrived(Device::new(self.ctx.clone(), info)),
            RawEvent::Left(info) => HotplugEvent::Left(Device::new(self.ctx.clone(), info)),
        }
    }

    /// Blocks until the next event.
    ///
    /// Only fails if the session is shutting down, which cannot happen while
    /// you hold this watcher.
    pub fn recv(&self) -> Result<HotplugEvent> {
        match self.rx.recv() {
            Ok(raw) => Ok(self.convert(raw)),
            Err(_) => Err(Error::with_message(ErrorKind::Other, "hotplug watcher disconnected")),
        }
    }

    /// Returns the next event, or `None` if none is queued.
    pub fn try_recv(&self) -> Result<Option<HotplugEvent>> {
        match self.rx.try_recv() {
            Ok(raw) => Ok(Some(self.convert(raw))),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Error::with_message(ErrorKind::Other, "hotplug watcher disconnected")),
        }
    }

    /// Waits up to `timeout` for the next event, returning `None` if it
    /// elapses first.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<HotplugEvent>> {
        match self.rx.recv_timeout(timeout) {
            Ok(raw) => Ok(Some(self.convert(raw))),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Error::with_message(ErrorKind::Other, "hotplug watcher disconnected")),
        }
    }

    /// A blocking iterator over the events, ending when the session does.
    pub fn iter(&self) -> HotplugIter<'_> {
        HotplugIter { watcher: self }
    }

    /// The underlying registration.
    pub fn registration(&self) -> &HotplugRegistration {
        &self.registration
    }
}

/// Blocking iterator returned by [`HotplugWatcher::iter`].
#[derive(Debug)]
pub struct HotplugIter<'a> {
    watcher: &'a HotplugWatcher,
}

impl Iterator for HotplugIter<'_> {
    type Item = HotplugEvent;

    fn next(&mut self) -> Option<HotplugEvent> {
        self.watcher.recv().ok()
    }
}

// ----- the registry ------------------------------------------------------------

type Callback = Box<dyn FnMut(&HotplugEvent) + Send>;

struct Listener {
    id: u64,
    filter: Filter,
    /// Held behind its own lock so that dispatch never holds the registry
    /// lock while user code runs.
    callback: Arc<Mutex<Callback>>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    listeners: Vec<Listener>,
    known: Vec<Arc<sys::DeviceInfo>>,
    started: bool,
}

/// Signals the hotplug thread. Set by the backend from whatever thread it
/// uses; coalescing is free because `pending` is just a flag.
#[derive(Default)]
struct Signal {
    state: Mutex<SignalState>,
    condvar: Condvar,
}

#[derive(Default)]
struct SignalState {
    pending: bool,
    stop: bool,
}

impl Signal {
    fn notify(&self) {
        let mut s = lock(&self.state);
        s.pending = true;
        self.condvar.notify_all();
    }

    fn stop(&self) {
        let mut s = lock(&self.state);
        s.stop = true;
        self.condvar.notify_all();
    }

    /// Blocks until there is something to do. Returns `false` to stop.
    fn wait(&self) -> bool {
        let mut s = lock(&self.state);
        while !s.pending && !s.stop {
            s = self.condvar.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        !s.stop
    }

    /// Consumes the pending flag; returns `false` if stopping.
    fn take_pending(&self) -> bool {
        let mut s = lock(&self.state);
        s.pending = false;
        !s.stop
    }

    fn stopping(&self) -> bool {
        lock(&self.state).stop
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Per-context hotplug state. Lives inside the context, so the dispatch
/// thread only ever holds a `Weak` to reach it.
pub(crate) struct Registry {
    state: Mutex<State>,
    /// Held while events are handed to callbacks, and while a new listener is
    /// added together with its initial batch. Keeps a rescan from slipping
    /// between a listener being added and its own devices being reported,
    /// which would let it see a departure before the matching arrival.
    dispatch: Mutex<()>,
    signal: Arc<Signal>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Registry {
    pub(crate) fn new() -> Registry {
        Registry {
            state: Mutex::new(State::default()),
            dispatch: Mutex::new(()),
            signal: Arc::new(Signal::default()),
            thread: Mutex::new(None),
        }
    }

    fn add(ctx: &Context, filter: Filter, enumerate_existing: bool, callback: Callback) -> Result<HotplugRegistration> {
        let inner = ctx.inner();
        let registry = &inner.hotplug;
        let callback = Arc::new(Mutex::new(callback));
        let dispatching = lock(&registry.dispatch);

        let (id, seed) = {
            let mut st = lock(&registry.state);
            if !st.started {
                // Ask the OS first: if this platform cannot do hotplug, fail
                // without leaving a half-started registry behind.
                let signal = Arc::clone(&registry.signal);
                inner.sys.watch_hotplug(Arc::new(move || signal.notify()))?;
                st.known = enumerate(&inner.sys);
                let weak = Arc::downgrade(inner);
                let signal = Arc::clone(&registry.signal);
                let handle = std::thread::Builder::new()
                    .name("rawusb-hotplug".into())
                    .spawn(move || dispatch_loop(weak, signal))
                    .map_err(|e| Error::from(e).context("spawn hotplug thread"))?;
                *lock(&registry.thread) = Some(handle);
                st.started = true;
            }
            st.next_id += 1;
            let id = st.next_id;
            st.listeners.push(Listener {
                id,
                filter,
                callback: Arc::clone(&callback),
            });
            let seed = if enumerate_existing { st.known.clone() } else { Vec::new() };
            (id, seed)
        };

        // Deliver the initial batch outside the registry lock, but still
        // holding off any rescan.
        for info in seed {
            let device = Device::new(ctx.clone(), info);
            if filter.matches(&device) {
                let event = HotplugEvent::Arrived(device);
                (lock(&callback))(&event);
            }
        }
        drop(dispatching);

        Ok(HotplugRegistration {
            ctx: Arc::downgrade(inner),
            id,
        })
    }

    fn remove(&self, id: u64) {
        lock(&self.state).listeners.retain(|l| l.id != id);
    }

    /// Re-enumerates and reports what changed since the last scan.
    fn rescan(&self, inner: &Arc<ContextInner>) {
        let Ok(current) = inner.sys.enumerate() else {
            // A transient enumeration failure must not be reported as every
            // device having been unplugged.
            return;
        };
        let current: Vec<Arc<sys::DeviceInfo>> = current.into_iter().map(Arc::new).collect();

        let _dispatching = lock(&self.dispatch);
        let (arrived, left, listeners) = {
            let mut st = lock(&self.state);
            let previous = std::mem::replace(&mut st.known, current.clone());
            let arrived: Vec<Arc<sys::DeviceInfo>> = current
                .iter()
                .filter(|c| !previous.iter().any(|p| sys::same_device(p, c)))
                .cloned()
                .collect();
            let left: Vec<Arc<sys::DeviceInfo>> = previous
                .into_iter()
                .filter(|p| !current.iter().any(|c| sys::same_device(c, p)))
                .collect();
            let listeners: Vec<(Filter, Arc<Mutex<Callback>>)> = st.listeners.iter().map(|l| (l.filter, Arc::clone(&l.callback))).collect();
            (arrived, left, listeners)
        };
        if (arrived.is_empty() && left.is_empty()) || listeners.is_empty() {
            return;
        }

        let ctx = Context::from_inner(Arc::clone(inner));
        // Departures first, so that a device replaced on the same port reads
        // as "left, then arrived".
        for (infos, arrival) in [(left, false), (arrived, true)] {
            for info in infos {
                let device = Device::new(ctx.clone(), info);
                let event = if arrival {
                    HotplugEvent::Arrived(device)
                } else {
                    HotplugEvent::Left(device)
                };
                for (filter, callback) in &listeners {
                    if filter.matches(event.device()) {
                        (lock(callback))(&event);
                    }
                }
            }
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.signal.stop();
        if let Some(handle) = lock(&self.thread).take() {
            // The hotplug thread itself may be the one dropping the context,
            // if it held the last reference. Joining would then deadlock.
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

fn enumerate(sys: &Arc<sys::Context>) -> Vec<Arc<sys::DeviceInfo>> {
    sys.enumerate().map(|v| v.into_iter().map(Arc::new).collect()).unwrap_or_default()
}

fn dispatch_loop(ctx: Weak<ContextInner>, signal: Arc<Signal>) {
    while signal.wait() {
        // Let a burst settle before looking, and give udev a moment to apply
        // permissions to a device node that just appeared.
        std::thread::sleep(COALESCE);
        if !signal.take_pending() {
            break;
        }
        let Some(inner) = ctx.upgrade() else { break };
        inner.hotplug.rescan(&inner);
        // Drop the strong reference before waiting again, so that a context
        // dropped meanwhile is freed promptly.
        drop(inner);
        if signal.stopping() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::DeviceDescriptor;

    fn descriptor(vendor_id: u16, product_id: u16, class: u8) -> DeviceDescriptor {
        DeviceDescriptor {
            usb_version: crate::types::Version(0x0200),
            class,
            sub_class: 0,
            protocol: 0,
            max_packet_size_0: 64,
            vendor_id,
            product_id,
            device_version: crate::types::Version(0),
            manufacturer_string_index: 0,
            product_string_index: 0,
            serial_number_string_index: 0,
            num_configurations: 1,
        }
    }

    #[test]
    fn filter_defaults_to_everything() {
        let f = Filter::default();
        assert_eq!(f.vendor_id, None);
        assert_eq!(f.product_id, None);
        assert_eq!(f.class, None);
    }

    #[test]
    fn descriptor_fields_drive_the_filter() {
        // Exercised without a live device: the vendor/product checks only
        // read the descriptor, which is what `matches` looks at first.
        let d = descriptor(0x046d, 0xc548, 0x00);
        let f = Filter {
            vendor_id: Some(0x046d),
            product_id: None,
            class: None,
        };
        assert!(f.vendor_id.is_none_or(|v| v == d.vendor_id));
        let f2 = Filter {
            vendor_id: Some(0x1234),
            ..f
        };
        assert!(!f2.vendor_id.is_none_or(|v| v == d.vendor_id));
    }

    #[test]
    fn signal_coalesces_and_stops() {
        let s = Signal::default();
        s.notify();
        s.notify();
        assert!(s.wait());
        assert!(s.take_pending());
        assert!(!s.stopping());
        s.stop();
        assert!(!s.wait());
        assert!(s.stopping());
    }
}
