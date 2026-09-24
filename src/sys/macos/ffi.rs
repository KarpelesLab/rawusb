//! IOKit and CoreFoundation declarations for the macOS backend.
//!
//! The USB user-client interfaces are COM-style objects: a pointer to a
//! pointer to a table of function pointers. The tables below reproduce the
//! layout of `IOUSBLib.h` up to the `182` revisions, which added the
//! timeout-taking (`...TO`) variants this backend prefers.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::upper_case_acronyms
)]

use std::ffi::{c_char, c_void};

pub(crate) type IOReturn = i32;
pub(crate) type kern_return_t = i32;
pub(crate) type mach_port_t = u32;
pub(crate) type io_object_t = u32;
pub(crate) type io_iterator_t = io_object_t;
pub(crate) type io_service_t = io_object_t;
pub(crate) type io_registry_entry_t = io_object_t;
pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type CFDictionaryRef = *const c_void;
pub(crate) type CFMutableDictionaryRef = *mut c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFUUIDRef = *const c_void;
pub(crate) type CFRunLoopRef = *mut c_void;
pub(crate) type CFRunLoopSourceRef = *mut c_void;
pub(crate) type CFRunLoopTimerRef = *mut c_void;
pub(crate) type CFAbsoluteTime = f64;
pub(crate) type CFTimeInterval = f64;
pub(crate) type CFIndex = isize;
pub(crate) type CFTypeID = usize;
pub(crate) type CFStringEncoding = u32;
pub(crate) type Boolean = u8;
pub(crate) type HRESULT = i32;
pub(crate) type IOOptionBits = u32;

pub(crate) const kCFStringEncodingUTF8: CFStringEncoding = 0x0800_0100;

pub(crate) const kIOReturnSuccess: IOReturn = 0;
pub(crate) const kIOReturnNoMemory: IOReturn = 0xe00002bdu32 as i32;
pub(crate) const kIOReturnNoDevice: IOReturn = 0xe00002c0u32 as i32;
pub(crate) const kIOReturnNotPrivileged: IOReturn = 0xe00002c1u32 as i32;
pub(crate) const kIOReturnBadArgument: IOReturn = 0xe00002c2u32 as i32;
pub(crate) const kIOReturnExclusiveAccess: IOReturn = 0xe00002c5u32 as i32;
pub(crate) const kIOReturnUnsupported: IOReturn = 0xe00002c7u32 as i32;
pub(crate) const kIOReturnNotOpen: IOReturn = 0xe00002cdu32 as i32;
pub(crate) const kIOReturnNotReady: IOReturn = 0xe00002ceu32 as i32;
pub(crate) const kIOReturnBusy: IOReturn = 0xe00002d5u32 as i32;
pub(crate) const kIOReturnTimeout: IOReturn = 0xe00002d6u32 as i32;
pub(crate) const kIOReturnNotAttached: IOReturn = 0xe00002d9u32 as i32;
pub(crate) const kIOReturnNoResources: IOReturn = 0xe00002beu32 as i32;
pub(crate) const kIOReturnOverrun: IOReturn = 0xe00002e8u32 as i32;
pub(crate) const kIOReturnUnderrun: IOReturn = 0xe00002e9u32 as i32;
pub(crate) const kIOReturnAborted: IOReturn = 0xe00002ebu32 as i32;
pub(crate) const kIOReturnNotResponding: IOReturn = 0xe00002edu32 as i32;
pub(crate) const kIOReturnNotFound: IOReturn = 0xe00002f0u32 as i32;
pub(crate) const kIOUSBPipeStalled: IOReturn = 0xe000404fu32 as i32;
pub(crate) const kIOUSBTransactionTimeout: IOReturn = 0xe0004051u32 as i32;
pub(crate) const kIOUSBTransactionReturned: IOReturn = 0xe0004059u32 as i32;

pub(crate) const kIOMasterPortDefault: mach_port_t = 0;
pub(crate) const kIOUSBFindInterfaceDontCare: u16 = 0xFFFF;

pub(crate) const kUSBIn: u8 = 1;
pub(crate) const kUSBOut: u8 = 0;

pub(crate) const kCFRunLoopRunFinished: i32 = 1;
pub(crate) const kCFRunLoopRunStopped: i32 = 2;
pub(crate) const kCFRunLoopRunTimedOut: i32 = 3;
pub(crate) const kCFRunLoopRunHandledSource: i32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CFUUIDBytes {
    pub(crate) bytes: [u8; 16],
}

impl CFUUIDBytes {
    pub(crate) const fn new(bytes: [u8; 16]) -> Self {
        CFUUIDBytes { bytes }
    }
}

// UUIDs from IOCFPlugIn.h and IOUSBLib.h.
pub(crate) const kIOCFPlugInInterfaceID: CFUUIDBytes = CFUUIDBytes::new([
    0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F,
]);
pub(crate) const kIOUSBDeviceUserClientTypeID: CFUUIDBytes = CFUUIDBytes::new([
    0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xD4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
]);
pub(crate) const kIOUSBInterfaceUserClientTypeID: CFUUIDBytes = CFUUIDBytes::new([
    0x2d, 0x97, 0x86, 0xc6, 0x9e, 0xf3, 0x11, 0xD4, 0xad, 0x51, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
]);
pub(crate) const kIOUSBDeviceInterfaceID: CFUUIDBytes = CFUUIDBytes::new([
    0x5c, 0x81, 0x87, 0xd0, 0x9e, 0xf3, 0x11, 0xD4, 0x8b, 0x45, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
]);
pub(crate) const kIOUSBDeviceInterfaceID182: CFUUIDBytes = CFUUIDBytes::new([
    0x15, 0x2f, 0xc4, 0x96, 0x48, 0x91, 0x11, 0xD5, 0x9d, 0x52, 0x00, 0x0a, 0x27, 0x80, 0x1e, 0x86,
]);
pub(crate) const kIOUSBInterfaceInterfaceID: CFUUIDBytes = CFUUIDBytes::new([
    0x73, 0xc9, 0x7a, 0xe8, 0x9e, 0xf3, 0x11, 0xD4, 0xb1, 0xd0, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
]);
pub(crate) const kIOUSBInterfaceInterfaceID182: CFUUIDBytes = CFUUIDBytes::new([
    0x49, 0x23, 0xac, 0x4c, 0x48, 0x96, 0x11, 0xD5, 0x92, 0x08, 0x00, 0x0a, 0x27, 0x80, 0x1e, 0x86,
]);

pub(crate) type IOAsyncCallback1 = unsafe extern "C" fn(refcon: *mut c_void, result: IOReturn, arg0: *mut c_void);

#[repr(C)]
pub(crate) struct IOUSBDevRequest {
    pub(crate) bmRequestType: u8,
    pub(crate) bRequest: u8,
    pub(crate) wValue: u16,
    pub(crate) wIndex: u16,
    pub(crate) wLength: u16,
    pub(crate) pData: *mut c_void,
    pub(crate) wLenDone: u32,
}

#[repr(C)]
pub(crate) struct IOUSBDevRequestTO {
    pub(crate) bmRequestType: u8,
    pub(crate) bRequest: u8,
    pub(crate) wValue: u16,
    pub(crate) wIndex: u16,
    pub(crate) wLength: u16,
    pub(crate) pData: *mut c_void,
    pub(crate) wLenDone: u32,
    pub(crate) noDataTimeout: u32,
    pub(crate) completionTimeout: u32,
}

#[repr(C)]
pub(crate) struct IOUSBFindInterfaceRequest {
    pub(crate) bInterfaceClass: u16,
    pub(crate) bInterfaceSubClass: u16,
    pub(crate) bInterfaceProtocol: u16,
    pub(crate) bAlternateSetting: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct IOUSBIsocFrame {
    pub(crate) frStatus: IOReturn,
    pub(crate) frReqCount: u16,
    pub(crate) frActCount: u16,
}

#[repr(C)]
pub(crate) struct AbsoluteTime {
    pub(crate) lo: u32,
    pub(crate) hi: u32,
}

/// `this` pointer of a COM object: points at the vtable pointer.
pub(crate) type This = *mut c_void;

#[repr(C)]
pub(crate) struct IOCFPlugInInterface {
    pub(crate) _reserved: *mut c_void,
    pub(crate) QueryInterface: unsafe extern "C" fn(This, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub(crate) AddRef: unsafe extern "C" fn(This) -> u32,
    pub(crate) Release: unsafe extern "C" fn(This) -> u32,
    pub(crate) version: u16,
    pub(crate) revision: u16,
    pub(crate) Probe: unsafe extern "C" fn(This, CFDictionaryRef, io_service_t, *mut i32) -> IOReturn,
    pub(crate) Start: unsafe extern "C" fn(This, CFDictionaryRef, io_service_t) -> IOReturn,
    pub(crate) Stop: unsafe extern "C" fn(This) -> IOReturn,
}

#[repr(C)]
pub(crate) struct IOUSBDeviceInterface182 {
    pub(crate) _reserved: *mut c_void,
    pub(crate) QueryInterface: unsafe extern "C" fn(This, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub(crate) AddRef: unsafe extern "C" fn(This) -> u32,
    pub(crate) Release: unsafe extern "C" fn(This) -> u32,
    pub(crate) CreateDeviceAsyncEventSource: unsafe extern "C" fn(This, *mut CFRunLoopSourceRef) -> IOReturn,
    pub(crate) GetDeviceAsyncEventSource: unsafe extern "C" fn(This) -> CFRunLoopSourceRef,
    pub(crate) CreateDeviceAsyncPort: unsafe extern "C" fn(This, *mut mach_port_t) -> IOReturn,
    pub(crate) GetDeviceAsyncPort: unsafe extern "C" fn(This) -> mach_port_t,
    pub(crate) USBDeviceOpen: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) USBDeviceClose: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) GetDeviceClass: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetDeviceSubClass: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetDeviceProtocol: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetDeviceVendor: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceProduct: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceReleaseNumber: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceAddress: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceBusPowerAvailable: unsafe extern "C" fn(This, *mut u32) -> IOReturn,
    pub(crate) GetDeviceSpeed: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetNumberOfConfigurations: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetLocationID: unsafe extern "C" fn(This, *mut u32) -> IOReturn,
    pub(crate) GetConfigurationDescriptorPtr: unsafe extern "C" fn(This, u8, *mut *mut u8) -> IOReturn,
    pub(crate) GetConfiguration: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) SetConfiguration: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) GetBusFrameNumber: unsafe extern "C" fn(This, *mut u64, *mut AbsoluteTime) -> IOReturn,
    pub(crate) ResetDevice: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) DeviceRequest: unsafe extern "C" fn(This, *mut IOUSBDevRequest) -> IOReturn,
    pub(crate) DeviceRequestAsync: unsafe extern "C" fn(This, *mut IOUSBDevRequest, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) CreateInterfaceIterator: unsafe extern "C" fn(This, *mut IOUSBFindInterfaceRequest, *mut io_iterator_t) -> IOReturn,
    // --- revision 182 ---
    pub(crate) USBDeviceOpenSeize: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) DeviceRequestTO: unsafe extern "C" fn(This, *mut IOUSBDevRequestTO) -> IOReturn,
    pub(crate) DeviceRequestAsyncTO: unsafe extern "C" fn(This, *mut IOUSBDevRequestTO, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) USBDeviceSuspend: unsafe extern "C" fn(This, Boolean) -> IOReturn,
    pub(crate) USBDeviceAbortPipeZero: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) USBGetManufacturerStringIndex: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) USBGetProductStringIndex: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) USBGetSerialNumberStringIndex: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
}

#[repr(C)]
pub(crate) struct IOUSBInterfaceInterface182 {
    pub(crate) _reserved: *mut c_void,
    pub(crate) QueryInterface: unsafe extern "C" fn(This, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub(crate) AddRef: unsafe extern "C" fn(This) -> u32,
    pub(crate) Release: unsafe extern "C" fn(This) -> u32,
    pub(crate) CreateInterfaceAsyncEventSource: unsafe extern "C" fn(This, *mut CFRunLoopSourceRef) -> IOReturn,
    pub(crate) GetInterfaceAsyncEventSource: unsafe extern "C" fn(This) -> CFRunLoopSourceRef,
    pub(crate) CreateInterfaceAsyncPort: unsafe extern "C" fn(This, *mut mach_port_t) -> IOReturn,
    pub(crate) GetInterfaceAsyncPort: unsafe extern "C" fn(This) -> mach_port_t,
    pub(crate) USBInterfaceOpen: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) USBInterfaceClose: unsafe extern "C" fn(This) -> IOReturn,
    pub(crate) GetInterfaceClass: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetInterfaceSubClass: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetInterfaceProtocol: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetDeviceVendor: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceProduct: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetDeviceReleaseNumber: unsafe extern "C" fn(This, *mut u16) -> IOReturn,
    pub(crate) GetConfigurationValue: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetInterfaceNumber: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetAlternateSetting: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetNumEndpoints: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
    pub(crate) GetLocationID: unsafe extern "C" fn(This, *mut u32) -> IOReturn,
    pub(crate) GetDevice: unsafe extern "C" fn(This, *mut io_service_t) -> IOReturn,
    pub(crate) SetAlternateInterface: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) GetBusFrameNumber: unsafe extern "C" fn(This, *mut u64, *mut AbsoluteTime) -> IOReturn,
    pub(crate) ControlRequest: unsafe extern "C" fn(This, u8, *mut IOUSBDevRequest) -> IOReturn,
    pub(crate) ControlRequestAsync: unsafe extern "C" fn(This, u8, *mut IOUSBDevRequest, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) GetPipeProperties: unsafe extern "C" fn(This, u8, *mut u8, *mut u8, *mut u8, *mut u16, *mut u8) -> IOReturn,
    pub(crate) GetPipeStatus: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) AbortPipe: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) ResetPipe: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) ClearPipeStall: unsafe extern "C" fn(This, u8) -> IOReturn,
    pub(crate) ReadPipe: unsafe extern "C" fn(This, u8, *mut c_void, *mut u32) -> IOReturn,
    pub(crate) WritePipe: unsafe extern "C" fn(This, u8, *mut c_void, u32) -> IOReturn,
    pub(crate) ReadPipeAsync: unsafe extern "C" fn(This, u8, *mut c_void, u32, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) WritePipeAsync: unsafe extern "C" fn(This, u8, *mut c_void, u32, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) ReadIsochPipeAsync:
        unsafe extern "C" fn(This, u8, *mut c_void, u64, u32, *mut IOUSBIsocFrame, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) WriteIsochPipeAsync:
        unsafe extern "C" fn(This, u8, *mut c_void, u64, u32, *mut IOUSBIsocFrame, IOAsyncCallback1, *mut c_void) -> IOReturn,
    // --- revision 182 ---
    pub(crate) ControlRequestTO: unsafe extern "C" fn(This, u8, *mut IOUSBDevRequestTO) -> IOReturn,
    pub(crate) ControlRequestAsyncTO: unsafe extern "C" fn(This, u8, *mut IOUSBDevRequestTO, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) ReadPipeTO: unsafe extern "C" fn(This, u8, *mut c_void, *mut u32, u32, u32) -> IOReturn,
    pub(crate) WritePipeTO: unsafe extern "C" fn(This, u8, *mut c_void, u32, u32, u32) -> IOReturn,
    pub(crate) ReadPipeAsyncTO: unsafe extern "C" fn(This, u8, *mut c_void, u32, u32, u32, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) WritePipeAsyncTO: unsafe extern "C" fn(This, u8, *mut c_void, u32, u32, u32, IOAsyncCallback1, *mut c_void) -> IOReturn,
    pub(crate) USBInterfaceGetStringIndex: unsafe extern "C" fn(This, *mut u8) -> IOReturn,
}

/// Opaque `IONotificationPortRef`.
pub(crate) type IONotificationPortRef = *mut c_void;
/// `void (*)(void *refcon, io_iterator_t iterator)`
pub(crate) type IOServiceMatchingCallback = unsafe extern "C" fn(refcon: *mut c_void, iterator: io_iterator_t);

/// Notification type strings from `IOKitKeys.h`. They are C macros, not
/// exported symbols, so they are spelled out here.
pub(crate) const kIOMatchedNotification: &std::ffi::CStr = c"IOServiceMatched";
/// See [`kIOMatchedNotification`].
pub(crate) const kIOTerminatedNotification: &std::ffi::CStr = c"IOServiceTerminate";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    pub(crate) fn IOServiceGetMatchingServices(
        mainPort: mach_port_t,
        matching: CFDictionaryRef,
        existing: *mut io_iterator_t,
    ) -> kern_return_t;
    pub(crate) fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    pub(crate) fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    pub(crate) fn IORegistryEntryGetRegistryEntryID(entry: io_registry_entry_t, entryID: *mut u64) -> kern_return_t;
    /// Returns a new reference, or null when the property is absent.
    pub(crate) fn IORegistryEntryCreateCFProperty(
        entry: io_registry_entry_t,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: IOOptionBits,
    ) -> CFTypeRef;
    pub(crate) fn IORegistryEntryGetChildEntry(
        entry: io_registry_entry_t,
        plane: *const c_char,
        child: *mut io_registry_entry_t,
    ) -> kern_return_t;
    pub(crate) fn IONotificationPortCreate(mainPort: mach_port_t) -> IONotificationPortRef;
    pub(crate) fn IONotificationPortDestroy(notify: IONotificationPortRef);
    pub(crate) fn IONotificationPortGetRunLoopSource(notify: IONotificationPortRef) -> CFRunLoopSourceRef;
    /// Consumes one reference on `matching`.
    pub(crate) fn IOServiceAddMatchingNotification(
        notifyPort: IONotificationPortRef,
        notificationType: *const c_char,
        matching: CFDictionaryRef,
        callback: IOServiceMatchingCallback,
        refCon: *mut c_void,
        notification: *mut io_iterator_t,
    ) -> kern_return_t;
    pub(crate) fn IOCreatePlugInInterfaceForService(
        service: io_service_t,
        pluginType: CFUUIDRef,
        interfaceType: CFUUIDRef,
        theInterface: *mut *mut *mut IOCFPlugInInterface,
        theScore: *mut i32,
    ) -> kern_return_t;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCFRunLoopDefaultMode: CFStringRef;
    pub(crate) fn CFRelease(cf: CFTypeRef);
    pub(crate) fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub(crate) fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    pub(crate) fn CFStringGetTypeID() -> CFTypeID;
    pub(crate) fn CFStringCreateWithCString(alloc: CFAllocatorRef, cStr: *const c_char, encoding: CFStringEncoding) -> CFStringRef;
    pub(crate) fn CFStringGetLength(theString: CFStringRef) -> CFIndex;
    pub(crate) fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
    pub(crate) fn CFStringGetCString(
        theString: CFStringRef,
        buffer: *mut c_char,
        bufferSize: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    pub(crate) fn CFUUIDGetConstantUUIDWithBytes(
        alloc: CFAllocatorRef,
        b0: u8,
        b1: u8,
        b2: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        b6: u8,
        b7: u8,
        b8: u8,
        b9: u8,
        b10: u8,
        b11: u8,
        b12: u8,
        b13: u8,
        b14: u8,
        b15: u8,
    ) -> CFUUIDRef;
    pub(crate) fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub(crate) fn CFRunLoopRunInMode(mode: CFStringRef, seconds: CFTimeInterval, returnAfterSourceHandled: Boolean) -> i32;
    pub(crate) fn CFRunLoopStop(rl: CFRunLoopRef);
    pub(crate) fn CFRunLoopWakeUp(rl: CFRunLoopRef);
    pub(crate) fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub(crate) fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub(crate) fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
    pub(crate) fn CFRunLoopTimerCreate(
        allocator: CFAllocatorRef,
        fireDate: CFAbsoluteTime,
        interval: CFTimeInterval,
        flags: u32,
        order: CFIndex,
        callout: Option<unsafe extern "C" fn(CFRunLoopTimerRef, *mut c_void)>,
        context: *mut c_void,
    ) -> CFRunLoopTimerRef;
    pub(crate) fn CFAbsoluteTimeGetCurrent() -> CFAbsoluteTime;
}

/// Converts a constant UUID into the `CFUUIDRef` IOKit wants.
pub(crate) fn uuid_ref(u: &CFUUIDBytes) -> CFUUIDRef {
    let b = u.bytes;
    // SAFETY: constant UUIDs are interned by CoreFoundation and never freed.
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            std::ptr::null(),
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15],
        )
    }
}
