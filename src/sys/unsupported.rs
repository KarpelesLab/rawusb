//! Fallback backend for platforms without USB support: everything fails with
//! [`ErrorKind::NotSupported`].

use super::DeviceInfo;
use crate::transfer::{Inner, State};
use crate::{Error, ErrorKind, Result};
use std::sync::Arc;

pub(crate) struct Location;

pub(crate) struct Context;

impl Context {
    pub(crate) fn new() -> Result<Arc<Self>> {
        Err(Error::with_message(
            ErrorKind::NotSupported,
            "USB access is not supported on this platform",
        ))
    }

    pub(crate) fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        Err(Error::new(ErrorKind::NotSupported))
    }

    pub(crate) fn open(&self, _dev: &Arc<DeviceInfo>) -> Result<Arc<Handle>> {
        Err(Error::new(ErrorKind::NotSupported))
    }
}

pub(crate) struct Handle;

impl Handle {
    pub(crate) fn active_configuration(&self) -> Result<Option<u8>> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn set_configuration(&self, _v: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn claim_interface(&self, _i: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn release_interface(&self, _i: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn set_alt_setting(&self, _i: u8, _a: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn clear_halt(&self, _ep: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn reset(&self) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn kernel_driver_active(&self, _i: u8) -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn detach_kernel_driver(&self, _i: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn attach_kernel_driver(&self, _i: u8) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn submit(&self, _t: &Arc<Inner>, _st: &mut State) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn cancel(&self, _t: &Arc<Inner>) -> Result<()> {
        Err(Error::new(ErrorKind::NotSupported))
    }
    pub(crate) fn cancel_all(&self) {}
}

#[derive(Default)]
pub(crate) struct TransferData;

pub(crate) fn errno_kind(_code: i32) -> ErrorKind {
    ErrorKind::Io
}
