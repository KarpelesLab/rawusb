//! The library context: owns the platform backend and its event thread.

use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::sys;
use crate::{Error, ErrorKind, Result};
use std::sync::Arc;

/// Everything a session owns, shared by every clone of a [`Context`] and by
/// the devices, handles and transfers derived from it.
pub(crate) struct ContextInner {
    pub(crate) sys: Arc<sys::Context>,
    #[cfg(feature = "hotplug")]
    pub(crate) hotplug: crate::hotplug::Registry,
}

/// A library session. Everything else is created from one.
///
/// A `Context` owns the operating-system resources used to enumerate devices
/// and one background thread that drives asynchronous transfer completion.
/// Cloning a context is cheap and shares the same session; the session ends
/// (and the thread exits) when the last clone and every handle and transfer
/// derived from it are dropped.
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

impl Context {
    /// Opens a new session.
    pub fn new() -> Result<Context> {
        Ok(Context {
            inner: Arc::new(ContextInner {
                sys: sys::Context::new()?,
                #[cfg(feature = "hotplug")]
                hotplug: crate::hotplug::Registry::new(),
            }),
        })
    }

    pub(crate) fn sys(&self) -> &Arc<sys::Context> {
        &self.inner.sys
    }

    #[cfg(feature = "hotplug")]
    pub(crate) fn inner(&self) -> &Arc<ContextInner> {
        &self.inner
    }

    #[cfg(feature = "hotplug")]
    pub(crate) fn from_inner(inner: Arc<ContextInner>) -> Context {
        Context { inner }
    }

    /// Lists the USB devices currently attached to the system.
    ///
    /// Enumeration reads descriptors without opening any device, so it never
    /// needs special permissions. Devices that cannot be inspected (for
    /// instance hubs the OS hides) are skipped silently.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let infos = self.inner.sys.enumerate()?;
        Ok(infos.into_iter().map(|info| Device::new(self.clone(), Arc::new(info))).collect())
    }

    /// Finds the first device with the given vendor and product IDs.
    pub fn find_device(&self, vendor_id: u16, product_id: u16) -> Result<Option<Device>> {
        Ok(self.devices()?.into_iter().find(|d| {
            let desc = d.device_descriptor();
            desc.vendor_id == vendor_id && desc.product_id == product_id
        }))
    }

    /// Opens the first device with the given vendor and product IDs, as
    /// `libusb_open_device_with_vid_pid` does. Fails with
    /// [`ErrorKind::NotFound`] if there is none.
    pub fn open_device_with_vid_pid(&self, vendor_id: u16, product_id: u16) -> Result<DeviceHandle> {
        match self.find_device(vendor_id, product_id)? {
            Some(d) => d.open(),
            None => Err(Error::with_message(ErrorKind::NotFound, "no device with that vendor/product id")),
        }
    }

    /// Finds the device with the given serial number among those whose
    /// vendor/product pair is in `ids`. An empty `ids` matches every device.
    ///
    /// Serial numbers come from [`Device::serial_number`], so no device is
    /// opened unless the OS does not know the serial of a candidate that has
    /// one; such a candidate is opened to read it, and skipped if it cannot
    /// be. The comparison is exact.
    ///
    /// ```no_run
    /// # let ctx = rawusb::Context::new()?;
    /// let ids = [(0x0403, 0x6001), (0x0403, 0x6010)];
    /// if let Some(dev) = ctx.find_device_by_serial(&ids, "FT8ZQ3XW")? {
    ///     println!("found at bus {} address {}", dev.bus_number(), dev.address());
    /// }
    /// # Ok::<(), rawusb::Error>(())
    /// ```
    pub fn find_device_by_serial(&self, ids: &[(u16, u16)], serial: &str) -> Result<Option<Device>> {
        Ok(self.lookup_serial(ids, serial)?.map(|(d, _)| d))
    }

    /// Opens the device [`find_device_by_serial`](Self::find_device_by_serial)
    /// finds. Fails with [`ErrorKind::NotFound`] if there is none.
    pub fn open_device_by_serial(&self, ids: &[(u16, u16)], serial: &str) -> Result<DeviceHandle> {
        match self.lookup_serial(ids, serial)? {
            Some((_, Some(h))) => Ok(h),
            Some((d, None)) => d.open(),
            None => Err(Error::with_message(ErrorKind::NotFound, "no device with that serial number")),
        }
    }

    /// The device matching `ids` and `serial`, with the handle opened to
    /// read its serial if that was needed.
    fn lookup_serial(&self, ids: &[(u16, u16)], serial: &str) -> Result<Option<(Device, Option<DeviceHandle>)>> {
        let candidates = self.devices()?.into_iter().filter(|d| {
            let desc = d.device_descriptor();
            desc.serial_number_string_index != 0 && (ids.is_empty() || ids.contains(&(desc.vendor_id, desc.product_id)))
        });
        for d in candidates {
            if let Some(s) = d.serial_number() {
                if s == serial {
                    return Ok(Some((d, None)));
                }
                continue;
            }
            let Ok(h) = d.open() else { continue };
            if h.read_serial_number_string().ok().flatten().as_deref() == Some(serial) {
                return Ok(Some((d, Some(h))));
            }
        }
        Ok(None)
    }

    /// Starts describing a hotplug watcher for this session.
    ///
    /// See the [`hotplug`](crate::hotplug) module for the whole story.
    ///
    /// ```no_run
    /// # let ctx = rawusb::Context::new()?;
    /// let watcher = ctx.hotplug().vendor_id(0x1234).enumerate_existing(true).watch()?;
    /// for event in watcher.iter() {
    ///     println!("{event:?}");
    /// }
    /// # Ok::<(), rawusb::Error>(())
    /// ```
    #[cfg(feature = "hotplug")]
    pub fn hotplug(&self) -> crate::hotplug::HotplugBuilder<'_> {
        crate::hotplug::HotplugBuilder::new(self)
    }
}
