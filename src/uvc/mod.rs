//! USB Video Class (UVC 1.0-1.5): webcams, capture cards, USB microscopes.
//!
//! This module is behind the `uvc` cargo feature.
//!
//! [`Camera`] parses the video function's descriptors (formats, frame sizes,
//! frame rates, control topology), gets and sets camera controls, and starts
//! a [`Stream`]: it negotiates the parameters with the device
//! (probe/commit), picks the alternate setting with enough bandwidth, keeps
//! isochronous or bulk transfers queued, and reassembles payloads into
//! [`Frame`]s.
//!
//! ```no_run
//! use rawusb::uvc::Camera;
//! use std::time::Duration;
//!
//! let ctx = rawusb::Context::new()?;
//! let dev = ctx.find_device(0x046d, 0x0825)?.expect("camera not plugged in");
//! let cam = Camera::open(&dev)?;
//! for vs in cam.streaming_interfaces() {
//!     for f in &vs.formats {
//!         let sizes: Vec<_> = f.frames.iter().map(|fr| (fr.width, fr.height)).collect();
//!         println!("{}: {sizes:?}", String::from_utf8_lossy(&f.fourcc()));
//!     }
//! }
//! let request = cam.find_format(*b"MJPG", 640, 480).expect("no 640x480 MJPEG");
//! let stream = cam.start(&request)?;
//! let frame = stream.next_frame(Duration::from_secs(2))?;
//! std::fs::write("frame.jpg", &frame.data)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Platform notes
//!
//! On Linux the `uvcvideo` driver is detached while the [`Camera`] lives (the
//! `/dev/video*` nodes disappear). On macOS, UVC devices are driven from user
//! space on recent releases and may be claimable; older releases and Windows
//! need the device bound to a generic driver (WinUSB on Windows).
//! Isochronous streaming on macOS is experimental in this crate.

mod descriptors;
mod stream;

pub use descriptors::{
    ControlInterface, Entity, EntityKind, FormatDescriptor, FormatKind, FrameDescriptor, FrameIntervals, StreamingInterface,
};
pub use stream::{Frame, StreamControl};

use crate::class::{self, Claim};
use crate::device::Device;
use crate::handle::DeviceHandle;
use crate::types::{ControlType, Direction, Recipient, TransferType, request_type};
use crate::{Error, ErrorKind, Result};
use std::time::Duration;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
/// Probe/commit can take a while on some cameras (they may reconfigure the
/// sensor), so it gets a longer timeout than plain controls.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Packets per isochronous transfer.
const ISO_PACKETS: usize = 32;

const SET_CUR: u8 = 0x01;
const VS_PROBE_CONTROL: u8 = 0x01;
const VS_COMMIT_CONTROL: u8 = 0x02;

/// A GET request on a control (UVC 1.5 §4.2): which value to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlRequest {
    /// The current value.
    Cur,
    /// The minimum value.
    Min,
    /// The maximum value.
    Max,
    /// The resolution (step).
    Res,
    /// The default value.
    Def,
    /// The control's length in bytes (a 2-byte answer).
    Len,
    /// The capabilities bitmap (a 1-byte answer: bit 0 GET supported, bit 1
    /// SET supported, bit 3 auto mode active, ...).
    Info,
}

impl ControlRequest {
    const fn code(self) -> u8 {
        match self {
            ControlRequest::Cur => 0x81,
            ControlRequest::Min => 0x82,
            ControlRequest::Max => 0x83,
            ControlRequest::Res => 0x84,
            ControlRequest::Len => 0x85,
            ControlRequest::Info => 0x86,
            ControlRequest::Def => 0x87,
        }
    }
}

/// Camera terminal control selectors (UVC 1.5 Table A-12), for use with the
/// ID of [`ControlInterface::camera_terminal`]. Sizes are in bytes.
pub mod ct {
    /// Scanning mode, 1.
    pub const SCANNING_MODE: u8 = 0x01;
    /// Auto-exposure mode bitmap, 1 (1 manual, 2 auto, 4 shutter priority,
    /// 8 aperture priority).
    pub const AE_MODE: u8 = 0x02;
    /// Auto-exposure priority, 1.
    pub const AE_PRIORITY: u8 = 0x03;
    /// Exposure time in 100 µs units, 4 (unsigned).
    pub const EXPOSURE_TIME_ABSOLUTE: u8 = 0x04;
    /// Exposure time step, 1 (signed).
    pub const EXPOSURE_TIME_RELATIVE: u8 = 0x05;
    /// Focus distance in millimetres, 2.
    pub const FOCUS_ABSOLUTE: u8 = 0x06;
    /// Focus step, 2.
    pub const FOCUS_RELATIVE: u8 = 0x07;
    /// Continuous autofocus on/off, 1.
    pub const FOCUS_AUTO: u8 = 0x08;
    /// Iris, 2.
    pub const IRIS_ABSOLUTE: u8 = 0x09;
    /// Iris step, 1.
    pub const IRIS_RELATIVE: u8 = 0x0a;
    /// Zoom, 2.
    pub const ZOOM_ABSOLUTE: u8 = 0x0b;
    /// Zoom speed, 3.
    pub const ZOOM_RELATIVE: u8 = 0x0c;
    /// Pan and tilt in arc-seconds, 8 (two signed 32-bit values).
    pub const PANTILT_ABSOLUTE: u8 = 0x0d;
    /// Pan/tilt speed, 4.
    pub const PANTILT_RELATIVE: u8 = 0x0e;
    /// Roll, 2.
    pub const ROLL_ABSOLUTE: u8 = 0x0f;
    /// Roll speed, 2.
    pub const ROLL_RELATIVE: u8 = 0x10;
    /// Privacy shutter, 1.
    pub const PRIVACY: u8 = 0x11;
}

/// Processing unit control selectors (UVC 1.5 Table A-13), for use with the
/// ID of [`ControlInterface::processing_unit`]. Sizes are in bytes.
pub mod pu {
    /// Backlight compensation, 2.
    pub const BACKLIGHT_COMPENSATION: u8 = 0x01;
    /// Brightness, 2 (signed).
    pub const BRIGHTNESS: u8 = 0x02;
    /// Contrast, 2.
    pub const CONTRAST: u8 = 0x03;
    /// Gain, 2.
    pub const GAIN: u8 = 0x04;
    /// Power line frequency, 1 (0 off, 1 50 Hz, 2 60 Hz, 3 auto).
    pub const POWER_LINE_FREQUENCY: u8 = 0x05;
    /// Hue, 2 (signed).
    pub const HUE: u8 = 0x06;
    /// Saturation, 2.
    pub const SATURATION: u8 = 0x07;
    /// Sharpness, 2.
    pub const SHARPNESS: u8 = 0x08;
    /// Gamma, 2.
    pub const GAMMA: u8 = 0x09;
    /// White balance temperature in kelvin, 2.
    pub const WHITE_BALANCE_TEMPERATURE: u8 = 0x0a;
    /// Automatic white balance temperature, 1.
    pub const WHITE_BALANCE_TEMPERATURE_AUTO: u8 = 0x0b;
    /// White balance components, 4.
    pub const WHITE_BALANCE_COMPONENT: u8 = 0x0c;
    /// Automatic white balance components, 1.
    pub const WHITE_BALANCE_COMPONENT_AUTO: u8 = 0x0d;
    /// Digital multiplier, 2.
    pub const DIGITAL_MULTIPLIER: u8 = 0x0e;
    /// Digital multiplier limit, 2.
    pub const DIGITAL_MULTIPLIER_LIMIT: u8 = 0x0f;
    /// Automatic hue, 1.
    pub const HUE_AUTO: u8 = 0x10;
    /// Analog video standard, 1.
    pub const ANALOG_VIDEO_STANDARD: u8 = 0x11;
    /// Analog video lock status, 1.
    pub const ANALOG_LOCK_STATUS: u8 = 0x12;
    /// Automatic contrast, 1.
    pub const CONTRAST_AUTO: u8 = 0x13;
}

/// What to stream: a streaming interface, format, frame size and interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamRequest {
    /// The VideoStreaming interface number.
    pub interface: u8,
    /// `bFormatIndex`.
    pub format_index: u8,
    /// `bFrameIndex`.
    pub frame_index: u8,
    /// Frame interval in 100 ns units (333333 for 30 fps); 0 for the frame's
    /// default.
    pub frame_interval: u32,
}

/// An open video function. See the [module documentation](self).
#[derive(Debug)]
pub struct Camera {
    claim: Claim,
    control: ControlInterface,
    streaming: Vec<StreamingInterface>,
}

impl Camera {
    /// Opens the first video function of a device.
    pub fn open(device: &Device) -> Result<Camera> {
        let vc = class::find_interface(device, "device has no video control interface", is_video_control)?;
        Self::open_interface(device.open()?, vc.number)
    }

    /// Opens a video function given its VideoControl interface. The control
    /// interface and all its streaming interfaces are claimed.
    pub fn open_interface(handle: DeviceHandle, control_interface: u8) -> Result<Camera> {
        let cfg = handle.device().active_config_descriptor()?;
        let vc = class::interface(handle.device(), control_interface)?;
        if !is_video_control(&vc) {
            return Err(Error::with_message(ErrorKind::InvalidParam, "not a video control interface"));
        }
        let control =
            ControlInterface::parse(&vc).ok_or_else(|| Error::with_message(ErrorKind::Io, "malformed video control descriptors"))?;
        let streaming: Vec<StreamingInterface> = control
            .streaming_interfaces
            .iter()
            .filter_map(|&n| cfg.interface(n))
            .filter_map(|i| StreamingInterface::parse(i.alt_setting(0).unwrap_or_else(|| i.first())))
            .collect();
        let mut interfaces = vec![control_interface];
        interfaces.extend(streaming.iter().map(|s| s.interface));
        let claim = Claim::new(handle, &interfaces)?;
        Ok(Camera { claim, control, streaming })
    }

    /// The underlying device handle.
    pub fn handle(&self) -> &DeviceHandle {
        self.claim.handle()
    }

    /// The VideoControl interface: UVC version, clock, terminals and units.
    pub fn control_interface(&self) -> &ControlInterface {
        &self.control
    }

    /// The VideoStreaming interfaces (video inputs to the host) with their
    /// formats and frame sizes.
    pub fn streaming_interfaces(&self) -> &[StreamingInterface] {
        &self.streaming
    }

    /// Finds a format by FourCC and frame size on any streaming interface,
    /// at the frame's default interval.
    pub fn find_format(&self, fourcc: [u8; 4], width: u16, height: u16) -> Option<StreamRequest> {
        self.streaming.iter().find_map(|vs| {
            let format = vs.format_by_fourcc(fourcc)?;
            let frame = format.frames.iter().find(|f| f.width == width && f.height == height)?;
            Some(StreamRequest {
                interface: vs.interface,
                format_index: format.index,
                frame_index: frame.index,
                frame_interval: 0,
            })
        })
    }

    // ----- controls --------------------------------------------------------

    /// Reads a unit or terminal control. `entity` is the unit/terminal ID
    /// (0 addresses the interface itself); `selector` comes from [`ct`],
    /// [`pu`] or an extension unit's documentation. Returns the bytes read.
    pub fn get_control(&self, entity: u8, selector: u8, request: ControlRequest, buf: &mut [u8]) -> Result<usize> {
        self.handle().control_read(
            request_type(Direction::In, ControlType::Class, Recipient::Interface),
            request.code(),
            (selector as u16) << 8,
            (entity as u16) << 8 | self.control.interface as u16,
            buf,
            CONTROL_TIMEOUT,
        )
    }

    /// Writes a unit or terminal control (SET_CUR).
    pub fn set_control(&self, entity: u8, selector: u8, data: &[u8]) -> Result<()> {
        self.handle().control_write(
            request_type(Direction::Out, ControlType::Class, Recipient::Interface),
            SET_CUR,
            (selector as u16) << 8,
            (entity as u16) << 8 | self.control.interface as u16,
            data,
            CONTROL_TIMEOUT,
        )?;
        Ok(())
    }

    /// Reads a control of up to 4 bytes as an integer, sign-extending when
    /// `signed` (brightness and hue are signed; most others are not).
    pub fn get_control_int(&self, entity: u8, selector: u8, request: ControlRequest, size: usize, signed: bool) -> Result<i64> {
        if !(1..=4).contains(&size) {
            return Err(Error::with_message(ErrorKind::InvalidParam, "integer controls are 1 to 4 bytes"));
        }
        let mut b = [0u8; 4];
        let n = self.get_control(entity, selector, request, &mut b[..size])?;
        if n != size {
            return Err(Error::with_message(ErrorKind::Io, "short control value"));
        }
        let raw = u32::from_le_bytes(b) as i64;
        let bits = 8 * size as u32;
        Ok(if signed && raw >> (bits - 1) & 1 != 0 {
            raw - (1i64 << bits)
        } else {
            raw
        })
    }

    /// Writes a control of up to 4 bytes from an integer.
    pub fn set_control_int(&self, entity: u8, selector: u8, size: usize, value: i64) -> Result<()> {
        if !(1..=4).contains(&size) {
            return Err(Error::with_message(ErrorKind::InvalidParam, "integer controls are 1 to 4 bytes"));
        }
        self.set_control(entity, selector, &(value as u32).to_le_bytes()[..size])
    }

    // ----- streaming -------------------------------------------------------

    fn streaming(&self, interface: u8) -> Result<&StreamingInterface> {
        self.streaming
            .iter()
            .find(|s| s.interface == interface)
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "not a streaming interface of this camera"))
    }

    fn vs_control(&self, interface: u8, selector: u8, request: u8, data: &mut [u8]) -> Result<usize> {
        let setup_dir = if request & 0x80 != 0 { Direction::In } else { Direction::Out };
        let rt = request_type(setup_dir, ControlType::Class, Recipient::Interface);
        if setup_dir == Direction::In {
            self.handle()
                .control_read(rt, request, (selector as u16) << 8, interface as u16, data, NEGOTIATION_TIMEOUT)
        } else {
            self.handle()
                .control_write(rt, request, (selector as u16) << 8, interface as u16, data, NEGOTIATION_TIMEOUT)
        }
    }

    /// Runs one probe exchange: proposes `control` and returns what the
    /// device made of it. Useful to learn the negotiated frame and payload
    /// sizes without streaming.
    pub fn probe(&self, interface: u8, control: &StreamControl) -> Result<StreamControl> {
        let size = StreamControl::wire_size(self.control.uvc_version);
        let mut b = control.to_bytes();
        self.vs_control(interface, VS_PROBE_CONTROL, SET_CUR, &mut b[..size])?;
        let mut reply = [0u8; 48];
        let n = self.vs_control(interface, VS_PROBE_CONTROL, ControlRequest::Cur.code(), &mut reply[..size])?;
        Ok(StreamControl::from_bytes(&reply[..n]))
    }

    /// Negotiates and starts streaming. The stream stops when the returned
    /// [`Stream`] is dropped.
    pub fn start(&self, request: &StreamRequest) -> Result<Stream<'_>> {
        let vs = self.streaming(request.interface)?;
        let format = vs
            .format(request.format_index)
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "no such format"))?;
        let frame = format
            .frame(request.frame_index)
            .ok_or_else(|| Error::with_message(ErrorKind::InvalidParam, "no such frame"))?;
        let interval = if request.frame_interval == 0 {
            frame.default_interval
        } else {
            request.frame_interval
        };

        let negotiated = self.probe(vs.interface, &StreamControl::new(format.index, frame.index, interval))?;
        let size = StreamControl::wire_size(self.control.uvc_version);
        let mut commit = negotiated.to_bytes();
        self.vs_control(vs.interface, VS_COMMIT_CONTROL, SET_CUR, &mut commit[..size])?;

        let max_frame_size = [negotiated.max_video_frame_size, frame.max_frame_size]
            .into_iter()
            .find(|&s| s != 0)
            .unwrap_or(frame.width as u32 * frame.height as u32 * 2) as usize;
        let payload = negotiated.max_payload_transfer_size as usize;
        let cfg = self.handle().device().active_config_descriptor()?;
        let alts = cfg
            .interface(vs.interface)
            .ok_or_else(|| Error::with_message(ErrorKind::NotFound, "streaming interface vanished"))?;

        let bulk = alts
            .alt_setting(0)
            .and_then(|a| a.endpoint(vs.endpoint))
            .filter(|e| e.transfer_type() == TransferType::Bulk);
        let (mode, pump) = if let Some(ep) = bulk {
            // Bulk: one payload per transfer, rounded up to whole packets.
            let packet = ep.packet_size().max(1) as usize;
            let len = payload.max(packet).div_ceil(packet) * packet;
            let pump = stream::Pump::start(self.handle(), TransferType::Bulk, vs.endpoint, len, 1, max_frame_size)?;
            (Mode::Bulk, pump)
        } else {
            // Isochronous: the smallest alternate setting whose bandwidth per
            // interval covers the payload, else the largest one.
            let mut candidates: Vec<(u32, u8)> = alts
                .alt_settings
                .iter()
                .filter_map(|a| {
                    let e = a.endpoint(vs.endpoint)?;
                    (e.transfer_type() == TransferType::Isochronous).then(|| (iso_bandwidth(e), a.alternate_setting))
                })
                .filter(|&(bw, _)| bw > 0)
                .collect();
            candidates.sort();
            let (bandwidth, alt) = candidates
                .iter()
                .find(|&&(bw, _)| bw as usize >= payload)
                .or(candidates.last())
                .copied()
                .ok_or_else(|| Error::with_message(ErrorKind::NotSupported, "no isochronous alternate setting for the video endpoint"))?;
            self.handle().set_alternate_setting(vs.interface, alt)?;
            let pump = stream::Pump::start(
                self.handle(),
                TransferType::Isochronous,
                vs.endpoint,
                bandwidth as usize,
                ISO_PACKETS,
                max_frame_size,
            );
            match pump {
                Ok(p) => (Mode::Isochronous, p),
                Err(e) => {
                    let _ = self.handle().set_alternate_setting(vs.interface, 0);
                    return Err(e);
                }
            }
        };
        Ok(Stream {
            camera: self,
            interface: vs.interface,
            endpoint: vs.endpoint,
            mode,
            control: negotiated,
            pump,
        })
    }
}

fn is_video_control(a: &crate::InterfaceDescriptor) -> bool {
    a.class == crate::types::class::VIDEO && a.sub_class == 0x01
}

/// Bytes an isochronous endpoint moves per service interval, sized the way
/// the Windows backend expects isochronous packets to be.
fn iso_bandwidth(e: &crate::EndpointDescriptor) -> u32 {
    match e.ss_companion {
        Some(c) if c.bytes_per_interval != 0 => c.bytes_per_interval as u32,
        _ => e.max_packet_size(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Isochronous,
    Bulk,
}

/// A running video stream. Frames are queued (four at most; older ones are
/// kept and newer ones dropped when the reader falls behind) until read with
/// [`next_frame`](Self::next_frame). Dropping the stream stops it.
pub struct Stream<'a> {
    camera: &'a Camera,
    interface: u8,
    endpoint: u8,
    mode: Mode,
    control: StreamControl,
    pump: stream::Pump,
}

impl std::fmt::Debug for Stream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("interface", &self.interface)
            .field("mode", &self.mode)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

impl Stream<'_> {
    /// The parameters the device committed to.
    pub fn control(&self) -> &StreamControl {
        &self.control
    }

    /// Waits for the next complete frame. Fails with [`ErrorKind::Timeout`]
    /// if none arrives in time, or with the transfer error that stopped the
    /// stream (for example [`ErrorKind::NoDevice`] after an unplug).
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        self.pump.next(timeout)
    }

    /// Frames dropped so far because the queue was full.
    pub fn dropped_frames(&self) -> u64 {
        self.pump.dropped()
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        self.pump.stop();
        let h = self.camera.handle();
        match self.mode {
            // Alternate setting 0 releases the reserved bandwidth.
            Mode::Isochronous => {
                let _ = h.set_alternate_setting(self.interface, 0);
            }
            // Bulk devices stop streaming when the endpoint is reset.
            Mode::Bulk => {
                let _ = h.clear_halt(self.endpoint);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_bandwidth_uses_companion_then_mult() {
        let cfg = crate::ConfigDescriptor::from_bytes(&descriptors::tests::webcam_config()).unwrap();
        let alt1 = cfg.interface(1).unwrap().alt_setting(1).unwrap();
        assert_eq!(iso_bandwidth(&alt1.endpoints[0]), 3072);
        let mut ss = alt1.endpoints[0].clone();
        ss.ss_companion = Some(crate::descriptors::SsEndpointCompanion {
            max_burst: 3,
            attributes: 0,
            bytes_per_interval: 4096,
        });
        assert_eq!(iso_bandwidth(&ss), 4096);
    }
}
