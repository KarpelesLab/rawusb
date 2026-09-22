//! Error type shared by every operation in the crate.

use std::fmt;

/// Broad classification of a failure, modelled on libusb's error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Generic I/O failure reported by the operating system.
    Io,
    /// An argument was out of range or inconsistent (bad endpoint, oversized
    /// buffer, interface not claimed, ...).
    InvalidParam,
    /// Permission denied (device node not accessible, or the OS driver refuses
    /// user-space access).
    Access,
    /// The device is gone (unplugged) or was never there.
    NoDevice,
    /// The requested entity (device, configuration, interface, string
    /// descriptor) does not exist.
    NotFound,
    /// The resource is busy: an interface is claimed elsewhere, a kernel
    /// driver is bound, or a transfer is already in flight.
    Busy,
    /// The operation did not complete before its timeout elapsed.
    Timeout,
    /// The device returned more data than fits the supplied buffer.
    Overflow,
    /// The endpoint is halted (STALL) or a control request was rejected.
    Pipe,
    /// The operation was interrupted (a transfer was cancelled while waiting
    /// for it, or a system call was interrupted by a signal).
    Interrupted,
    /// Memory allocation failed, or a kernel-side memory limit was reached.
    NoMem,
    /// The operation is not supported on this platform, driver, or device.
    NotSupported,
    /// A failure that fits none of the other categories.
    Other,
}

impl ErrorKind {
    /// Short human-readable description, in the style of `libusb_strerror`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Io => "input/output error",
            ErrorKind::InvalidParam => "invalid parameter",
            ErrorKind::Access => "access denied (insufficient permissions)",
            ErrorKind::NoDevice => "no such device (it may have been disconnected)",
            ErrorKind::NotFound => "entity not found",
            ErrorKind::Busy => "resource busy",
            ErrorKind::Timeout => "operation timed out",
            ErrorKind::Overflow => "overflow",
            ErrorKind::Pipe => "pipe error (endpoint halted or request rejected)",
            ErrorKind::Interrupted => "operation interrupted",
            ErrorKind::NoMem => "insufficient memory",
            ErrorKind::NotSupported => "operation not supported or unimplemented on this platform",
            ErrorKind::Other => "other error",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error type returned by every fallible operation in this crate.
///
/// An error always carries an [`ErrorKind`]. When it originates from the
/// operating system it also carries the raw OS error code (`errno`,
/// `GetLastError()` or an `IOReturn`), and it may carry a short message
/// naming the operation that failed.
#[derive(Clone)]
pub struct Error {
    kind: ErrorKind,
    code: Option<i32>,
    context: Option<std::borrow::Cow<'static, str>>,
}

impl Error {
    /// Creates an error of the given kind with no OS code and no message.
    pub const fn new(kind: ErrorKind) -> Self {
        Error {
            kind,
            code: None,
            context: None,
        }
    }

    /// Creates an error of the given kind carrying a raw OS error code.
    pub const fn from_code(kind: ErrorKind, code: i32) -> Self {
        Error {
            kind,
            code: Some(code),
            context: None,
        }
    }

    /// Creates an error of the given kind with a descriptive message.
    pub fn with_message(kind: ErrorKind, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Error {
            kind,
            code: None,
            context: Some(message.into()),
        }
    }

    /// Attaches a message describing the operation that failed.
    pub fn context(mut self, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        self.context = Some(message.into());
        self
    }

    /// Builds an error from the calling thread's last OS error.
    pub fn last_os_error() -> Self {
        Self::from(std::io::Error::last_os_error())
    }

    /// The error's category.
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The raw operating-system error code, if the error came from the OS.
    pub const fn os_code(&self) -> Option<i32> {
        self.code
    }

    /// The optional message naming the failed operation.
    pub fn message(&self) -> Option<&str> {
        self.context.as_deref()
    }

    /// `true` for [`ErrorKind::Timeout`].
    pub const fn is_timeout(&self) -> bool {
        matches!(self.kind, ErrorKind::Timeout)
    }

    /// `true` for [`ErrorKind::NoDevice`].
    pub const fn is_no_device(&self) -> bool {
        matches!(self.kind, ErrorKind::NoDevice)
    }

    /// `true` for [`ErrorKind::Pipe`] (STALL / rejected request).
    pub const fn is_stall(&self) -> bool {
        matches!(self.kind, ErrorKind::Pipe)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Error");
        d.field("kind", &self.kind);
        if let Some(code) = self.code {
            d.field("code", &code);
        }
        if let Some(ctx) = &self.context {
            d.field("context", ctx);
        }
        d.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ctx) = &self.context {
            write!(f, "{ctx}: ")?;
        }
        f.write_str(self.kind.as_str())?;
        if let Some(code) = self.code {
            let os = std::io::Error::from_raw_os_error(code);
            write!(f, " (os error {code}: {os})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::new(kind)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as Io;
        let kind = match e.kind() {
            Io::PermissionDenied => ErrorKind::Access,
            Io::NotFound => ErrorKind::NotFound,
            Io::TimedOut => ErrorKind::Timeout,
            Io::Interrupted => ErrorKind::Interrupted,
            Io::OutOfMemory => ErrorKind::NoMem,
            Io::Unsupported => ErrorKind::NotSupported,
            Io::InvalidInput => ErrorKind::InvalidParam,
            Io::ResourceBusy => ErrorKind::Busy,
            Io::BrokenPipe => ErrorKind::Pipe,
            _ => match e.raw_os_error() {
                Some(code) => crate::sys::errno_kind(code),
                None => ErrorKind::Io,
            },
        };
        Error {
            kind,
            code: e.raw_os_error(),
            context: None,
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        use std::io::ErrorKind as Io;
        let kind = match e.kind {
            ErrorKind::Io => Io::Other,
            ErrorKind::InvalidParam => Io::InvalidInput,
            ErrorKind::Access => Io::PermissionDenied,
            ErrorKind::NoDevice => Io::NotConnected,
            ErrorKind::NotFound => Io::NotFound,
            ErrorKind::Busy => Io::ResourceBusy,
            ErrorKind::Timeout => Io::TimedOut,
            ErrorKind::Overflow => Io::InvalidData,
            ErrorKind::Pipe => Io::BrokenPipe,
            ErrorKind::Interrupted => Io::Interrupted,
            ErrorKind::NoMem => Io::OutOfMemory,
            ErrorKind::NotSupported => Io::Unsupported,
            ErrorKind::Other => Io::Other,
        };
        std::io::Error::new(kind, e)
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
