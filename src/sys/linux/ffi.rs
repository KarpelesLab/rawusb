//! The handful of libc symbols the Linux backend needs. `std` already links
//! libc, so declaring them here costs nothing and avoids a dependency.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_int, c_short, c_ulong, c_void};

#[repr(C)]
pub(crate) struct pollfd {
    pub(crate) fd: c_int,
    pub(crate) events: c_short,
    pub(crate) revents: c_short,
}

pub(crate) const POLLIN: c_short = 0x001;
pub(crate) const POLLOUT: c_short = 0x004;
pub(crate) const POLLERR: c_short = 0x008;
pub(crate) const POLLHUP: c_short = 0x010;
pub(crate) const POLLNVAL: c_short = 0x020;

pub(crate) const F_GETFL: c_int = 3;
pub(crate) const F_SETFL: c_int = 4;
pub(crate) const F_SETFD: c_int = 2;
pub(crate) const FD_CLOEXEC: c_int = 1;

#[cfg(any(target_arch = "mips", target_arch = "mips64"))]
pub(crate) const O_NONBLOCK: c_int = 0o200;
#[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
pub(crate) const O_NONBLOCK: c_int = 0x4000;
#[cfg(not(any(target_arch = "mips", target_arch = "mips64", target_arch = "sparc", target_arch = "sparc64")))]
pub(crate) const O_NONBLOCK: c_int = 0o4000;

// errno values (asm-generic). mips and sparc renumber some of these; those
// targets are not verified.
pub(crate) const EPERM: i32 = 1;
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EINTR: i32 = 4;
pub(crate) const EAGAIN: i32 = 11;
pub(crate) const ENOMEM: i32 = 12;
pub(crate) const EACCES: i32 = 13;
pub(crate) const EBUSY: i32 = 16;
pub(crate) const EXDEV: i32 = 18;
pub(crate) const ENODEV: i32 = 19;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ENOTTY: i32 = 25;
pub(crate) const EPIPE: i32 = 32;
pub(crate) const ENOSYS: i32 = 38;
pub(crate) const ENODATA: i32 = 61;
pub(crate) const ETIME: i32 = 62;
pub(crate) const ENOSR: i32 = 63;
pub(crate) const ECOMM: i32 = 70;
pub(crate) const EPROTO: i32 = 71;
pub(crate) const EOVERFLOW: i32 = 75;
pub(crate) const EILSEQ: i32 = 84;
pub(crate) const EOPNOTSUPP: i32 = 95;
pub(crate) const ECONNRESET: i32 = 104;
pub(crate) const ESHUTDOWN: i32 = 108;
pub(crate) const ETIMEDOUT: i32 = 110;
pub(crate) const EREMOTEIO: i32 = 121;

// Netlink, for the hotplug backend. `SOCK_CLOEXEC`/`SOCK_NONBLOCK` are
// deliberately not used: their values follow `O_CLOEXEC`/`O_NONBLOCK`, which
// differ on sparc and alpha. `fcntl` is spelled the same everywhere.
pub(crate) const AF_NETLINK: c_int = 16;
pub(crate) const SOCK_RAW: c_int = 3;
pub(crate) const NETLINK_KOBJECT_UEVENT: c_int = 15;
/// Multicast group 1 carries the kernel's own uevents. Unlike group 2 (what
/// udev re-broadcasts), it is readable without privileges.
pub(crate) const UEVENT_GROUP_KERNEL: u32 = 1;

/// `struct sockaddr_nl`.
#[repr(C)]
#[derive(Default)]
pub(crate) struct sockaddr_nl {
    pub(crate) nl_family: u16,
    pub(crate) nl_pad: u16,
    pub(crate) nl_pid: u32,
    pub(crate) nl_groups: u32,
}

unsafe extern "C" {
    pub(crate) fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    pub(crate) fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    pub(crate) fn recvfrom(fd: c_int, buf: *mut c_void, len: usize, flags: c_int, addr: *mut c_void, addr_len: *mut u32) -> isize;
}

unsafe extern "C" {
    /// glibc declares the request as `unsigned long`, musl and bionic as
    /// `int`. Both read the same low 32 bits from the argument register, so
    /// one declaration serves all three.
    pub(crate) fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub(crate) fn poll(fds: *mut pollfd, nfds: c_ulong, timeout: c_int) -> c_int;
    pub(crate) fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

/// Last `errno`.
pub(crate) fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Marks a descriptor close-on-exec.
pub(crate) fn set_cloexec(fd: c_int) -> std::io::Result<()> {
    // SAFETY: plain fcntl call on a descriptor we own.
    if unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Puts a descriptor into non-blocking mode.
pub(crate) fn set_nonblocking(fd: c_int) -> std::io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = fcntl(fd, F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
