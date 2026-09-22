//! `linux/usbdevice_fs.h`: the structures and ioctl numbers of the usbfs
//! character devices under `/dev/bus/usb`.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_ulong, c_void};
use std::mem::size_of;

// ----- _IOC encoding ---------------------------------------------------------

#[cfg(any(
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc",
    target_arch = "sparc64"
))]
mod ioc {
    pub(super) const SIZEBITS: u32 = 13;
    pub(super) const NONE: u32 = 1;
    pub(super) const READ: u32 = 2;
    pub(super) const WRITE: u32 = 4;
}

#[cfg(not(any(
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc",
    target_arch = "sparc64"
)))]
mod ioc {
    pub(super) const SIZEBITS: u32 = 14;
    pub(super) const NONE: u32 = 0;
    pub(super) const READ: u32 = 2;
    pub(super) const WRITE: u32 = 1;
}

const NRBITS: u32 = 8;
const TYPEBITS: u32 = 8;
const NRSHIFT: u32 = 0;
const TYPESHIFT: u32 = NRSHIFT + NRBITS;
const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
const DIRSHIFT: u32 = SIZESHIFT + ioc::SIZEBITS;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> c_ulong {
    ((dir << DIRSHIFT) | ((ty as u32) << TYPESHIFT) | ((nr as u32) << NRSHIFT) | ((size as u32) << SIZESHIFT)) as c_ulong
}

const fn io(ty: u8, nr: u8) -> c_ulong {
    ioc(ioc::NONE, ty, nr, 0)
}
const fn ior<T>(ty: u8, nr: u8) -> c_ulong {
    ioc(ioc::READ, ty, nr, size_of::<T>())
}
const fn iow<T>(ty: u8, nr: u8) -> c_ulong {
    ioc(ioc::WRITE, ty, nr, size_of::<T>())
}
const fn iowr<T>(ty: u8, nr: u8) -> c_ulong {
    ioc(ioc::READ | ioc::WRITE, ty, nr, size_of::<T>())
}

// ----- structures ------------------------------------------------------------

#[repr(C)]
pub(crate) struct usbdevfs_ctrltransfer {
    pub(crate) bRequestType: u8,
    pub(crate) bRequest: u8,
    pub(crate) wValue: u16,
    pub(crate) wIndex: u16,
    pub(crate) wLength: u16,
    pub(crate) timeout: u32,
    pub(crate) data: *mut c_void,
}

#[repr(C)]
pub(crate) struct usbdevfs_bulktransfer {
    pub(crate) ep: u32,
    pub(crate) len: u32,
    pub(crate) timeout: u32,
    pub(crate) data: *mut c_void,
}

#[repr(C)]
pub(crate) struct usbdevfs_setinterface {
    pub(crate) interface: u32,
    pub(crate) altsetting: u32,
}

#[repr(C)]
pub(crate) struct usbdevfs_disconnectsignal {
    pub(crate) signr: u32,
    pub(crate) context: *mut c_void,
}

pub(crate) const USBDEVFS_MAXDRIVERNAME: usize = 255;

#[repr(C)]
pub(crate) struct usbdevfs_getdriver {
    pub(crate) interface: u32,
    pub(crate) driver: [u8; USBDEVFS_MAXDRIVERNAME + 1],
}

#[repr(C)]
pub(crate) struct usbdevfs_connectinfo {
    pub(crate) devnum: u32,
    pub(crate) slow: u8,
}

pub(crate) const USBDEVFS_URB_SHORT_NOT_OK: u32 = 0x01;
pub(crate) const USBDEVFS_URB_ISO_ASAP: u32 = 0x02;
pub(crate) const USBDEVFS_URB_BULK_CONTINUATION: u32 = 0x04;
pub(crate) const USBDEVFS_URB_ZERO_PACKET: u32 = 0x40;
pub(crate) const USBDEVFS_URB_NO_INTERRUPT: u32 = 0x80;

pub(crate) const USBDEVFS_URB_TYPE_ISO: u8 = 0;
pub(crate) const USBDEVFS_URB_TYPE_INTERRUPT: u8 = 1;
pub(crate) const USBDEVFS_URB_TYPE_CONTROL: u8 = 2;
pub(crate) const USBDEVFS_URB_TYPE_BULK: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct usbdevfs_iso_packet_desc {
    pub(crate) length: u32,
    pub(crate) actual_length: u32,
    /// Declared `unsigned int` by the kernel but carries a negated errno.
    pub(crate) status: u32,
}

/// `struct usbdevfs_urb` minus its trailing flexible `iso_frame_desc[]`
/// array; the packet descriptors follow immediately in memory.
#[repr(C)]
pub(crate) struct usbdevfs_urb {
    pub(crate) type_: u8,
    pub(crate) endpoint: u8,
    pub(crate) status: i32,
    pub(crate) flags: u32,
    pub(crate) buffer: *mut c_void,
    pub(crate) buffer_length: i32,
    pub(crate) actual_length: i32,
    pub(crate) start_frame: i32,
    /// `number_of_packets` for isochronous URBs, `stream_id` for bulk streams.
    pub(crate) number_of_packets: i32,
    pub(crate) error_count: i32,
    pub(crate) signr: u32,
    pub(crate) usercontext: *mut c_void,
}

#[repr(C)]
pub(crate) struct usbdevfs_ioctl {
    pub(crate) ifno: i32,
    pub(crate) ioctl_code: i32,
    pub(crate) data: *mut c_void,
}

#[repr(C)]
pub(crate) struct usbdevfs_hub_portinfo {
    pub(crate) nports: u8,
    pub(crate) port: [u8; 127],
}

pub(crate) const USBDEVFS_CAP_ZERO_PACKET: u32 = 0x01;
pub(crate) const USBDEVFS_CAP_BULK_CONTINUATION: u32 = 0x02;
pub(crate) const USBDEVFS_CAP_NO_PACKET_SIZE_LIM: u32 = 0x04;
pub(crate) const USBDEVFS_CAP_BULK_SCATTER_GATHER: u32 = 0x08;
pub(crate) const USBDEVFS_CAP_REAP_AFTER_DISCONNECT: u32 = 0x10;
pub(crate) const USBDEVFS_CAP_MMAP: u32 = 0x20;
pub(crate) const USBDEVFS_CAP_DROP_PRIVILEGES: u32 = 0x40;
pub(crate) const USBDEVFS_CAP_CONNINFO_EX: u32 = 0x80;
pub(crate) const USBDEVFS_CAP_SUSPEND: u32 = 0x100;

pub(crate) const USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER: u32 = 0x01;
pub(crate) const USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER: u32 = 0x02;

#[repr(C)]
pub(crate) struct usbdevfs_disconnect_claim {
    pub(crate) interface: u32,
    pub(crate) flags: u32,
    pub(crate) driver: [u8; USBDEVFS_MAXDRIVERNAME + 1],
}

#[repr(C)]
pub(crate) struct usbdevfs_streams {
    pub(crate) num_streams: u32,
    pub(crate) num_eps: u32,
    // followed by `u8 eps[num_eps]`
}

// ----- ioctl numbers ---------------------------------------------------------
// usbfs uses _IOR/_IOW with the direction seen from the kernel's point of view
// reversed relative to the rest of the kernel; the numbers below are what the
// header says, not what the names suggest.

const U: u8 = b'U';

pub(crate) const USBDEVFS_CONTROL: c_ulong = iowr::<usbdevfs_ctrltransfer>(U, 0);
pub(crate) const USBDEVFS_BULK: c_ulong = iowr::<usbdevfs_bulktransfer>(U, 2);
pub(crate) const USBDEVFS_RESETEP: c_ulong = ior::<u32>(U, 3);
pub(crate) const USBDEVFS_SETINTERFACE: c_ulong = ior::<usbdevfs_setinterface>(U, 4);
pub(crate) const USBDEVFS_SETCONFIGURATION: c_ulong = ior::<u32>(U, 5);
pub(crate) const USBDEVFS_GETDRIVER: c_ulong = iow::<usbdevfs_getdriver>(U, 8);
pub(crate) const USBDEVFS_SUBMITURB: c_ulong = ior::<usbdevfs_urb>(U, 10);
pub(crate) const USBDEVFS_DISCARDURB: c_ulong = io(U, 11);
pub(crate) const USBDEVFS_REAPURB: c_ulong = iow::<*mut c_void>(U, 12);
pub(crate) const USBDEVFS_REAPURBNDELAY: c_ulong = iow::<*mut c_void>(U, 13);
pub(crate) const USBDEVFS_DISCSIGNAL: c_ulong = ior::<usbdevfs_disconnectsignal>(U, 14);
pub(crate) const USBDEVFS_CLAIMINTERFACE: c_ulong = ior::<u32>(U, 15);
pub(crate) const USBDEVFS_RELEASEINTERFACE: c_ulong = ior::<u32>(U, 16);
pub(crate) const USBDEVFS_CONNECTINFO: c_ulong = iow::<usbdevfs_connectinfo>(U, 17);
pub(crate) const USBDEVFS_IOCTL: c_ulong = iowr::<usbdevfs_ioctl>(U, 18);
pub(crate) const USBDEVFS_HUB_PORTINFO: c_ulong = ior::<usbdevfs_hub_portinfo>(U, 19);
pub(crate) const USBDEVFS_RESET: c_ulong = io(U, 20);
pub(crate) const USBDEVFS_CLEAR_HALT: c_ulong = ior::<u32>(U, 21);
pub(crate) const USBDEVFS_DISCONNECT: c_ulong = io(U, 22);
pub(crate) const USBDEVFS_CONNECT: c_ulong = io(U, 23);
pub(crate) const USBDEVFS_CLAIM_PORT: c_ulong = ior::<u32>(U, 24);
pub(crate) const USBDEVFS_RELEASE_PORT: c_ulong = ior::<u32>(U, 25);
pub(crate) const USBDEVFS_GET_CAPABILITIES: c_ulong = ior::<u32>(U, 26);
pub(crate) const USBDEVFS_DISCONNECT_CLAIM: c_ulong = ior::<usbdevfs_disconnect_claim>(U, 27);
pub(crate) const USBDEVFS_ALLOC_STREAMS: c_ulong = ior::<usbdevfs_streams>(U, 28);
pub(crate) const USBDEVFS_FREE_STREAMS: c_ulong = ior::<usbdevfs_streams>(U, 29);
pub(crate) const USBDEVFS_DROP_PRIVILEGES: c_ulong = iow::<u32>(U, 30);
pub(crate) const USBDEVFS_GET_SPEED: c_ulong = io(U, 31);

/// Largest bulk URB accepted by kernels without scatter-gather support.
pub(crate) const MAX_BULK_BUFFER_LENGTH: usize = 16384;
/// Largest number of packets in one isochronous URB.
pub(crate) const MAX_ISO_PACKETS_PER_URB: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
    fn ioctl_numbers_match_the_header() {
        // Values taken from a 64-bit x86 build of <linux/usbdevice_fs.h>.
        assert_eq!(size_of::<usbdevfs_urb>(), 56);
        assert_eq!(USBDEVFS_CONTROL, 0xC0185500);
        assert_eq!(USBDEVFS_BULK, 0xC0185502);
        assert_eq!(USBDEVFS_SETINTERFACE, 0x80085504);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x80045505);
        assert_eq!(USBDEVFS_GETDRIVER, 0x41045508);
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038550A);
        assert_eq!(USBDEVFS_DISCARDURB, 0x0000550B);
        assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008550D);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004550F);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x80045510);
        assert_eq!(USBDEVFS_IOCTL, 0xC0105512);
        assert_eq!(USBDEVFS_RESET, 0x00005514);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x80045515);
        assert_eq!(USBDEVFS_GET_CAPABILITIES, 0x8004551A);
        assert_eq!(USBDEVFS_DISCONNECT_CLAIM, 0x8108551B);
    }

    #[test]
    fn iso_descs_follow_the_urb() {
        // The flexible array member starts right after the fixed part.
        assert_eq!(size_of::<usbdevfs_urb>() % 4, 0);
        assert_eq!(size_of::<usbdevfs_iso_packet_desc>(), 12);
    }
}
