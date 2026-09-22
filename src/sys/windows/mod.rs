//! Windows backend: SetupAPI/CfgMgr for enumeration, the hub driver's
//! ioctls for descriptors, and WinUSB with an I/O completion port for
//! transfers.
//!
//! Only devices (or composite-device functions) bound to the WinUSB driver
//! can be claimed and used for I/O. Any device can be enumerated and opened;
//! on a device without WinUSB the handle only serves `GET_DESCRIPTOR`
//! control requests, which are routed through the parent hub.

mod ffi;

#[cfg(feature = "hotplug")]
use super::Notifier;
use super::{DeviceInfo, split_config_descriptors};
use crate::descriptors::{ConfigDescriptor, DeviceDescriptor};
use crate::transfer::{Inner, State};
use crate::types::{ControlSetup, IsoPacket, Speed, TransferStatus, TransferType, Version, descriptor_type, request};
use crate::{Error, ErrorKind, Result};
use ffi::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Instant;

/// Maps a Win32 error code to an error kind.
pub(crate) fn errno_kind(code: i32) -> ErrorKind {
    match code as DWORD {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_NOT_FOUND => ErrorKind::NotFound,
        ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION => ErrorKind::Access,
        ERROR_DEVICE_NOT_CONNECTED | ERROR_NO_SUCH_DEVICE | ERROR_BAD_COMMAND => ErrorKind::NoDevice,
        ERROR_BUSY => ErrorKind::Busy,
        ERROR_SEM_TIMEOUT | ERROR_TIMEOUT | WAIT_TIMEOUT => ErrorKind::Timeout,
        ERROR_GEN_FAILURE => ErrorKind::Pipe,
        ERROR_MORE_DATA | ERROR_BUFFER_OVERFLOW | ERROR_INSUFFICIENT_BUFFER => ErrorKind::Overflow,
        ERROR_INVALID_PARAMETER | ERROR_INVALID_HANDLE => ErrorKind::InvalidParam,
        ERROR_NOT_ENOUGH_MEMORY => ErrorKind::NoMem,
        ERROR_OPERATION_ABORTED => ErrorKind::Interrupted,
        ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION => ErrorKind::NotSupported,
        _ => ErrorKind::Io,
    }
}

fn os_error(op: &'static str) -> Error {
    let code = last_error();
    Error::from_code(errno_kind(code as i32), code as i32).context(op)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A composite-device function bound to WinUSB.
struct Function {
    first_interface: u8,
    path: String,
}

/// Where a device lives and how it can be reached.
pub(crate) struct Location {
    /// Device interface path of the whole device (`\\?\usb#vid_...`).
    path: String,
    /// Interface path of the parent hub, for descriptor requests.
    hub_path: Option<String>,
    /// Port on the parent hub (the hub's connection index).
    port: u32,
    /// Whether the device itself is bound to WinUSB.
    winusb: bool,
    /// WinUSB-bound functions of a composite device.
    functions: Vec<Function>,
}

const WAKE_KEY: usize = 1;

// ----- context & event thread ---------------------------------------------------------

pub(crate) struct Context {
    iocp: Arc<OwnedHandle>,
    handles: Mutex<Vec<Weak<Handle>>>,
    #[cfg(feature = "hotplug")]
    hotplug: Mutex<Option<Hotplug>>,
    /// Transfers completed synchronously (hub descriptor requests) that must
    /// be reported from the event thread.
    deferred: Mutex<Vec<(Arc<Inner>, TransferStatus, usize)>>,
}

impl Context {
    pub(crate) fn new() -> Result<Arc<Self>> {
        // SAFETY: creating a fresh completion port.
        let iocp = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if iocp.is_null() {
            return Err(os_error("CreateIoCompletionPort"));
        }
        let iocp = Arc::new(OwnedHandle(iocp));
        let ctx = Arc::new(Context {
            iocp: Arc::clone(&iocp),
            handles: Mutex::new(Vec::new()),
            #[cfg(feature = "hotplug")]
            hotplug: Mutex::new(None),
            deferred: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&ctx);
        std::thread::Builder::new()
            .name("rawusb-events".into())
            .spawn(move || event_loop(weak, iocp))
            .map_err(|e| Error::from(e).context("spawn event thread"))?;
        Ok(ctx)
    }

    fn wake(&self) {
        // SAFETY: posting a packet with no OVERLAPPED to our own port.
        unsafe { PostQueuedCompletionStatus(self.iocp.0, 0, WAKE_KEY, std::ptr::null_mut()) };
    }

    fn defer(&self, inner: &Arc<Inner>, status: TransferStatus, len: usize) {
        lock(&self.deferred).push((Arc::clone(inner), status, len));
        self.wake();
    }

    pub(crate) fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        enumerate()
    }

    /// Starts reporting device changes through `notify`.
    ///
    /// `CM_Register_Notification` calls back on a thread-pool thread, which
    /// only ever sets a flag here, so there is no extra thread to run.
    #[cfg(feature = "hotplug")]
    pub(crate) fn watch_hotplug(self: &Arc<Self>, notify: Notifier) -> Result<()> {
        let mut slot = lock(&self.hotplug);
        if slot.is_some() {
            return Ok(());
        }
        let api = cm_notify_api().ok_or_else(|| {
            Error::with_message(
                ErrorKind::NotSupported,
                "hotplug needs CM_Register_Notification, which arrives with Windows 10 1709",
            )
        })?;
        let mut filter: CM_NOTIFY_FILTER = CM_NOTIFY_FILTER {
            cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as DWORD,
            Flags: 0,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            Reserved: 0,
            u: CM_NOTIFY_FILTER_UNION {
                DeviceInterface: CM_NOTIFY_FILTER_DEVICEINTERFACE {
                    ClassGuid: GUID_DEVINTERFACE_USB_DEVICE,
                },
            },
        };
        // The callback needs the notifier for as long as it can fire; the box
        // is reclaimed once the registration is cancelled.
        let refcon = Box::into_raw(Box::new(notify));
        let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: valid filter, context and out-pointer; the callback has the
        // signature the API expects.
        let cr = unsafe { (api.register)(&mut filter, refcon as *mut c_void, hotplug_callback, &mut handle) };
        if cr != CR_SUCCESS {
            // SAFETY: nothing was registered, so nothing can reach the box.
            drop(unsafe { Box::from_raw(refcon) });
            return Err(Error::with_message(ErrorKind::Other, "CM_Register_Notification failed"));
        }
        *slot = Some(Hotplug { handle, refcon });
        Ok(())
    }

    pub(crate) fn open(self: &Arc<Self>, dev: &Arc<DeviceInfo>) -> Result<Arc<Handle>> {
        let cfg = dev
            .active_config
            .and_then(|v| dev.configs.iter().find(|c| c.get(5) == Some(&v)))
            .or_else(|| dev.configs.first())
            .and_then(|raw| ConfigDescriptor::from_bytes(raw).ok());
        let main = if dev.location.winusb {
            Some(Arc::new(WinUsbDevice::open(&dev.location.path, self)?))
        } else {
            None
        };
        let handle = Arc::new(Handle {
            ctx: Arc::clone(self),
            info: Arc::clone(dev),
            cfg,
            main,
            functions: Mutex::new(HashMap::new()),
            claimed: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            iso_inflight: Mutex::new(HashMap::new()),
            disconnected: AtomicBool::new(false),
        });
        lock(&self.handles).push(Arc::downgrade(&handle));
        Ok(handle)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.wake();
    }
}

/// Heap block handed to the kernel with each overlapped request. The
/// `OVERLAPPED` must stay first: the completion port hands back a pointer to
/// it, which we cast back to this structure.
#[repr(C)]
struct OvEntry {
    ov: OVERLAPPED,
    inner: Arc<Inner>,
    /// Per-packet results of an isochronous read, written by the driver.
    iso_descs: Vec<USBD_ISO_PACKET_DESCRIPTOR>,
    /// Keeps the isochronous buffer registered until the transfer completes.
    iso_buffer: Option<IsochBuffer>,
}

fn event_loop(ctx: Weak<Context>, iocp: Arc<OwnedHandle>) {
    loop {
        let (handles, timeout_ms) = {
            let Some(ctx) = ctx.upgrade() else { break };
            let deferred: Vec<_> = std::mem::take(&mut *lock(&ctx.deferred));
            for (inner, status, len) in deferred {
                inner.complete(status, len, None);
            }
            let handles: Vec<Arc<Handle>> = {
                let mut g = lock(&ctx.handles);
                g.retain(|w| w.strong_count() > 0);
                g.iter().filter_map(|w| w.upgrade()).collect()
            };
            let now = Instant::now();
            let timeout = match handles.iter().filter_map(|h| h.next_deadline()).min() {
                Some(d) => d
                    .saturating_duration_since(now)
                    .as_millis()
                    .saturating_add(1)
                    .min(INFINITE as u128 - 1) as DWORD,
                None => INFINITE,
            };
            (handles, timeout)
        };

        let mut bytes: DWORD = 0;
        let mut key: usize = 0;
        let mut ov: *mut OVERLAPPED = std::ptr::null_mut();
        // SAFETY: valid out-pointers; the port lives as long as this thread.
        let ok = unsafe { GetQueuedCompletionStatus(iocp.0, &mut bytes, &mut key, &mut ov, timeout_ms) };
        let err = if ok == FALSE { last_error() } else { ERROR_SUCCESS };

        if !ov.is_null() {
            // SAFETY: every OVERLAPPED we hand to the kernel is the first
            // field of a leaked `Box<OvEntry>`, recovered exactly once here.
            let entry = unsafe { Box::from_raw(ov as *mut OvEntry) };
            let inner = Arc::clone(&entry.inner);
            inner.handle.finish(&inner, ov as usize, bytes as usize, err, &entry.iso_descs);
            // Dropping the entry also unregisters any isochronous buffer.
            drop(entry);
        } else if ok == FALSE && err != WAIT_TIMEOUT {
            // The port is gone.
            break;
        }

        let now = Instant::now();
        for h in &handles {
            h.expire(now);
        }
    }
}

// ----- hotplug -------------------------------------------------------------------------

/// `CM_Register_Notification`.
#[cfg(feature = "hotplug")]
type CmRegisterFn = unsafe extern "system" fn(*mut CM_NOTIFY_FILTER, *mut c_void, CM_NOTIFY_CALLBACK, *mut HCMNOTIFICATION) -> CONFIGRET;
/// `CM_Unregister_Notification`.
#[cfg(feature = "hotplug")]
type CmUnregisterFn = unsafe extern "system" fn(HCMNOTIFICATION) -> CONFIGRET;

/// The two `cfgmgr32` entry points, resolved at run time.
#[cfg(feature = "hotplug")]
struct CmNotifyApi {
    register: CmRegisterFn,
    unregister: CmUnregisterFn,
}

#[cfg(feature = "hotplug")]
fn cm_notify_api() -> Option<&'static CmNotifyApi> {
    static API: OnceLock<Option<CmNotifyApi>> = OnceLock::new();
    API.get_or_init(|| {
        let register = proc_address("cfgmgr32.dll", b"CM_Register_Notification\0")?;
        let unregister = proc_address("cfgmgr32.dll", b"CM_Unregister_Notification\0")?;
        // SAFETY: these are the documented signatures of the two exports.
        unsafe {
            Some(CmNotifyApi {
                register: std::mem::transmute::<*const c_void, CmRegisterFn>(register),
                unregister: std::mem::transmute::<*const c_void, CmUnregisterFn>(unregister),
            })
        }
    })
    .as_ref()
}

/// A live `CM_Register_Notification` registration owned by a context.
#[cfg(feature = "hotplug")]
struct Hotplug {
    handle: HCMNOTIFICATION,
    refcon: *mut Notifier,
}

// SAFETY: the handle is only passed back to cfgmgr32, and the box behind
// `refcon` is immutable once installed.
#[cfg(feature = "hotplug")]
unsafe impl Send for Hotplug {}
// SAFETY: as above.
#[cfg(feature = "hotplug")]
unsafe impl Sync for Hotplug {}

#[cfg(feature = "hotplug")]
impl Drop for Hotplug {
    fn drop(&mut self) {
        if let Some(api) = cm_notify_api() {
            // SAFETY: `CM_Unregister_Notification` waits for any callback in
            // flight to return, so the notifier box is unreachable afterwards.
            unsafe {
                (api.unregister)(self.handle);
                drop(Box::from_raw(self.refcon));
            }
        }
    }
}

/// Runs on a system thread-pool thread when a USB device interface appears or
/// disappears.
#[cfg(feature = "hotplug")]
unsafe extern "system" fn hotplug_callback(
    _notification: HCMNOTIFICATION,
    context: *mut c_void,
    action: DWORD,
    _event_data: *mut c_void,
    _event_data_size: DWORD,
) -> DWORD {
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL {
        // SAFETY: `context` is the boxed notifier installed by
        // `watch_hotplug`, freed only after this registration is cancelled.
        let notify = unsafe { &*(context as *const Notifier) };
        notify();
    }
    ERROR_SUCCESS
}

// ----- enumeration ---------------------------------------------------------------------

/// A SetupAPI device information set.
struct DevInfoSet(HDEVINFO);

impl DevInfoSet {
    fn for_interface(guid: &GUID) -> Result<DevInfoSet> {
        // SAFETY: plain SetupAPI call with a valid GUID.
        let set = unsafe { SetupDiGetClassDevsW(guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) };
        if set == INVALID_HANDLE_VALUE {
            return Err(os_error("SetupDiGetClassDevs"));
        }
        Ok(DevInfoSet(set))
    }

    /// Enumerates `(device path, devinst)` pairs for one interface class.
    fn interfaces(&self, guid: &GUID) -> Vec<(String, DEVINST)> {
        let mut out = Vec::new();
        let mut index = 0;
        loop {
            let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as DWORD,
                InterfaceClassGuid: *guid,
                Flags: 0,
                Reserved: 0,
            };
            // SAFETY: valid set and out-structure.
            if unsafe { SetupDiEnumDeviceInterfaces(self.0, std::ptr::null_mut(), guid, index, &mut ifdata) } == FALSE {
                break;
            }
            index += 1;
            let mut required: DWORD = 0;
            // SAFETY: size query; a NULL buffer is allowed.
            unsafe { SetupDiGetDeviceInterfaceDetailW(self.0, &mut ifdata, std::ptr::null_mut(), 0, &mut required, std::ptr::null_mut()) };
            if required < 8 {
                continue;
            }
            let mut buf = vec![0u32; (required as usize).div_ceil(4)];
            buf[0] = SP_DEVICE_INTERFACE_DETAIL_DATA_W_SIZE;
            let mut devinfo = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as DWORD,
                ClassGuid: GUID::new(0, 0, 0, [0; 8]),
                DevInst: 0,
                Reserved: 0,
            };
            // SAFETY: `buf` holds `required` bytes with cbSize set.
            let ok = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    self.0,
                    &mut ifdata,
                    buf.as_mut_ptr() as *mut c_void,
                    required,
                    &mut required,
                    &mut devinfo,
                )
            };
            if ok == FALSE {
                continue;
            }
            // SAFETY: the path starts 4 bytes in and is NUL-terminated within
            // the buffer.
            let path = unsafe {
                let p = (buf.as_ptr() as *const u8).add(4) as *const u16;
                let n = (required as usize - 4) / 2;
                from_wide(std::slice::from_raw_parts(p, n))
            };
            out.push((path, devinfo.DevInst));
        }
        out
    }
}

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: destroying the set we created.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn instance_id(devinst: DEVINST) -> Option<String> {
    let mut buf = [0u16; 512];
    // SAFETY: buffer size passed correctly.
    let cr = unsafe { CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), buf.len() as DWORD, 0) };
    (cr == CR_SUCCESS).then(|| from_wide(&buf))
}

fn parent(devinst: DEVINST) -> Option<DEVINST> {
    let mut p: DEVINST = 0;
    // SAFETY: plain call.
    let cr = unsafe { CM_Get_Parent(&mut p, devinst, 0) };
    (cr == CR_SUCCESS).then_some(p)
}

fn children(devinst: DEVINST) -> Vec<DEVINST> {
    let mut out = Vec::new();
    let mut child: DEVINST = 0;
    // SAFETY: plain calls.
    unsafe {
        if CM_Get_Child(&mut child, devinst, 0) != CR_SUCCESS {
            return out;
        }
        out.push(child);
        loop {
            let mut sibling: DEVINST = 0;
            if CM_Get_Sibling(&mut sibling, child, 0) != CR_SUCCESS {
                break;
            }
            out.push(sibling);
            child = sibling;
        }
    }
    out
}

fn registry_property(devinst: DEVINST, property: DWORD) -> Option<Vec<u8>> {
    let mut ty: DWORD = 0;
    let mut len: DWORD = 0;
    // SAFETY: size query with a NULL buffer.
    let cr = unsafe { CM_Get_DevNode_Registry_PropertyW(devinst, property, &mut ty, std::ptr::null_mut(), &mut len, 0) };
    if cr != CR_BUFFER_SMALL && cr != CR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u8; len as usize + 2];
    // SAFETY: `buf` holds `len` bytes.
    let cr = unsafe { CM_Get_DevNode_Registry_PropertyW(devinst, property, &mut ty, buf.as_mut_ptr() as *mut c_void, &mut len, 0) };
    if cr != CR_SUCCESS {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
}

fn registry_string(devinst: DEVINST, property: DWORD) -> Option<String> {
    let raw = registry_property(devinst, property)?;
    let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    Some(from_wide(&units))
}

fn registry_u32(devinst: DEVINST, property: DWORD) -> Option<u32> {
    let raw = registry_property(devinst, property)?;
    raw.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn is_root_hub(id: &str) -> bool {
    id.to_ascii_uppercase().starts_with("USB\\ROOT_HUB")
}

/// Device interface paths a device instance exposes for one class GUID.
fn interface_paths(guid: &GUID, instance: &str) -> Vec<String> {
    let id = wide(instance);
    let mut len: DWORD = 0;
    // SAFETY: plain size query.
    if unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, guid, id.as_ptr(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) } != CR_SUCCESS
        || len < 2
    {
        return Vec::new();
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units.
    if unsafe { CM_Get_Device_Interface_ListW(guid, id.as_ptr(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) }
        != CR_SUCCESS
    {
        return Vec::new();
    }
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The interface GUIDs a WinUSB function registers (`DeviceInterfaceGUIDs`
/// or `DeviceInterfaceGUID` in its hardware registry key).
fn device_interface_guids(devinst: DEVINST) -> Vec<GUID> {
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: plain call; the key is closed below.
    if unsafe { CM_Open_DevNode_Key(devinst, KEY_READ, 0, REG_DISPOSITION_OPEN_EXISTING, &mut key, CM_REGISTRY_HARDWARE) } != CR_SUCCESS {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in ["DeviceInterfaceGUIDs", "DeviceInterfaceGUID"] {
        let wname = wide(name);
        let mut ty: DWORD = 0;
        let mut len: DWORD = 0;
        // SAFETY: size query.
        if unsafe { RegQueryValueExW(key, wname.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut len) } != 0 {
            continue;
        }
        let mut buf = vec![0u8; len as usize + 4];
        // SAFETY: `buf` holds `len` bytes.
        if unsafe { RegQueryValueExW(key, wname.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr(), &mut len) } != 0 {
            continue;
        }
        let units: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        for s in units.split(|&c| c == 0) {
            if let Some(g) = GUID::parse(&String::from_utf16_lossy(s)) {
                out.push(g);
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    // SAFETY: closing the key we opened.
    unsafe { RegCloseKey(key) };
    out
}

/// Opens a hub's device interface for ioctls.
fn open_hub(path: &str) -> Result<OwnedHandle> {
    let wpath = wide(path);
    // SAFETY: valid NUL-terminated path.
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(os_error("open hub"));
    }
    Ok(OwnedHandle(h))
}

struct ConnectionInfo {
    device_descriptor: [u8; 18],
    current_config: u8,
    speed: Speed,
    address: u16,
}

fn hub_connection_info(hub: &OwnedHandle, port: u32) -> Option<ConnectionInfo> {
    // SAFETY: zeroed POD structure.
    let mut info: USB_NODE_CONNECTION_INFORMATION_EX = unsafe { std::mem::zeroed() };
    info.ConnectionIndex = port;
    let size = std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX>() as DWORD;
    let mut returned: DWORD = 0;
    // SAFETY: in/out buffers point at `info` with the right size.
    let ok = unsafe {
        DeviceIoControl(
            hub.0,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
            &info as *const _ as *const c_void,
            size,
            &mut info as *mut _ as *mut c_void,
            size,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE || info.ConnectionStatus != USB_CONNECTION_STATUS_DEVICE_CONNECTED {
        return None;
    }
    let mut speed = match info.Speed {
        0 => Speed::Low,
        1 => Speed::Full,
        2 => Speed::High,
        3 => Speed::Super,
        _ => Speed::Unknown,
    };
    let mut v2 = USB_NODE_CONNECTION_INFORMATION_EX_V2 {
        ConnectionIndex: port,
        Length: std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as DWORD,
        SupportedUsbProtocols: 0,
        Flags: 0,
    };
    // SAFETY: as above with the V2 structure.
    let ok = unsafe {
        DeviceIoControl(
            hub.0,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
            &v2 as *const _ as *const c_void,
            v2.Length,
            &mut v2 as *mut _ as *mut c_void,
            v2.Length,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != FALSE {
        if v2.Flags & USB_NODE_CONNECTION_INFORMATION_EX_V2_OPERATING_AT_SUPERSPEED_PLUS_OR_HIGHER != 0 {
            speed = Speed::SuperPlus;
        } else if v2.Flags & USB_NODE_CONNECTION_INFORMATION_EX_V2_OPERATING_AT_SUPERSPEED_OR_HIGHER != 0 {
            speed = Speed::Super;
        }
    }
    Some(ConnectionInfo {
        device_descriptor: info.DeviceDescriptor,
        current_config: info.CurrentConfigurationValue,
        speed,
        address: info.DeviceAddress,
    })
}

/// Runs a GET_DESCRIPTOR request for the device on `port` through its hub.
fn hub_get_descriptor(hub: &OwnedHandle, port: u32, value: u16, index: u16, out: &mut [u8]) -> Result<usize> {
    let header = std::mem::size_of::<USB_DESCRIPTOR_REQUEST>();
    let len = out.len().min(u16::MAX as usize);
    let mut buf = vec![0u8; header + len];
    let req = USB_DESCRIPTOR_REQUEST {
        ConnectionIndex: port,
        bmRequest: 0x80,
        bRequest: request::GET_DESCRIPTOR,
        wValue: value,
        wIndex: index,
        wLength: len as u16,
    };
    // SAFETY: `buf` is at least `header` bytes long.
    unsafe { std::ptr::write_unaligned(buf.as_mut_ptr() as *mut USB_DESCRIPTOR_REQUEST, req) };
    let mut returned: DWORD = 0;
    // SAFETY: the same buffer serves as input and output, sized as declared.
    let ok = unsafe {
        DeviceIoControl(
            hub.0,
            IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
            buf.as_ptr() as *const c_void,
            buf.len() as DWORD,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as DWORD,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        return Err(os_error("hub descriptor request"));
    }
    let n = (returned as usize).saturating_sub(header).min(len);
    out[..n].copy_from_slice(&buf[header..header + n]);
    Ok(n)
}

fn hub_config_descriptors(hub: &OwnedHandle, port: u32, count: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for i in 0..count {
        let value = (descriptor_type::CONFIG as u16) << 8 | i as u16;
        let mut head = [0u8; 9];
        let Ok(9) = hub_get_descriptor(hub, port, value, 0, &mut head) else {
            break;
        };
        let total = u16::from_le_bytes([head[2], head[3]]) as usize;
        let mut full = vec![0u8; total.max(9)];
        let Ok(n) = hub_get_descriptor(hub, port, value, 0, &mut full) else {
            break;
        };
        full.truncate(n);
        out.extend(split_config_descriptors(&full).into_iter().take(1));
    }
    out
}

struct RootHub {
    devinst: DEVINST,
    bus: u8,
    path: String,
    instance: String,
}

fn enumerate() -> Result<Vec<DeviceInfo>> {
    let hub_set = DevInfoSet::for_interface(&GUID_DEVINTERFACE_USB_HUB)?;
    let dev_set = DevInfoSet::for_interface(&GUID_DEVINTERFACE_USB_DEVICE)?;

    // Root hubs define the buses, numbered in a stable (sorted) order.
    let mut roots: Vec<RootHub> = hub_set
        .interfaces(&GUID_DEVINTERFACE_USB_HUB)
        .into_iter()
        .filter_map(|(path, devinst)| {
            let instance = instance_id(devinst)?;
            is_root_hub(&instance).then_some(RootHub {
                devinst,
                bus: 0,
                path,
                instance,
            })
        })
        .collect();
    roots.sort_by(|a, b| a.instance.cmp(&b.instance));
    for (i, r) in roots.iter_mut().enumerate() {
        r.bus = (i + 1).min(255) as u8;
    }

    let mut out: Vec<DeviceInfo> = Vec::new();
    for r in &roots {
        out.push(root_hub_info(r));
    }

    let mut seen: Vec<DEVINST> = Vec::new();
    let mut candidates = dev_set.interfaces(&GUID_DEVINTERFACE_USB_DEVICE);
    candidates.extend(hub_set.interfaces(&GUID_DEVINTERFACE_USB_HUB));
    for (path, devinst) in candidates {
        if seen.contains(&devinst) {
            continue;
        }
        seen.push(devinst);
        let Some(instance) = instance_id(devinst) else { continue };
        if is_root_hub(&instance) {
            continue;
        }
        if let Some(info) = describe_device(&roots, path, devinst) {
            out.push(info);
        }
    }
    out.sort_by_key(|d| (d.bus_number, d.address));
    Ok(out)
}

fn root_hub_info(r: &RootHub) -> DeviceInfo {
    let super_speed = r.instance.to_ascii_uppercase().contains("ROOT_HUB3");
    // Vendor/product of the host controller, when it is a PCI device.
    let (mut vid, mut pid) = (0u16, 0u16);
    if let Some(p) = parent(r.devinst)
        && let Some(hwid) = registry_string(p, CM_DRP_HARDWAREID)
    {
        let upper = hwid.to_ascii_uppercase();
        if let Some(i) = upper.find("VEN_") {
            vid = u16::from_str_radix(upper.get(i + 4..i + 8).unwrap_or(""), 16).unwrap_or(0);
        }
        if let Some(i) = upper.find("DEV_") {
            pid = u16::from_str_radix(upper.get(i + 4..i + 8).unwrap_or(""), 16).unwrap_or(0);
        }
    }
    let desc = DeviceDescriptor {
        usb_version: Version(if super_speed { 0x0300 } else { 0x0200 }),
        class: crate::types::class::HUB,
        sub_class: 0,
        protocol: if super_speed { 3 } else { 1 },
        max_packet_size_0: if super_speed { 9 } else { 64 },
        vendor_id: vid,
        product_id: pid,
        device_version: Version(0),
        manufacturer_string_index: 0,
        product_string_index: 0,
        serial_number_string_index: 0,
        num_configurations: 1,
    };
    DeviceInfo {
        bus_number: r.bus,
        address: 0,
        port_numbers: Vec::new(),
        speed: if super_speed { Speed::Super } else { Speed::High },
        device_descriptor: desc,
        configs: Vec::new(),
        active_config: Some(1),
        location: Location {
            path: r.path.clone(),
            hub_path: None,
            port: 0,
            winusb: false,
            functions: Vec::new(),
        },
    }
}

fn describe_device(roots: &[RootHub], path: String, devinst: DEVINST) -> Option<DeviceInfo> {
    // Walk up to the root hub, collecting the port at each level.
    let mut ports_rev = Vec::new();
    let mut cur = devinst;
    let bus;
    let mut hub_path = None;
    loop {
        let port = registry_u32(cur, CM_DRP_ADDRESS)? as u8;
        ports_rev.push(port);
        let p = parent(cur)?;
        if hub_path.is_none() {
            let pid = instance_id(p)?;
            hub_path = interface_paths(&GUID_DEVINTERFACE_USB_HUB, &pid).into_iter().next();
        }
        if let Some(r) = roots.iter().find(|r| r.devinst == p) {
            bus = r.bus;
            break;
        }
        let pid = instance_id(p)?;
        if !pid.to_ascii_uppercase().starts_with("USB\\") {
            return None; // not under a USB hub we know
        }
        cur = p;
        if ports_rev.len() > 7 {
            return None;
        }
    }
    ports_rev.reverse();
    let port = *ports_rev.last()? as u32;
    let hub_path = hub_path?;
    let hub = open_hub(&hub_path).ok()?;
    let conn = hub_connection_info(&hub, port)?;
    let device_descriptor = DeviceDescriptor::from_bytes(&conn.device_descriptor).ok()?;
    let configs = hub_config_descriptors(&hub, port, device_descriptor.num_configurations);

    let service = registry_string(devinst, CM_DRP_SERVICE).unwrap_or_default();
    let winusb = service.eq_ignore_ascii_case("WinUSB");
    let mut functions = Vec::new();
    if service.eq_ignore_ascii_case("usbccgp") {
        for child in children(devinst) {
            let Some(id) = instance_id(child) else { continue };
            let child_service = registry_string(child, CM_DRP_SERVICE).unwrap_or_default();
            if !child_service.eq_ignore_ascii_case("WinUSB") {
                continue;
            }
            let upper = id.to_ascii_uppercase();
            let Some(i) = upper.find("&MI_") else { continue };
            let Ok(first) = u8::from_str_radix(upper.get(i + 4..i + 6).unwrap_or(""), 16) else {
                continue;
            };
            let path = device_interface_guids(child).iter().flat_map(|g| interface_paths(g, &id)).next();
            if let Some(path) = path {
                functions.push(Function {
                    first_interface: first,
                    path,
                });
            }
        }
        functions.sort_by_key(|f| f.first_interface);
    }

    Some(DeviceInfo {
        bus_number: bus,
        address: conn.address.min(255) as u8,
        port_numbers: ports_rev,
        speed: conn.speed,
        device_descriptor,
        configs,
        active_config: Some(conn.current_config),
        location: Location {
            path,
            hub_path: Some(hub_path),
            port,
            winusb,
            functions,
        },
    })
}

// ----- isochronous transfers ---------------------------------------------------------------

/// The WinUSB isochronous entry points, resolved at run time because they
/// only exist from Windows 8.1 onwards.
/// `WinUsb_RegisterIsochBuffer`.
type IsochRegisterFn = unsafe extern "system" fn(WINUSB_INTERFACE_HANDLE, u8, *mut u8, u32, *mut WINUSB_ISOCH_BUFFER_HANDLE) -> BOOL;
/// `WinUsb_UnregisterIsochBuffer`.
type IsochUnregisterFn = unsafe extern "system" fn(WINUSB_ISOCH_BUFFER_HANDLE) -> BOOL;
/// `WinUsb_ReadIsochPipeAsap`.
type IsochReadFn =
    unsafe extern "system" fn(WINUSB_ISOCH_BUFFER_HANDLE, u32, u32, BOOL, u32, *mut USBD_ISO_PACKET_DESCRIPTOR, *mut OVERLAPPED) -> BOOL;
/// `WinUsb_WriteIsochPipeAsap`.
type IsochWriteFn = unsafe extern "system" fn(WINUSB_ISOCH_BUFFER_HANDLE, u32, u32, BOOL, *mut OVERLAPPED) -> BOOL;

struct IsochApi {
    register: IsochRegisterFn,
    unregister: IsochUnregisterFn,
    read_asap: IsochReadFn,
    write_asap: IsochWriteFn,
}

fn isoch_api() -> Option<&'static IsochApi> {
    static API: OnceLock<Option<IsochApi>> = OnceLock::new();
    API.get_or_init(|| {
        let register = proc_address("winusb.dll", b"WinUsb_RegisterIsochBuffer\0")?;
        let unregister = proc_address("winusb.dll", b"WinUsb_UnregisterIsochBuffer\0")?;
        let read_asap = proc_address("winusb.dll", b"WinUsb_ReadIsochPipeAsap\0")?;
        let write_asap = proc_address("winusb.dll", b"WinUsb_WriteIsochPipeAsap\0")?;
        // SAFETY: these are the documented signatures of the four exports.
        unsafe {
            Some(IsochApi {
                register: std::mem::transmute::<*const c_void, IsochRegisterFn>(register),
                unregister: std::mem::transmute::<*const c_void, IsochUnregisterFn>(unregister),
                read_asap: std::mem::transmute::<*const c_void, IsochReadFn>(read_asap),
                write_asap: std::mem::transmute::<*const c_void, IsochWriteFn>(write_asap),
            })
        }
    })
    .as_ref()
}

/// A buffer registered with WinUSB for the duration of one submission.
struct IsochBuffer {
    handle: WINUSB_ISOCH_BUFFER_HANDLE,
}

// SAFETY: the handle is only ever passed back to WinUSB.
unsafe impl Send for IsochBuffer {}

impl Drop for IsochBuffer {
    fn drop(&mut self) {
        if let Some(api) = isoch_api() {
            // SAFETY: the handle came from `WinUsb_RegisterIsochBuffer` and is
            // released exactly once, after the transfer has completed.
            unsafe { (api.unregister)(self.handle) };
        }
    }
}

/// Maps a `USBD_STATUS` to the per-packet status the caller sees.
fn usbd_packet_status(status: u32) -> TransferStatus {
    if usbd_success(status) {
        return TransferStatus::Completed;
    }
    match status {
        USBD_STATUS_STALL_PID => TransferStatus::Stall,
        USBD_STATUS_DATA_OVERRUN | USBD_STATUS_BUFFER_OVERRUN => TransferStatus::Overflow,
        USBD_STATUS_CANCELED => TransferStatus::Cancelled,
        USBD_STATUS_DEV_NOT_RESPONDING | USBD_STATUS_DEVICE_GONE => TransferStatus::NoDevice,
        _ => TransferStatus::Error,
    }
}

// ----- WinUSB handles --------------------------------------------------------------------

/// An opened WinUSB device file with its primary interface handle.
struct WinUsbDevice {
    file: OwnedHandle,
    winusb: WINUSB_INTERFACE_HANDLE,
}

// SAFETY: WinUSB handles are plain kernel/driver handles usable from any thread.
unsafe impl Send for WinUsbDevice {}
// SAFETY: as above.
unsafe impl Sync for WinUsbDevice {}

impl WinUsbDevice {
    fn open(path: &str, ctx: &Context) -> Result<WinUsbDevice> {
        let wpath = wide(path);
        // SAFETY: valid NUL-terminated path; overlapped I/O requested.
        let h = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(os_error("open device"));
        }
        let file = OwnedHandle(h);
        let mut winusb: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
        // SAFETY: valid file handle and out-pointer.
        if unsafe { WinUsb_Initialize(file.0, &mut winusb) } == FALSE {
            return Err(os_error("WinUsb_Initialize"));
        }
        // SAFETY: associating our file with the context's completion port.
        if unsafe { CreateIoCompletionPort(file.0, ctx.iocp.0, 0, 0) }.is_null() {
            let e = os_error("associate completion port");
            // SAFETY: freeing the handle we just obtained.
            unsafe { WinUsb_Free(winusb) };
            return Err(e);
        }
        Ok(WinUsbDevice { file, winusb })
    }
}

impl Drop for WinUsbDevice {
    fn drop(&mut self) {
        // SAFETY: freeing before the file closes, exactly once.
        unsafe { WinUsb_Free(self.winusb) };
    }
}

struct Claimed {
    dev: Arc<WinUsbDevice>,
    handle: WINUSB_INTERFACE_HANDLE,
    /// `true` when `handle` came from `WinUsb_GetAssociatedInterface` and
    /// must be freed by us.
    owned: bool,
    alt: u8,
}

// SAFETY: see `WinUsbDevice`.
unsafe impl Send for Claimed {}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: freeing an associated interface handle we own.
            unsafe { WinUsb_Free(self.handle) };
        }
    }
}

pub(crate) struct Handle {
    ctx: Arc<Context>,
    info: Arc<DeviceInfo>,
    cfg: Option<ConfigDescriptor>,
    main: Option<Arc<WinUsbDevice>>,
    /// Opened composite functions, keyed by first interface number.
    functions: Mutex<HashMap<u8, Arc<WinUsbDevice>>>,
    claimed: Mutex<HashMap<u8, Claimed>>,
    /// Outstanding overlapped requests keyed by OVERLAPPED address.
    pending: Mutex<HashMap<usize, Arc<Inner>>>,
    /// Isochronous transfers in flight per endpoint. A submission joins the
    /// running stream when this is non-zero, and starts a new one otherwise.
    iso_inflight: Mutex<HashMap<u8, u32>>,
    disconnected: AtomicBool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.ctx.wake();
    }
}

impl Handle {
    fn not_winusb() -> Error {
        Error::with_message(ErrorKind::NotSupported, "device is not bound to the WinUSB driver")
    }

    /// The function covering `interface`, opening it on first use.
    fn function_for(&self, interface: u8) -> Result<(u8, Arc<WinUsbDevice>)> {
        if let Some(main) = &self.main {
            return Ok((0, Arc::clone(main)));
        }
        let loc = &self.info.location;
        // Interface association descriptors group interfaces into functions.
        let first = self
            .cfg
            .as_ref()
            .map(|c| c.interface_associations())
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.first_interface <= interface && interface < a.first_interface.saturating_add(a.interface_count))
            .map(|a| a.first_interface)
            .unwrap_or(interface);
        let f = loc
            .functions
            .iter()
            .find(|f| f.first_interface == first)
            .ok_or_else(Self::not_winusb)?;
        let mut opened = lock(&self.functions);
        if let Some(d) = opened.get(&first) {
            return Ok((first, Arc::clone(d)));
        }
        let d = Arc::new(WinUsbDevice::open(&f.path, &self.ctx)?);
        opened.insert(first, Arc::clone(&d));
        Ok((first, d))
    }

    /// Any WinUSB interface handle suitable for control transfers.
    fn control_handle(&self) -> Option<WINUSB_INTERFACE_HANDLE> {
        if let Some(main) = &self.main {
            return Some(main.winusb);
        }
        if let Some(c) = lock(&self.claimed).values().next() {
            return Some(c.handle);
        }
        if let Some(d) = lock(&self.functions).values().next() {
            return Some(d.winusb);
        }
        let first = self.info.location.functions.first()?.first_interface;
        self.function_for(first).ok().map(|(_, d)| d.winusb)
    }

    /// The claimed interface owning an endpoint, per its current alternate
    /// setting.
    fn pipe_owner(&self, endpoint: u8) -> Result<(Arc<WinUsbDevice>, WINUSB_INTERFACE_HANDLE)> {
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "no configuration descriptor"))?;
        let claimed = lock(&self.claimed);
        for (num, c) in claimed.iter() {
            if let Some(i) = cfg.interface(*num)
                && let Some(alt) = i.alt_setting(c.alt)
                && alt.endpoint(endpoint).is_some()
            {
                return Ok((Arc::clone(&c.dev), c.handle));
            }
        }
        Err(Error::with_message(
            ErrorKind::NotFound,
            "endpoint does not belong to a claimed interface",
        ))
    }

    pub(crate) fn active_configuration(&self) -> Result<Option<u8>> {
        Ok(self.info.active_config)
    }

    pub(crate) fn set_configuration(&self, value: u8) -> Result<()> {
        if Some(value) == self.info.active_config {
            Ok(())
        } else {
            Err(Error::with_message(
                ErrorKind::NotSupported,
                "WinUSB cannot change the configuration",
            ))
        }
    }

    pub(crate) fn claim_interface(&self, interface: u8) -> Result<()> {
        if lock(&self.claimed).contains_key(&interface) {
            return Ok(());
        }
        let (first, dev) = self.function_for(interface)?;
        let index = interface - first;
        let (handle, owned) = if index == 0 {
            (dev.winusb, false)
        } else {
            let mut h: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
            // SAFETY: valid primary handle and out-pointer.
            if unsafe { WinUsb_GetAssociatedInterface(dev.winusb, index - 1, &mut h) } == FALSE {
                return Err(os_error("WinUsb_GetAssociatedInterface"));
            }
            (h, true)
        };
        let mut alt: u8 = 0;
        // SAFETY: valid handle and out-pointer; failure just leaves alt = 0.
        unsafe { WinUsb_GetCurrentAlternateSetting(handle, &mut alt) };
        lock(&self.claimed).insert(interface, Claimed { dev, handle, owned, alt });
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
        let c = claimed
            .get_mut(&interface)
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "interface not claimed"))?;
        // SAFETY: valid interface handle.
        if unsafe { WinUsb_SetCurrentAlternateSetting(c.handle, alt) } == FALSE {
            return Err(os_error("WinUsb_SetCurrentAlternateSetting"));
        }
        c.alt = alt;
        Ok(())
    }

    pub(crate) fn clear_halt(&self, endpoint: u8) -> Result<()> {
        let (_dev, h) = self.pipe_owner(endpoint)?;
        // SAFETY: valid interface handle.
        if unsafe { WinUsb_ResetPipe(h, endpoint) } == FALSE {
            return Err(os_error("WinUsb_ResetPipe"));
        }
        // Whatever isochronous stream was running on the pipe is over.
        lock(&self.iso_inflight).remove(&endpoint);
        Ok(())
    }

    pub(crate) fn reset(&self) -> Result<()> {
        Err(Error::with_message(ErrorKind::NotSupported, "WinUSB cannot reset a device"))
    }

    pub(crate) fn kernel_driver_active(&self, interface: u8) -> Result<bool> {
        if self.main.is_some() {
            return Ok(false);
        }
        Ok(self.function_for(interface).is_err())
    }

    pub(crate) fn detach_kernel_driver(&self, _interface: u8) -> Result<()> {
        Err(Error::with_message(
            ErrorKind::NotSupported,
            "drivers cannot be detached on Windows",
        ))
    }

    pub(crate) fn attach_kernel_driver(&self, _interface: u8) -> Result<()> {
        Err(Error::with_message(
            ErrorKind::NotSupported,
            "drivers cannot be attached on Windows",
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

        let entry = Box::new(OvEntry {
            // SAFETY: OVERLAPPED is POD; zero is its idle state.
            ov: unsafe { std::mem::zeroed() },
            inner: Arc::clone(inner),
            iso_descs: Vec::new(),
            iso_buffer: None,
        });
        let entry = Box::into_raw(entry);
        let ov = entry as *mut OVERLAPPED;
        let mut transferred: u32 = 0;

        let (file, ok) = match inner.kind {
            TransferType::Control => {
                let setup = ControlSetup::from_bytes(st.buffer[..8].try_into().expect("checked by caller"));
                let data_len = (buf_len - 8).min(setup.length as usize) as u32;
                match self.control_handle() {
                    Some(h) => {
                        let packet = WINUSB_SETUP_PACKET {
                            RequestType: setup.request_type,
                            Request: setup.request,
                            Value: setup.value,
                            Index: setup.index,
                            Length: data_len as u16,
                        };
                        // SAFETY: buffer valid for `data_len` bytes past the setup packet; `ov` outlives the request.
                        let ok = unsafe { WinUsb_ControlTransfer(h, packet, buf_ptr.add(8), data_len, &mut transferred, ov) };
                        (self.control_file(), ok)
                    }
                    None => {
                        // SAFETY: reclaiming the box we just leaked; nothing was submitted.
                        drop(unsafe { Box::from_raw(entry) });
                        drop(sub);
                        return self.hub_control(inner, &setup, &mut st.buffer[8..]);
                    }
                }
            }
            TransferType::Bulk | TransferType::Interrupt => {
                let (dev, h) = match self.pipe_owner(inner.endpoint) {
                    Ok(x) => x,
                    Err(e) => {
                        // SAFETY: nothing submitted yet.
                        drop(unsafe { Box::from_raw(entry) });
                        return Err(e);
                    }
                };
                let ok = if inner.endpoint & 0x80 != 0 {
                    // SAFETY: buffer valid for `buf_len` bytes; `ov` outlives the request.
                    unsafe { WinUsb_ReadPipe(h, inner.endpoint, buf_ptr, buf_len as u32, &mut transferred, ov) }
                } else {
                    let zlp: u8 = st.flags.zero_packet as u8;
                    // SAFETY: policy value is one byte as documented.
                    unsafe { WinUsb_SetPipePolicy(h, inner.endpoint, SHORT_PACKET_TERMINATE, 1, &zlp as *const u8 as *const c_void) };
                    // SAFETY: as for ReadPipe.
                    unsafe { WinUsb_WritePipe(h, inner.endpoint, buf_ptr, buf_len as u32, &mut transferred, ov) }
                };
                (dev.file.0, ok)
            }
            TransferType::Isochronous => {
                let prepared = match isoch_api() {
                    Some(api) => self.prepare_isochronous(api, inner, st, buf_ptr, buf_len, entry, ov),
                    None => Err(Error::with_message(
                        ErrorKind::NotSupported,
                        "isochronous transfers need the WinUSB isoch API, which arrives with Windows 8.1",
                    )),
                };
                match prepared {
                    Ok(pair) => pair,
                    Err(e) => {
                        // SAFETY: nothing was submitted, so the kernel never
                        // saw `ov`; dropping the box also unregisters the
                        // isochronous buffer if one was registered.
                        drop(unsafe { Box::from_raw(entry) });
                        return Err(e);
                    }
                }
            }
        };

        if ok == FALSE {
            let e = last_error();
            if e != ERROR_IO_PENDING {
                // SAFETY: the request was rejected, so the kernel never saw `ov`.
                drop(unsafe { Box::from_raw(entry) });
                return Err(Error::from_code(errno_kind(e as i32), e as i32).context("submit transfer"));
            }
        }
        // Whether it completed inline or is pending, a completion packet is
        // on its way to the port.
        lock(&self.pending).insert(ov as usize, Arc::clone(inner));
        *sub = Some(WinSub {
            ov: ov as usize,
            file,
            deadline: (!st.timeout.is_zero()).then(|| Instant::now() + st.timeout),
            cancelled: false,
            timed_out: false,
        });
        drop(sub);
        self.ctx.wake();
        Ok(())
    }

    /// `wMaxPacketSize` of an endpoint in the alternate setting currently
    /// selected on the interface that owns it.
    fn endpoint_max_packet(&self, address: u8) -> Result<u32> {
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "no configuration descriptor"))?;
        let claimed = lock(&self.claimed);
        for (number, c) in claimed.iter() {
            if let Some(interface) = cfg.interface(*number)
                && let Some(alt) = interface.alt_setting(c.alt)
                && let Some(endpoint) = alt.endpoint(address)
            {
                return Ok(endpoint.max_packet_size());
            }
        }
        Err(Error::with_message(
            ErrorKind::NotFound,
            "endpoint does not belong to a claimed interface",
        ))
    }

    /// Validates the packet layout, registers the buffer and starts one
    /// isochronous transfer. On success the caller owns the submission, and
    /// `entry` owns the registration until the transfer completes.
    #[allow(clippy::too_many_arguments)]
    fn prepare_isochronous(
        &self,
        api: &'static IsochApi,
        inner: &Arc<Inner>,
        st: &State,
        buf_ptr: *mut u8,
        buf_len: usize,
        entry: *mut OvEntry,
        ov: *mut OVERLAPPED,
    ) -> Result<(HANDLE, BOOL)> {
        let endpoint = inner.endpoint;
        let is_in = endpoint & 0x80 != 0;
        let (dev, interface) = self.pipe_owner(endpoint)?;
        let max_packet = self.endpoint_max_packet(endpoint)?;
        let packets = &st.iso_packets;

        // WinUSB slices the buffer itself, at the endpoint's packet size,
        // rather than following a caller-supplied packet table.
        for (i, packet) in packets.iter().enumerate() {
            let last = i + 1 == packets.len();
            let acceptable = packet.length == max_packet || (!is_in && last && packet.length <= max_packet);
            if !acceptable {
                return Err(Error::with_message(
                    ErrorKind::InvalidParam,
                    "WinUSB splits isochronous transfers at the endpoint's packet size: every packet must be exactly that long, except the last packet of an OUT transfer, which may be shorter",
                ));
            }
        }
        let total: usize = packets.iter().map(|p| p.length as usize).sum();
        if total > buf_len {
            return Err(Error::with_message(
                ErrorKind::InvalidParam,
                "isochronous packets do not fit the buffer",
            ));
        }

        let mut handle: WINUSB_ISOCH_BUFFER_HANDLE = std::ptr::null_mut();
        // SAFETY: the buffer is valid for `buf_len` bytes and stays put until
        // the transfer completes, which is also when it is unregistered.
        if unsafe { (api.register)(interface, endpoint, buf_ptr, buf_len as u32, &mut handle) } == FALSE {
            return Err(os_error("WinUsb_RegisterIsochBuffer"));
        }
        // SAFETY: `entry` is our own allocation; the kernel has not seen it yet.
        let descriptors = unsafe {
            (*entry).iso_buffer = Some(IsochBuffer { handle });
            if is_in {
                (*entry).iso_descs = vec![USBD_ISO_PACKET_DESCRIPTOR::default(); packets.len()];
            }
            (*entry).iso_descs.as_mut_ptr()
        };

        // Joining the stream already running on this endpoint keeps the
        // packets contiguous; with nothing in flight, start a fresh one.
        let mut inflight = lock(&self.iso_inflight);
        let continue_stream = if inflight.get(&endpoint).is_some_and(|&n| n > 0) {
            TRUE
        } else {
            FALSE
        };
        // SAFETY: valid registration, buffer range and `OVERLAPPED`; for a
        // read the descriptor array holds one entry per packet.
        let ok = unsafe {
            if is_in {
                (api.read_asap)(handle, 0, total as u32, continue_stream, packets.len() as u32, descriptors, ov)
            } else {
                (api.write_asap)(handle, 0, total as u32, continue_stream, ov)
            }
        };
        if ok != FALSE || last_error() == ERROR_IO_PENDING {
            *inflight.entry(endpoint).or_insert(0) += 1;
        }
        Ok((dev.file.0, ok))
    }

    fn control_file(&self) -> HANDLE {
        if let Some(m) = &self.main {
            return m.file.0;
        }
        if let Some(c) = lock(&self.claimed).values().next() {
            return c.dev.file.0;
        }
        lock(&self.functions)
            .values()
            .next()
            .map(|d| d.file.0)
            .unwrap_or(std::ptr::null_mut())
    }

    /// Serves a standard GET_DESCRIPTOR request through the parent hub for
    /// devices we cannot open with WinUSB. Completion is reported from the
    /// event thread like any other.
    fn hub_control(&self, inner: &Arc<Inner>, setup: &ControlSetup, data: &mut [u8]) -> Result<()> {
        let is_get_descriptor = setup.request_type == 0x80 && setup.request == request::GET_DESCRIPTOR;
        let Some(hub_path) = self.info.location.hub_path.as_deref().filter(|_| is_get_descriptor) else {
            return Err(Self::not_winusb());
        };
        let hub = open_hub(hub_path)?;
        let len = data.len().min(setup.length as usize);
        let result = hub_get_descriptor(&hub, self.info.location.port, setup.value, setup.index, &mut data[..len]);
        let (status, n) = match result {
            Ok(n) => (TransferStatus::Completed, n),
            Err(e) if e.kind() == ErrorKind::NoDevice => (TransferStatus::NoDevice, 0),
            Err(_) => (TransferStatus::Stall, 0),
        };
        self.ctx.defer(inner, status, n);
        Ok(())
    }

    pub(crate) fn cancel(&self, inner: &Arc<Inner>) -> Result<()> {
        let mut g = lock(&inner.sys.sub);
        let Some(sub) = g.as_mut() else {
            return Err(Error::with_message(ErrorKind::NotFound, "transfer is not in flight"));
        };
        // SAFETY: cancelling the specific request identified by its OVERLAPPED.
        if unsafe { CancelIoEx(sub.file, sub.ov as *mut OVERLAPPED) } != FALSE {
            sub.cancelled = true;
        }
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
            // SAFETY: as in `cancel`.
            if unsafe { CancelIoEx(sub.file, sub.ov as *mut OVERLAPPED) } != FALSE {
                sub.timed_out = true;
            }
            sub.deadline = None;
        }
    }

    fn finish(&self, inner: &Arc<Inner>, ov: usize, bytes: usize, err: DWORD, iso_descs: &[USBD_ISO_PACKET_DESCRIPTOR]) {
        lock(&self.pending).remove(&ov);
        let Some(sub) = lock(&inner.sys.sub).take() else { return };
        let status = if sub.timed_out {
            TransferStatus::TimedOut
        } else if sub.cancelled {
            TransferStatus::Cancelled
        } else {
            match err {
                ERROR_SUCCESS => TransferStatus::Completed,
                ERROR_OPERATION_ABORTED => TransferStatus::Cancelled,
                ERROR_SEM_TIMEOUT | ERROR_TIMEOUT => TransferStatus::TimedOut,
                ERROR_GEN_FAILURE => TransferStatus::Stall,
                ERROR_DEVICE_NOT_CONNECTED | ERROR_NO_SUCH_DEVICE | ERROR_FILE_NOT_FOUND | ERROR_BAD_COMMAND => {
                    self.disconnected.store(true, Ordering::Relaxed);
                    TransferStatus::NoDevice
                }
                ERROR_MORE_DATA | ERROR_BUFFER_OVERFLOW => TransferStatus::Overflow,
                _ => TransferStatus::Error,
            }
        };
        if inner.kind == TransferType::Isochronous {
            self.finish_isochronous(inner, status, bytes, iso_descs);
            return;
        }
        inner.complete(status, bytes, None);
    }

    /// Turns one completed isochronous submission into per-packet results.
    fn finish_isochronous(&self, inner: &Arc<Inner>, status: TransferStatus, bytes: usize, iso_descs: &[USBD_ISO_PACKET_DESCRIPTOR]) {
        {
            let mut inflight = lock(&self.iso_inflight);
            let remaining = inflight.entry(inner.endpoint).or_insert(0);
            *remaining = remaining.saturating_sub(1);
            if status != TransferStatus::Completed {
                // The stream is broken; the next submission starts a new one.
                *remaining = 0;
            }
        }
        let requested: Vec<u32> = {
            let state = inner.lock();
            state.iso_packets.iter().map(|p| p.length).collect()
        };
        let mut packets = Vec::with_capacity(requested.len());
        let actual = if iso_descs.is_empty() {
            // An OUT transfer: WinUSB reports one total rather than a table,
            // so spread it across the packets in order.
            let mut remaining = bytes;
            for &length in &requested {
                let took = remaining.min(length as usize);
                remaining -= took;
                packets.push(IsoPacket {
                    length,
                    actual_length: took as u32,
                    status,
                });
            }
            bytes
        } else {
            let mut total = 0usize;
            for (i, &length) in requested.iter().enumerate() {
                let descriptor = iso_descs.get(i).copied().unwrap_or_default();
                total += descriptor.Length as usize;
                packets.push(IsoPacket {
                    length,
                    actual_length: descriptor.Length,
                    status: usbd_packet_status(descriptor.Status),
                });
            }
            total
        };
        inner.complete(status, actual, Some(packets));
    }
}

// ----- per-transfer state ------------------------------------------------------------------

struct WinSub {
    ov: usize,
    file: HANDLE,
    deadline: Option<Instant>,
    cancelled: bool,
    timed_out: bool,
}

// SAFETY: the raw handle is only used for CancelIoEx, which is thread-safe.
unsafe impl Send for WinSub {}

/// Backend state stored inside every transfer.
#[derive(Default)]
pub(crate) struct TransferData {
    sub: Mutex<Option<WinSub>>,
}
