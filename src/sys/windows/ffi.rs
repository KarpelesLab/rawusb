//! Win32 declarations used by the Windows backend: kernel32 (files, I/O
//! completion ports), setupapi/cfgmgr32 (device enumeration), advapi32
//! (device registry keys) and winusb.

#![allow(non_camel_case_types, non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub(crate) type HANDLE = *mut c_void;
pub(crate) type HKEY = *mut c_void;
pub(crate) type BOOL = i32;
pub(crate) type DWORD = u32;
pub(crate) type HDEVINFO = *mut c_void;
pub(crate) type DEVINST = u32;
pub(crate) type CONFIGRET = u32;
pub(crate) type WINUSB_INTERFACE_HANDLE = *mut c_void;

pub(crate) const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub(crate) const TRUE: BOOL = 1;
pub(crate) const FALSE: BOOL = 0;

pub(crate) const GENERIC_READ: DWORD = 0x8000_0000;
pub(crate) const GENERIC_WRITE: DWORD = 0x4000_0000;
pub(crate) const FILE_SHARE_READ: DWORD = 0x1;
pub(crate) const FILE_SHARE_WRITE: DWORD = 0x2;
pub(crate) const OPEN_EXISTING: DWORD = 3;
pub(crate) const FILE_ATTRIBUTE_NORMAL: DWORD = 0x80;
pub(crate) const FILE_FLAG_OVERLAPPED: DWORD = 0x4000_0000;
pub(crate) const INFINITE: DWORD = 0xFFFF_FFFF;

pub(crate) const ERROR_SUCCESS: DWORD = 0;
pub(crate) const ERROR_INVALID_FUNCTION: DWORD = 1;
pub(crate) const ERROR_FILE_NOT_FOUND: DWORD = 2;
pub(crate) const ERROR_PATH_NOT_FOUND: DWORD = 3;
pub(crate) const ERROR_ACCESS_DENIED: DWORD = 5;
pub(crate) const ERROR_INVALID_HANDLE: DWORD = 6;
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: DWORD = 8;
pub(crate) const ERROR_BAD_COMMAND: DWORD = 22;
pub(crate) const ERROR_GEN_FAILURE: DWORD = 31;
pub(crate) const ERROR_SHARING_VIOLATION: DWORD = 32;
pub(crate) const ERROR_NOT_SUPPORTED: DWORD = 50;
pub(crate) const ERROR_INVALID_PARAMETER: DWORD = 87;
pub(crate) const ERROR_SEM_TIMEOUT: DWORD = 121;
pub(crate) const ERROR_INSUFFICIENT_BUFFER: DWORD = 122;
pub(crate) const ERROR_BUSY: DWORD = 170;
pub(crate) const ERROR_MORE_DATA: DWORD = 234;
pub(crate) const WAIT_TIMEOUT: DWORD = 258;
pub(crate) const ERROR_NO_MORE_ITEMS: DWORD = 259;
pub(crate) const ERROR_NO_SUCH_DEVICE: DWORD = 433;
pub(crate) const ERROR_OPERATION_ABORTED: DWORD = 995;
pub(crate) const ERROR_IO_PENDING: DWORD = 997;
pub(crate) const ERROR_DEVICE_NOT_CONNECTED: DWORD = 1167;
pub(crate) const ERROR_NOT_FOUND: DWORD = 1168;
pub(crate) const ERROR_TIMEOUT: DWORD = 1460;
pub(crate) const ERROR_BUFFER_OVERFLOW: DWORD = 111;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct GUID {
    pub(crate) Data1: u32,
    pub(crate) Data2: u16,
    pub(crate) Data3: u16,
    pub(crate) Data4: [u8; 8],
}

impl GUID {
    pub(crate) const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
        GUID {
            Data1: d1,
            Data2: d2,
            Data3: d3,
            Data4: d4,
        }
    }

    /// Parses `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` (braces optional).
    pub(crate) fn parse(s: &str) -> Option<GUID> {
        let s = s.trim().trim_start_matches('{').trim_end_matches('}');
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 5 || parts[3].len() != 4 || parts[4].len() != 12 {
            return None;
        }
        let d1 = u32::from_str_radix(parts[0], 16).ok()?;
        let d2 = u16::from_str_radix(parts[1], 16).ok()?;
        let d3 = u16::from_str_radix(parts[2], 16).ok()?;
        let mut d4 = [0u8; 8];
        let tail = format!("{}{}", parts[3], parts[4]);
        for (i, b) in d4.iter_mut().enumerate() {
            *b = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(GUID::new(d1, d2, d3, d4))
    }
}

/// `{A5DCBF10-6530-11D2-901F-00C04FB951ED}`
pub(crate) const GUID_DEVINTERFACE_USB_DEVICE: GUID =
    GUID::new(0xA5DCBF10, 0x6530, 0x11D2, [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED]);
/// `{F18A0E88-C30C-11D0-8815-00A0C906BED8}`
pub(crate) const GUID_DEVINTERFACE_USB_HUB: GUID = GUID::new(0xF18A0E88, 0xC30C, 0x11D0, [0x88, 0x15, 0x00, 0xA0, 0xC9, 0x06, 0xBE, 0xD8]);

#[repr(C)]
pub(crate) struct OVERLAPPED {
    pub(crate) Internal: usize,
    pub(crate) InternalHigh: usize,
    pub(crate) Offset: u32,
    pub(crate) OffsetHigh: u32,
    pub(crate) hEvent: HANDLE,
}

#[repr(C)]
pub(crate) struct SP_DEVINFO_DATA {
    pub(crate) cbSize: DWORD,
    pub(crate) ClassGuid: GUID,
    pub(crate) DevInst: DEVINST,
    pub(crate) Reserved: usize,
}

#[repr(C)]
pub(crate) struct SP_DEVICE_INTERFACE_DATA {
    pub(crate) cbSize: DWORD,
    pub(crate) InterfaceClassGuid: GUID,
    pub(crate) Flags: DWORD,
    pub(crate) Reserved: usize,
}

/// `cbSize` of `SP_DEVICE_INTERFACE_DETAIL_DATA_W`: the header packs this
/// structure on 32-bit Windows (6 bytes) and aligns it on 64-bit (8 bytes).
pub(crate) const SP_DEVICE_INTERFACE_DETAIL_DATA_W_SIZE: DWORD = if std::mem::size_of::<usize>() == 8 { 8 } else { 6 };

pub(crate) const DIGCF_PRESENT: DWORD = 0x02;
pub(crate) const DIGCF_ALLCLASSES: DWORD = 0x04;
pub(crate) const DIGCF_DEVICEINTERFACE: DWORD = 0x10;

pub(crate) const SPDRP_HARDWAREID: DWORD = 0x01;
pub(crate) const SPDRP_SERVICE: DWORD = 0x04;
pub(crate) const SPDRP_DRIVER: DWORD = 0x09;
pub(crate) const SPDRP_ADDRESS: DWORD = 0x1C;

pub(crate) const DICS_FLAG_GLOBAL: DWORD = 0x1;
pub(crate) const DIREG_DEV: DWORD = 0x1;
pub(crate) const KEY_READ: DWORD = 0x2_0019;
pub(crate) const REG_SZ: DWORD = 1;
pub(crate) const REG_MULTI_SZ: DWORD = 7;

/// Opaque `HCMNOTIFICATION`.
pub(crate) type HCMNOTIFICATION = *mut c_void;

pub(crate) const CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE: DWORD = 0;
pub(crate) const CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL: DWORD = 0;
pub(crate) const CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL: DWORD = 1;

/// The `u` member of `CM_NOTIFY_FILTER`. `DeviceInstance` is the largest arm
/// (`MAX_DEVICE_ID_LEN` wide characters), which is what fixes the size of the
/// structure the API validates against `cbSize`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union CM_NOTIFY_FILTER_UNION {
    pub(crate) DeviceInterface: CM_NOTIFY_FILTER_DEVICEINTERFACE,
    pub(crate) DeviceHandle: HANDLE,
    pub(crate) DeviceInstance: [u16; 200],
}

/// The `DeviceInterface` arm of [`CM_NOTIFY_FILTER_UNION`].
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CM_NOTIFY_FILTER_DEVICEINTERFACE {
    pub(crate) ClassGuid: GUID,
}

/// `CM_NOTIFY_FILTER`.
#[repr(C)]
pub(crate) struct CM_NOTIFY_FILTER {
    pub(crate) cbSize: DWORD,
    pub(crate) Flags: DWORD,
    pub(crate) FilterType: DWORD,
    pub(crate) Reserved: DWORD,
    pub(crate) u: CM_NOTIFY_FILTER_UNION,
}

// The API validates `cbSize` against its own idea of the structure, so the
// layout has to match exactly: four DWORDs followed by a union whose largest
// arm is `WCHAR InstanceId[MAX_DEVICE_ID_LEN]`, aligned to a pointer.
const _: () = assert!(std::mem::size_of::<CM_NOTIFY_FILTER>() == 16 + 400);
const _: () = assert!(std::mem::align_of::<CM_NOTIFY_FILTER>() == std::mem::align_of::<HANDLE>());

/// `PCM_NOTIFY_CALLBACK`.
pub(crate) type CM_NOTIFY_CALLBACK = unsafe extern "system" fn(
    hNotify: HCMNOTIFICATION,
    Context: *mut c_void,
    Action: DWORD,
    EventData: *mut c_void,
    EventDataSize: DWORD,
) -> DWORD;

pub(crate) const CR_SUCCESS: CONFIGRET = 0;
pub(crate) const CR_BUFFER_SMALL: CONFIGRET = 0x1A;
pub(crate) const CM_GET_DEVICE_INTERFACE_LIST_PRESENT: DWORD = 0;
pub(crate) const CM_DRP_HARDWAREID: DWORD = 0x02;
pub(crate) const CM_DRP_SERVICE: DWORD = 0x05;
pub(crate) const CM_DRP_ADDRESS: DWORD = 0x1D;
pub(crate) const REG_DISPOSITION_OPEN_EXISTING: DWORD = 1;
pub(crate) const CM_REGISTRY_HARDWARE: DWORD = 0;

// ----- usbioctl.h -------------------------------------------------------------

const FILE_DEVICE_USB: u32 = 0x22;
const fn ctl_code(function: u32) -> DWORD {
    (FILE_DEVICE_USB << 16) | (function << 2)
}
pub(crate) const IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION: DWORD = ctl_code(260);
pub(crate) const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX: DWORD = ctl_code(274);
pub(crate) const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2: DWORD = ctl_code(279);

#[repr(C)]
pub(crate) struct USB_NODE_CONNECTION_INFORMATION_EX {
    pub(crate) ConnectionIndex: u32,
    pub(crate) DeviceDescriptor: [u8; 18],
    pub(crate) CurrentConfigurationValue: u8,
    pub(crate) Speed: u8,
    pub(crate) DeviceIsHub: u8,
    pub(crate) DeviceAddress: u16,
    pub(crate) NumberOfOpenPipes: u32,
    pub(crate) ConnectionStatus: u32,
    // USB_PIPE_INFO PipeList[0] follows; we allocate room for 32.
    pub(crate) PipeList: [[u8; 12]; 32],
}

pub(crate) const USB_CONNECTION_STATUS_DEVICE_CONNECTED: u32 = 1;

#[repr(C)]
pub(crate) struct USB_NODE_CONNECTION_INFORMATION_EX_V2 {
    pub(crate) ConnectionIndex: u32,
    pub(crate) Length: u32,
    pub(crate) SupportedUsbProtocols: u32,
    pub(crate) Flags: u32,
}

pub(crate) const USB_NODE_CONNECTION_INFORMATION_EX_V2_OPERATING_AT_SUPERSPEED_OR_HIGHER: u32 = 0x1;
pub(crate) const USB_NODE_CONNECTION_INFORMATION_EX_V2_OPERATING_AT_SUPERSPEED_PLUS_OR_HIGHER: u32 = 0x4;

#[repr(C)]
pub(crate) struct USB_DESCRIPTOR_REQUEST {
    pub(crate) ConnectionIndex: u32,
    pub(crate) bmRequest: u8,
    pub(crate) bRequest: u8,
    pub(crate) wValue: u16,
    pub(crate) wIndex: u16,
    pub(crate) wLength: u16,
    // UCHAR Data[0] follows.
}

// ----- winusb.h -----------------------------------------------------------------

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(crate) struct WINUSB_SETUP_PACKET {
    pub(crate) RequestType: u8,
    pub(crate) Request: u8,
    pub(crate) Value: u16,
    pub(crate) Index: u16,
    pub(crate) Length: u16,
}

/// Opaque handle to a buffer registered for isochronous transfers.
pub(crate) type WINUSB_ISOCH_BUFFER_HANDLE = *mut c_void;

/// `USBD_ISO_PACKET_DESCRIPTOR`: one packet's slot in an isochronous buffer.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct USBD_ISO_PACKET_DESCRIPTOR {
    /// Offset of the packet data from the start of the registered buffer.
    pub(crate) Offset: u32,
    /// Bytes transferred, filled in by the driver.
    pub(crate) Length: u32,
    /// `USBD_STATUS` for this packet.
    pub(crate) Status: u32,
}

// Three ULONGs, no padding: WinUSB fills an array of these in place.
const _: () = assert!(std::mem::size_of::<USBD_ISO_PACKET_DESCRIPTOR>() == 12);

/// `USBD_SUCCESS(s)`: the top nibble carries the severity.
pub(crate) const fn usbd_success(status: u32) -> bool {
    status >> 28 == 0
}

pub(crate) const USBD_STATUS_STALL_PID: u32 = 0xC000_0004;
pub(crate) const USBD_STATUS_DEV_NOT_RESPONDING: u32 = 0xC000_0001;
pub(crate) const USBD_STATUS_DATA_OVERRUN: u32 = 0xC000_0008;
pub(crate) const USBD_STATUS_BUFFER_OVERRUN: u32 = 0xC000_3003;
pub(crate) const USBD_STATUS_CANCELED: u32 = 0xC001_0000;
pub(crate) const USBD_STATUS_DEVICE_GONE: u32 = 0xC000_1000;

// Passed by value to `WinUsb_ControlTransfer`; the wire layout of a USB
// setup packet, with no padding between the fields.
const _: () = assert!(std::mem::size_of::<WINUSB_SETUP_PACKET>() == 8);

pub(crate) const SHORT_PACKET_TERMINATE: DWORD = 0x01;
pub(crate) const AUTO_CLEAR_STALL: DWORD = 0x02;
pub(crate) const PIPE_TRANSFER_TIMEOUT: DWORD = 0x03;
pub(crate) const IGNORE_SHORT_PACKETS: DWORD = 0x04;
pub(crate) const ALLOW_PARTIAL_READS: DWORD = 0x05;
pub(crate) const AUTO_FLUSH: DWORD = 0x06;
pub(crate) const RAW_IO: DWORD = 0x07;
pub(crate) const MAXIMUM_TRANSFER_SIZE: DWORD = 0x08;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub(crate) fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: DWORD,
        dwShareMode: DWORD,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: DWORD,
        dwFlagsAndAttributes: DWORD,
        hTemplateFile: HANDLE,
    ) -> HANDLE;
    pub(crate) fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub(crate) fn GetLastError() -> DWORD;
    pub(crate) fn DeviceIoControl(
        hDevice: HANDLE,
        dwIoControlCode: DWORD,
        lpInBuffer: *const c_void,
        nInBufferSize: DWORD,
        lpOutBuffer: *mut c_void,
        nOutBufferSize: DWORD,
        lpBytesReturned: *mut DWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn CreateIoCompletionPort(
        FileHandle: HANDLE,
        ExistingCompletionPort: HANDLE,
        CompletionKey: usize,
        NumberOfConcurrentThreads: DWORD,
    ) -> HANDLE;
    pub(crate) fn GetQueuedCompletionStatus(
        CompletionPort: HANDLE,
        lpNumberOfBytesTransferred: *mut DWORD,
        lpCompletionKey: *mut usize,
        lpOverlapped: *mut *mut OVERLAPPED,
        dwMilliseconds: DWORD,
    ) -> BOOL;
    pub(crate) fn PostQueuedCompletionStatus(
        CompletionPort: HANDLE,
        dwNumberOfBytesTransferred: DWORD,
        dwCompletionKey: usize,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn CancelIoEx(hFile: HANDLE, lpOverlapped: *mut OVERLAPPED) -> BOOL;
    pub(crate) fn GetModuleHandleW(lpModuleName: *const u16) -> HANDLE;
    pub(crate) fn GetProcAddress(hModule: HANDLE, lpProcName: *const u8) -> *const c_void;
}

/// Looks up an exported function in a DLL that is already loaded.
///
/// Used for entry points that do not exist on every supported Windows
/// version: importing those statically would stop the whole process from
/// loading on an older system, whereas resolving them here simply reports
/// the feature as unsupported. `symbol` must be NUL-terminated.
///
/// `GetModuleHandleW` only finds a module that is already mapped, which is
/// the case here because every DLL looked up this way also provides symbols
/// this crate imports statically (see the `#[link]` blocks below). Dropping
/// one of those imports would quietly turn the dependent feature into
/// `NotSupported`.
pub(crate) fn proc_address(module: &str, symbol: &[u8]) -> Option<*const c_void> {
    debug_assert_eq!(symbol.last(), Some(&0), "symbol name must be NUL-terminated");
    let module = wide(module);
    // SAFETY: both strings are NUL-terminated and outlive the calls.
    unsafe {
        let handle = GetModuleHandleW(module.as_ptr());
        if handle.is_null() {
            return None;
        }
        let address = GetProcAddress(handle, symbol.as_ptr());
        if address.is_null() { None } else { Some(address) }
    }
}

#[link(name = "setupapi")]
unsafe extern "system" {
    pub(crate) fn SetupDiGetClassDevsW(ClassGuid: *const GUID, Enumerator: *const u16, hwndParent: *mut c_void, Flags: DWORD) -> HDEVINFO;
    pub(crate) fn SetupDiDestroyDeviceInfoList(DeviceInfoSet: HDEVINFO) -> BOOL;
    pub(crate) fn SetupDiEnumDeviceInfo(DeviceInfoSet: HDEVINFO, MemberIndex: DWORD, DeviceInfoData: *mut SP_DEVINFO_DATA) -> BOOL;
    pub(crate) fn SetupDiEnumDeviceInterfaces(
        DeviceInfoSet: HDEVINFO,
        DeviceInfoData: *mut SP_DEVINFO_DATA,
        InterfaceClassGuid: *const GUID,
        MemberIndex: DWORD,
        DeviceInterfaceData: *mut SP_DEVICE_INTERFACE_DATA,
    ) -> BOOL;
    pub(crate) fn SetupDiGetDeviceInterfaceDetailW(
        DeviceInfoSet: HDEVINFO,
        DeviceInterfaceData: *mut SP_DEVICE_INTERFACE_DATA,
        DeviceInterfaceDetailData: *mut c_void,
        DeviceInterfaceDetailDataSize: DWORD,
        RequiredSize: *mut DWORD,
        DeviceInfoData: *mut SP_DEVINFO_DATA,
    ) -> BOOL;
    pub(crate) fn SetupDiGetDeviceRegistryPropertyW(
        DeviceInfoSet: HDEVINFO,
        DeviceInfoData: *mut SP_DEVINFO_DATA,
        Property: DWORD,
        PropertyRegDataType: *mut DWORD,
        PropertyBuffer: *mut u8,
        PropertyBufferSize: DWORD,
        RequiredSize: *mut DWORD,
    ) -> BOOL;
    pub(crate) fn SetupDiOpenDeviceInfoW(
        DeviceInfoSet: HDEVINFO,
        DeviceInstanceId: *const u16,
        hwndParent: *mut c_void,
        OpenFlags: DWORD,
        DeviceInfoData: *mut SP_DEVINFO_DATA,
    ) -> BOOL;
    pub(crate) fn SetupDiOpenDevRegKey(
        DeviceInfoSet: HDEVINFO,
        DeviceInfoData: *mut SP_DEVINFO_DATA,
        Scope: DWORD,
        HwProfile: DWORD,
        KeyType: DWORD,
        samDesired: DWORD,
    ) -> HKEY;
}

#[link(name = "cfgmgr32")]
unsafe extern "system" {
    pub(crate) fn CM_Get_Parent(pdnDevInst: *mut DEVINST, dnDevInst: DEVINST, ulFlags: DWORD) -> CONFIGRET;
    pub(crate) fn CM_Get_Child(pdnDevInst: *mut DEVINST, dnDevInst: DEVINST, ulFlags: DWORD) -> CONFIGRET;
    pub(crate) fn CM_Get_Sibling(pdnDevInst: *mut DEVINST, dnDevInst: DEVINST, ulFlags: DWORD) -> CONFIGRET;
    pub(crate) fn CM_Get_Device_IDW(dnDevInst: DEVINST, Buffer: *mut u16, BufferLen: DWORD, ulFlags: DWORD) -> CONFIGRET;
    pub(crate) fn CM_Get_Device_Interface_List_SizeW(
        pulLen: *mut DWORD,
        InterfaceClassGuid: *const GUID,
        pDeviceID: *const u16,
        ulFlags: DWORD,
    ) -> CONFIGRET;
    pub(crate) fn CM_Get_Device_Interface_ListW(
        InterfaceClassGuid: *const GUID,
        pDeviceID: *const u16,
        Buffer: *mut u16,
        BufferLen: DWORD,
        ulFlags: DWORD,
    ) -> CONFIGRET;
    pub(crate) fn CM_Get_DevNode_Registry_PropertyW(
        dnDevInst: DEVINST,
        ulProperty: DWORD,
        pulRegDataType: *mut DWORD,
        Buffer: *mut c_void,
        pulLength: *mut DWORD,
        ulFlags: DWORD,
    ) -> CONFIGRET;
    pub(crate) fn CM_Open_DevNode_Key(
        dnDevNode: DEVINST,
        samDesired: DWORD,
        ulHardwareProfile: DWORD,
        ulFlags: DWORD,
        phkDevice: *mut HKEY,
        ulFlags2: DWORD,
    ) -> CONFIGRET;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub(crate) fn RegQueryValueExW(
        hKey: HKEY,
        lpValueName: *const u16,
        lpReserved: *mut DWORD,
        lpType: *mut DWORD,
        lpData: *mut u8,
        lpcbData: *mut DWORD,
    ) -> i32;
    pub(crate) fn RegCloseKey(hKey: HKEY) -> i32;
}

#[link(name = "winusb")]
unsafe extern "system" {
    pub(crate) fn WinUsb_Initialize(DeviceHandle: HANDLE, InterfaceHandle: *mut WINUSB_INTERFACE_HANDLE) -> BOOL;
    pub(crate) fn WinUsb_Free(InterfaceHandle: WINUSB_INTERFACE_HANDLE) -> BOOL;
    pub(crate) fn WinUsb_GetAssociatedInterface(
        InterfaceHandle: WINUSB_INTERFACE_HANDLE,
        AssociatedInterfaceIndex: u8,
        AssociatedInterfaceHandle: *mut WINUSB_INTERFACE_HANDLE,
    ) -> BOOL;
    pub(crate) fn WinUsb_ControlTransfer(
        InterfaceHandle: WINUSB_INTERFACE_HANDLE,
        SetupPacket: WINUSB_SETUP_PACKET,
        Buffer: *mut u8,
        BufferLength: u32,
        LengthTransferred: *mut u32,
        Overlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn WinUsb_ReadPipe(
        InterfaceHandle: WINUSB_INTERFACE_HANDLE,
        PipeID: u8,
        Buffer: *mut u8,
        BufferLength: u32,
        LengthTransferred: *mut u32,
        Overlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn WinUsb_WritePipe(
        InterfaceHandle: WINUSB_INTERFACE_HANDLE,
        PipeID: u8,
        Buffer: *mut u8,
        BufferLength: u32,
        LengthTransferred: *mut u32,
        Overlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn WinUsb_AbortPipe(InterfaceHandle: WINUSB_INTERFACE_HANDLE, PipeID: u8) -> BOOL;
    pub(crate) fn WinUsb_ResetPipe(InterfaceHandle: WINUSB_INTERFACE_HANDLE, PipeID: u8) -> BOOL;
    pub(crate) fn WinUsb_SetCurrentAlternateSetting(InterfaceHandle: WINUSB_INTERFACE_HANDLE, SettingNumber: u8) -> BOOL;
    pub(crate) fn WinUsb_GetCurrentAlternateSetting(InterfaceHandle: WINUSB_INTERFACE_HANDLE, SettingNumber: *mut u8) -> BOOL;
    pub(crate) fn WinUsb_SetPipePolicy(
        InterfaceHandle: WINUSB_INTERFACE_HANDLE,
        PipeID: u8,
        PolicyType: DWORD,
        ValueLength: DWORD,
        Value: *const c_void,
    ) -> BOOL;
}

/// Last Win32 error code.
pub(crate) fn last_error() -> DWORD {
    // SAFETY: trivially safe.
    unsafe { GetLastError() }
}

/// NUL-terminated UTF-16 for Win32 string arguments.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a NUL-terminated UTF-16 buffer.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// RAII wrapper closing a kernel handle.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; the kernel serialises access.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own the handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}
