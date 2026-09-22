//! windows backend (not written yet): behaves like the unsupported backend.

#[path = "unsupported.rs"]
mod unsupported;
pub(crate) use unsupported::*;
