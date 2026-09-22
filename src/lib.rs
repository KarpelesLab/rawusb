//! Dependency-free, cross-platform USB device access in the spirit of libusb.
//!
//! Work in progress: the platform backends are being filled in.

pub mod descriptors;
mod error;
pub mod types;

pub use error::{Error, ErrorKind, Result};

pub(crate) mod sys {
    /// Maps a raw OS error number to an [`ErrorKind`](crate::ErrorKind).
    pub(crate) fn errno_kind(_code: i32) -> crate::ErrorKind {
        crate::ErrorKind::Io
    }
}
