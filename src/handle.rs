//! An open device: configuration, interfaces, and synchronous transfers.

use crate::descriptors::{decode_language_ids, decode_string_descriptor};
use crate::device::Device;
use crate::sys;
use crate::transfer::Transfer;
use crate::types::{ControlSetup, ControlType, Direction, Recipient, TransferType, descriptor_type, request};
use crate::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared between every clone of a [`DeviceHandle`]. Dropped when the last
/// user clone goes away, which cancels outstanding transfers and releases
/// claimed interfaces; the OS handle itself lives on (inside `sys`) until the
/// last in-flight transfer has drained.
pub(crate) struct HandleShared {
    pub(crate) sys: Arc<sys::Handle>,
    device: Device,
    claimed: Mutex<Vec<u8>>,
    detached: Mutex<Vec<u8>>,
    auto_detach: AtomicBool,
    /// Interfaces a class helper currently drives, so that two helpers can
    /// never share one.
    #[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
    leased: Mutex<Vec<u8>>,
    /// The language string descriptors are read in, once known.
    string_language: Mutex<Option<u16>>,
}

impl Drop for HandleShared {
    fn drop(&mut self) {
        self.sys.cancel_all();
        let claimed = std::mem::take(self.claimed.get_mut().unwrap_or_else(|e| e.into_inner()));
        for iface in claimed {
            let _ = self.sys.release_interface(iface);
        }
        let detached = std::mem::take(self.detached.get_mut().unwrap_or_else(|e| e.into_inner()));
        for iface in detached {
            let _ = self.sys.attach_kernel_driver(iface);
        }
    }
}

/// An open USB device.
///
/// Handles are cheap to clone; all clones refer to the same open device. When
/// the last clone is dropped, in-flight transfers are cancelled, claimed
/// interfaces are released (re-attaching any kernel driver that was
/// auto-detached), and the device is closed.
///
/// The synchronous methods here are thin wrappers around [`Transfer`]: each
/// allocates a transfer, submits it and waits. Use [`Transfer`] directly for
/// anything performance-sensitive or concurrent.
#[derive(Clone)]
pub struct DeviceHandle {
    shared: Arc<HandleShared>,
}

impl std::fmt::Debug for DeviceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceHandle").field("device", &self.shared.device).finish()
    }
}

/// Timeout used by the string-descriptor helpers, matching libusb.
const STRING_TIMEOUT: Duration = Duration::from_millis(1000);

impl DeviceHandle {
    pub(crate) fn new(device: Device, sys: Arc<sys::Handle>) -> DeviceHandle {
        DeviceHandle {
            shared: Arc::new(HandleShared {
                sys,
                device,
                claimed: Mutex::new(Vec::new()),
                detached: Mutex::new(Vec::new()),
                auto_detach: AtomicBool::new(false),
                #[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
                leased: Mutex::new(Vec::new()),
                string_language: Mutex::new(None),
            }),
        }
    }

    pub(crate) fn from_shared(shared: Arc<HandleShared>) -> DeviceHandle {
        DeviceHandle { shared }
    }

    pub(crate) fn shared(&self) -> &Arc<HandleShared> {
        &self.shared
    }

    /// The device this handle was opened from.
    pub fn device(&self) -> &Device {
        &self.shared.device
    }

    // ----- configuration & interfaces -------------------------------------

    /// The `bConfigurationValue` of the active configuration, or 0 if the
    /// device is unconfigured.
    pub fn active_configuration(&self) -> Result<u8> {
        if let Some(v) = self.shared.sys.active_configuration()? {
            return Ok(v);
        }
        let mut buf = [0u8; 1];
        let n = self.control_read(
            crate::types::request_type(Direction::In, ControlType::Standard, Recipient::Device),
            request::GET_CONFIGURATION,
            0,
            0,
            &mut buf,
            STRING_TIMEOUT,
        )?;
        if n != 1 {
            return Err(Error::with_message(ErrorKind::Io, "GET_CONFIGURATION returned no data"));
        }
        Ok(buf[0])
    }

    /// Selects a configuration by `bConfigurationValue`. Fails with
    /// [`ErrorKind::Busy`] while interfaces are claimed or kernel drivers are
    /// bound to them.
    pub fn set_configuration(&self, value: u8) -> Result<()> {
        self.shared.sys.set_configuration(value)
    }

    /// Claims an interface so that its endpoints can be used. If
    /// [`set_auto_detach_kernel_driver`](Self::set_auto_detach_kernel_driver)
    /// is on, a kernel driver bound to the interface is detached first.
    pub fn claim_interface(&self, interface: u8) -> Result<()> {
        self.claim(interface, self.shared.auto_detach.load(Ordering::Relaxed))
    }

    /// Takes the whole device: claims every interface of the active
    /// configuration, detaching kernel drivers from them whatever the
    /// auto-detach setting. They stay claimed until released or until the
    /// last clone of this handle is dropped, when the kernel drivers are
    /// re-attached.
    ///
    /// Class helpers opened on a taken device use these claims instead of
    /// making their own, and leave the interfaces claimed when dropped, so
    /// helpers can come and go without the kernel driver grabbing the
    /// interface in between.
    ///
    /// All or nothing: if one interface cannot be claimed, the ones claimed
    /// by this call are released again and the error is returned.
    pub fn claim_all_interfaces(&self) -> Result<()> {
        let cfg = self.device().active_config_descriptor()?;
        let mut newly = Vec::new();
        for iface in cfg.interfaces.iter().map(|i| i.number) {
            if self.is_claimed(iface) {
                continue;
            }
            if let Err(e) = self.claim(iface, true) {
                for &i in &newly {
                    let _ = self.release_interface(i);
                }
                return Err(e.context(format!("claiming interface {iface}")));
            }
            newly.push(iface);
        }
        Ok(())
    }

    /// Whether this handle holds a claim on the interface.
    pub fn is_claimed(&self, interface: u8) -> bool {
        self.shared.claimed.lock().unwrap_or_else(|e| e.into_inner()).contains(&interface)
    }

    /// Claims an interface, detaching (and later re-attaching) a bound kernel
    /// driver whatever the auto-detach setting. The class helpers use this so
    /// they work out of the box without changing the caller's handle setting.
    #[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
    pub(crate) fn claim_interface_detaching(&self, interface: u8) -> Result<()> {
        self.claim(interface, true)
    }

    /// Reserves interfaces for one class helper. Fails with
    /// [`ErrorKind::Busy`] if another helper already drives one of them.
    #[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
    pub(crate) fn lease(&self, interfaces: &[u8]) -> Result<()> {
        let mut leased = self.shared.leased.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = interfaces.iter().find(|i| leased.contains(i)) {
            return Err(Error::with_message(
                ErrorKind::Busy,
                format!("interface {i} is already in use by another class helper"),
            ));
        }
        leased.extend_from_slice(interfaces);
        Ok(())
    }

    #[cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]
    pub(crate) fn unlease(&self, interfaces: &[u8]) {
        self.shared
            .leased
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|i| !interfaces.contains(i));
    }

    fn claim(&self, interface: u8, detach: bool) -> Result<()> {
        if detach && self.shared.sys.kernel_driver_active(interface).unwrap_or(false) {
            match self.shared.sys.detach_kernel_driver(interface) {
                Ok(()) => self.shared.detached.lock().unwrap_or_else(|e| e.into_inner()).push(interface),
                Err(e) if e.kind() == ErrorKind::NotSupported || e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        self.shared.sys.claim_interface(interface)?;
        let mut claimed = self.shared.claimed.lock().unwrap_or_else(|e| e.into_inner());
        if !claimed.contains(&interface) {
            claimed.push(interface);
        }
        Ok(())
    }

    /// Releases a previously claimed interface, resetting it to alternate
    /// setting 0 and re-attaching a kernel driver that was auto-detached.
    pub fn release_interface(&self, interface: u8) -> Result<()> {
        self.shared.sys.release_interface(interface)?;
        self.shared
            .claimed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&i| i != interface);
        let mut detached = self.shared.detached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = detached.iter().position(|&i| i == interface) {
            detached.remove(pos);
            drop(detached);
            let _ = self.shared.sys.attach_kernel_driver(interface);
        }
        Ok(())
    }

    /// Interfaces currently claimed through this handle.
    pub fn claimed_interfaces(&self) -> Vec<u8> {
        self.shared.claimed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Activates an alternate setting of a claimed interface.
    pub fn set_alternate_setting(&self, interface: u8, alternate_setting: u8) -> Result<()> {
        self.shared.sys.set_alt_setting(interface, alternate_setting)
    }

    /// Clears the halt/stall condition on an endpoint.
    pub fn clear_halt(&self, endpoint: u8) -> Result<()> {
        self.shared.sys.clear_halt(endpoint)
    }

    /// Performs a USB port reset. On success the device is re-enumerated by
    /// the OS but this handle stays usable; if the device changed (for
    /// instance re-enumerated with a different descriptor), the call fails
    /// with [`ErrorKind::NotFound`] and the handle must be reopened.
    pub fn reset(&self) -> Result<()> {
        self.shared.sys.reset()?;
        // A reset unbinds every interface; take ours back, as libusb does.
        let claimed = self.claimed_interfaces();
        for iface in claimed {
            if self.shared.sys.claim_interface(iface).is_err() {
                return Err(Error::with_message(ErrorKind::NotFound, "device changed across reset; reopen it"));
            }
        }
        Ok(())
    }

    /// Whether a kernel driver is bound to the interface. Always `false` on
    /// platforms where the question does not apply.
    pub fn kernel_driver_active(&self, interface: u8) -> Result<bool> {
        self.shared.sys.kernel_driver_active(interface)
    }

    /// Unbinds the kernel driver from an interface so it can be claimed.
    pub fn detach_kernel_driver(&self, interface: u8) -> Result<()> {
        self.shared.sys.detach_kernel_driver(interface)
    }

    /// Re-binds the kernel driver to an interface.
    pub fn attach_kernel_driver(&self, interface: u8) -> Result<()> {
        self.shared.sys.attach_kernel_driver(interface)
    }

    /// Enables or disables automatic kernel-driver detaching on
    /// [`claim_interface`](Self::claim_interface) (and re-attaching on
    /// release). Off by default, like libusb.
    pub fn set_auto_detach_kernel_driver(&self, enable: bool) {
        self.shared.auto_detach.store(enable, Ordering::Relaxed);
    }

    // ----- synchronous transfers -------------------------------------------

    /// Runs a control transfer described by `setup`. For an IN request the
    /// response lands in `data` (at most `data.len()` bytes; `setup.length`
    /// is clamped to it); for an OUT request `data` is sent. Returns the
    /// number of data bytes transferred.
    pub fn control_transfer(&self, mut setup: ControlSetup, data: &mut [u8], timeout: Duration) -> Result<usize> {
        let len = data.len().min(u16::MAX as usize) as u16;
        setup.length = len;
        let t = Transfer::control(self, setup, &data[..len as usize]);
        t.set_timeout(timeout)?;
        let n = t.submit_and_wait()?;
        if setup.direction() == Direction::In {
            let received = t.data()?;
            data[..received.len()].copy_from_slice(&received);
        }
        Ok(n)
    }

    /// Reads from the device with a control request (`bmRequestType` must
    /// have the IN bit set). Returns the number of bytes received.
    pub fn control_read(&self, request_type: u8, request: u8, value: u16, index: u16, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if request_type & 0x80 == 0 {
            return Err(Error::with_message(
                ErrorKind::InvalidParam,
                "control_read needs an IN request type",
            ));
        }
        self.control_transfer(ControlSetup::raw(request_type, request, value, index, 0), buf, timeout)
    }

    /// Writes to the device with a control request (`bmRequestType` must have
    /// the IN bit clear). Returns the number of bytes sent.
    pub fn control_write(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], timeout: Duration) -> Result<usize> {
        if request_type & 0x80 != 0 {
            return Err(Error::with_message(
                ErrorKind::InvalidParam,
                "control_write needs an OUT request type",
            ));
        }
        let setup = ControlSetup::raw(request_type, request, value, index, data.len() as u16);
        let t = Transfer::control(self, setup, data);
        t.set_timeout(timeout)?;
        t.submit_and_wait()
    }

    fn sync_read(&self, kind: TransferType, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if endpoint & 0x80 == 0 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "read needs an IN endpoint"));
        }
        let t = Transfer::new(self, kind, endpoint, vec![0u8; buf.len()]);
        t.set_timeout(timeout)?;
        let n = t.submit_and_wait()?;
        buf[..n].copy_from_slice(&t.data()?);
        Ok(n)
    }

    fn sync_write(&self, kind: TransferType, endpoint: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        if endpoint & 0x80 != 0 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "write needs an OUT endpoint"));
        }
        let t = Transfer::new(self, kind, endpoint, data.to_vec());
        t.set_timeout(timeout)?;
        t.submit_and_wait()
    }

    /// Reads from a bulk IN endpoint. Returns the number of bytes received,
    /// which may be less than `buf.len()`.
    pub fn bulk_read(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.sync_read(TransferType::Bulk, endpoint, buf, timeout)
    }

    /// Writes to a bulk OUT endpoint. Returns the number of bytes sent.
    pub fn bulk_write(&self, endpoint: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        self.sync_write(TransferType::Bulk, endpoint, data, timeout)
    }

    /// Reads from an interrupt IN endpoint.
    pub fn interrupt_read(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.sync_read(TransferType::Interrupt, endpoint, buf, timeout)
    }

    /// Writes to an interrupt OUT endpoint.
    pub fn interrupt_write(&self, endpoint: u8, data: &[u8], timeout: Duration) -> Result<usize> {
        self.sync_write(TransferType::Interrupt, endpoint, data, timeout)
    }

    // ----- descriptors -----------------------------------------------------

    /// Fetches a descriptor with a standard GET_DESCRIPTOR request. Returns
    /// the number of bytes received.
    pub fn read_descriptor(&self, descriptor_type: u8, index: u8, language_id: u16, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.control_read(
            crate::types::request_type(Direction::In, ControlType::Standard, Recipient::Device),
            request::GET_DESCRIPTOR,
            (descriptor_type as u16) << 8 | index as u16,
            language_id,
            buf,
            timeout,
        )
    }

    /// Fetches a raw string descriptor the way the Linux kernel does: ask
    /// for the largest possible descriptor, and if the device stalls that or
    /// sends less than a header, read the 2-byte header and then exactly the
    /// length it declares. Returns the bytes received, trimmed to `bLength`.
    fn read_string_raw(&self, index: u8, language_id: u16, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = [0u8; 255];
        let n = match self.read_descriptor(descriptor_type::STRING, index, language_id, &mut buf, timeout) {
            Ok(n) if n >= 2 => n,
            Err(e) if !e.is_stall() => return Err(e),
            _ => {
                let mut head = [0u8; 2];
                let got = self.read_descriptor(descriptor_type::STRING, index, language_id, &mut head, timeout)?;
                if got < 2 || head[0] < 2 {
                    return Err(Error::with_message(ErrorKind::Io, "string descriptor too short"));
                }
                let want = head[0] as usize;
                self.read_descriptor(descriptor_type::STRING, index, language_id, &mut buf[..want], timeout)?
            }
        };
        Ok(buf[..n.min(buf[0].max(2) as usize)].to_vec())
    }

    /// The language IDs the device offers string descriptors in.
    pub fn read_languages(&self, timeout: Duration) -> Result<Vec<u16>> {
        decode_language_ids(&self.read_string_raw(0, 0, timeout)?)
    }

    /// Reads a string descriptor in the given language.
    pub fn read_string_descriptor(&self, language_id: u16, index: u8, timeout: Duration) -> Result<String> {
        if index == 0 {
            return Err(Error::with_message(ErrorKind::InvalidParam, "string index 0 is the language table"));
        }
        decode_string_descriptor(&self.read_string_raw(index, language_id, timeout)?)
    }

    /// The language [`read_string`](Self::read_string) uses: the device's
    /// first one, read once per handle. A device whose language table is
    /// empty or malformed gets US English (0x0409), as the Linux kernel
    /// assumes; one that cannot return the table at all gets the error.
    fn string_language(&self) -> Result<u16> {
        if let Some(lang) = *self.shared.string_language.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(lang);
        }
        let lang = match self.read_languages(STRING_TIMEOUT) {
            Ok(langs) => langs.first().copied().unwrap_or(0x0409),
            Err(e) if e.kind() == ErrorKind::Io => 0x0409,
            Err(e) => return Err(e),
        };
        *self.shared.string_language.lock().unwrap_or_else(|e| e.into_inner()) = Some(lang);
        Ok(lang)
    }

    /// Reads a string descriptor in the device's first language, as
    /// `libusb_get_string_descriptor_ascii` does (but returns full Unicode).
    pub fn read_string(&self, index: u8) -> Result<String> {
        let lang = self.string_language()?;
        self.read_string_descriptor(lang, index, STRING_TIMEOUT)
    }

    fn read_optional_string(&self, index: u8) -> Result<Option<String>> {
        if index == 0 { Ok(None) } else { self.read_string(index).map(Some) }
    }

    /// The manufacturer string, if the device has one.
    pub fn read_manufacturer_string(&self) -> Result<Option<String>> {
        self.read_optional_string(self.device().device_descriptor().manufacturer_string_index)
    }

    /// The product string, if the device has one.
    pub fn read_product_string(&self) -> Result<Option<String>> {
        self.read_optional_string(self.device().device_descriptor().product_string_index)
    }

    /// The serial number string, if the device has one.
    pub fn read_serial_number_string(&self) -> Result<Option<String>> {
        self.read_optional_string(self.device().device_descriptor().serial_number_string_index)
    }

    /// Reads the Binary Object Store descriptor (USB 2.1+ / 3.x devices).
    /// Returns the raw bytes; fails with [`ErrorKind::Pipe`] on devices that
    /// do not have one.
    pub fn read_bos_descriptor(&self, timeout: Duration) -> Result<Vec<u8>> {
        let mut head = [0u8; 5];
        let n = self.read_descriptor(descriptor_type::BOS, 0, 0, &mut head, timeout)?;
        if n < 5 || head[1] != descriptor_type::BOS {
            return Err(Error::with_message(ErrorKind::Io, "malformed BOS descriptor"));
        }
        let total = u16::from_le_bytes([head[2], head[3]]) as usize;
        let mut buf = vec![0u8; total.max(5)];
        let n = self.read_descriptor(descriptor_type::BOS, 0, 0, &mut buf, timeout)?;
        buf.truncate(n);
        Ok(buf)
    }
}
