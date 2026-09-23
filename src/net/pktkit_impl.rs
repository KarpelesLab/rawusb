//! [`pktkit::L2Device`] for [`NetDevice`]: a USB network adapter as a pktkit
//! Layer 2 device.

use super::NetDevice;
use pktkit::{DeviceStats, Frame, L2Device, L2Handler, MacAddr};
use std::io;

/// Frames are handed to the pktkit handler on rawusb's event thread,
/// borrowed from the transfer buffer (no copy). `send` never blocks, so a
/// handler that forwards into another USB adapter (through an `L2Hub`, say)
/// cannot deadlock the event thread; a full transmit queue drops the frame,
/// as a NIC would.
impl L2Device for NetDevice {
    fn set_handler(&self, h: L2Handler) {
        self.set_receive_handler(move |bytes| {
            // The handler's error has nowhere to go; pktkit devices count
            // such failures as drops on the sending side.
            let _ = h(Frame::from_slice(bytes));
        });
    }

    fn send(&self, frame: &Frame) -> pktkit::Result<()> {
        NetDevice::send(self, frame.as_bytes()).map_err(io::Error::from)
    }

    fn hw_addr(&self) -> MacAddr {
        MacAddr::new(self.mac_address())
    }

    fn close(&self) -> pktkit::Result<()> {
        NetDevice::close(self).map_err(io::Error::from)
    }

    fn stats(&self) -> Option<&DeviceStats> {
        Some(&self.shared.counters.pktkit)
    }
}
