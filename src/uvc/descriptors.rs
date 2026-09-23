//! UVC class-specific descriptors (UVC 1.5 §3.7 and §3.9, plus the payload
//! format specifications for uncompressed, MJPEG and frame-based video).

use crate::class::{self, le16};
use crate::descriptors::InterfaceDescriptor;

const CS_INTERFACE: u8 = 0x24;

/// VideoControl descriptor subtypes.
mod vc {
    pub(super) const HEADER: u8 = 0x01;
    pub(super) const INPUT_TERMINAL: u8 = 0x02;
    pub(super) const OUTPUT_TERMINAL: u8 = 0x03;
    pub(super) const SELECTOR_UNIT: u8 = 0x04;
    pub(super) const PROCESSING_UNIT: u8 = 0x05;
    pub(super) const EXTENSION_UNIT: u8 = 0x06;
    pub(super) const ENCODING_UNIT: u8 = 0x07;
}

/// VideoStreaming descriptor subtypes.
mod vs {
    pub(super) const INPUT_HEADER: u8 = 0x01;
    pub(super) const FORMAT_UNCOMPRESSED: u8 = 0x04;
    pub(super) const FRAME_UNCOMPRESSED: u8 = 0x05;
    pub(super) const FORMAT_MJPEG: u8 = 0x06;
    pub(super) const FRAME_MJPEG: u8 = 0x07;
    pub(super) const FORMAT_FRAME_BASED: u8 = 0x10;
    pub(super) const FRAME_FRAME_BASED: u8 = 0x11;
}

fn le32(b: &[u8], at: usize) -> u32 {
    le16(b, at) as u32 | (le16(b, at + 2) as u32) << 16
}

/// Little-endian bitmap of `size` bytes at `at`, truncated to 64 bits.
fn bitmap(b: &[u8], at: usize, size: usize) -> u64 {
    (0..size.min(8)).fold(0, |acc, i| acc | (b.get(at + i).copied().unwrap_or(0) as u64) << (8 * i))
}

/// Walks the class-specific (CS_INTERFACE) descriptors of an interface, as
/// `(subtype, descriptor)`. Some devices put them after the endpoints, where
/// they end up in the endpoint's `extra`.
fn cs_interface(iface: &InterfaceDescriptor) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::once(&iface.extra)
        .chain(iface.endpoints.iter().map(|e| &e.extra))
        .flat_map(|extra| class::descriptors(extra))
        .filter(|(ty, d)| *ty == CS_INTERFACE && d.len() >= 3)
        .map(|(_, d)| (d[2], d))
}

/// A terminal or unit in the camera's control topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// `bTerminalID` / `bUnitID`: the value to address controls with.
    pub id: u8,
    /// What it is.
    pub kind: EntityKind,
    /// Controls bitmap (`bmControls`), bit `n` meaning control selector
    /// `n + 1` is supported. 0 for entities without controls.
    pub controls: u64,
}

/// The kind of a terminal or unit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntityKind {
    /// The camera sensor (input terminal of type 0x0201).
    CameraTerminal,
    /// Another input terminal, with its `wTerminalType`.
    InputTerminal(u16),
    /// An output terminal (normally the USB streaming terminal).
    OutputTerminal(u16),
    /// A selector unit.
    SelectorUnit,
    /// The processing unit: brightness, contrast, white balance, ...
    ProcessingUnit,
    /// A vendor extension unit, identified by its GUID.
    ExtensionUnit {
        /// `guidExtensionCode`, in wire byte order.
        guid: [u8; 16],
        /// `bNumControls`.
        num_controls: u8,
    },
    /// An encoding unit (UVC 1.5).
    EncodingUnit,
}

/// The VideoControl interface: UVC version and control topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlInterface {
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// `bcdUVC`.
    pub uvc_version: u16,
    /// `dwClockFrequency` (Hz), the unit of the payload timestamps.
    pub clock_frequency: u32,
    /// The streaming interfaces that belong to this function.
    pub streaming_interfaces: Vec<u8>,
    /// Terminals and units.
    pub entities: Vec<Entity>,
}

impl ControlInterface {
    /// Parses the class-specific descriptors of a VideoControl interface.
    pub fn parse(iface: &InterfaceDescriptor) -> Option<ControlInterface> {
        let mut out = ControlInterface {
            interface: iface.number,
            uvc_version: 0,
            clock_frequency: 0,
            streaming_interfaces: Vec::new(),
            entities: Vec::new(),
        };
        let mut seen_header = false;
        for (subtype, d) in cs_interface(iface) {
            let entity = |kind, controls| Entity {
                id: d.get(3).copied().unwrap_or(0),
                kind,
                controls,
            };
            match subtype {
                vc::HEADER if d.len() >= 12 => {
                    seen_header = true;
                    out.uvc_version = le16(d, 3);
                    out.clock_frequency = le32(d, 7);
                    let n = d[11] as usize;
                    out.streaming_interfaces = d[12..].iter().take(n).copied().collect();
                }
                vc::INPUT_TERMINAL if d.len() >= 8 => {
                    let ty = le16(d, 4);
                    if ty == 0x0201 && d.len() >= 15 {
                        let size = d[14] as usize;
                        out.entities.push(entity(EntityKind::CameraTerminal, bitmap(d, 15, size)));
                    } else {
                        out.entities.push(entity(EntityKind::InputTerminal(ty), 0));
                    }
                }
                vc::OUTPUT_TERMINAL if d.len() >= 9 => out.entities.push(entity(EntityKind::OutputTerminal(le16(d, 4)), 0)),
                vc::SELECTOR_UNIT if d.len() >= 5 => out.entities.push(entity(EntityKind::SelectorUnit, 0)),
                vc::PROCESSING_UNIT if d.len() >= 8 => {
                    let size = d[7] as usize;
                    out.entities.push(entity(EntityKind::ProcessingUnit, bitmap(d, 8, size)));
                }
                vc::EXTENSION_UNIT if d.len() >= 22 => {
                    let mut guid = [0u8; 16];
                    guid.copy_from_slice(&d[4..20]);
                    let pins = d[21] as usize;
                    let size = d.get(22 + pins).copied().unwrap_or(0) as usize;
                    let controls = bitmap(d, 23 + pins, size);
                    out.entities
                        .push(entity(EntityKind::ExtensionUnit { guid, num_controls: d[20] }, controls));
                }
                vc::ENCODING_UNIT if d.len() >= 5 => out.entities.push(entity(EntityKind::EncodingUnit, 0)),
                _ => {}
            }
        }
        seen_header.then_some(out)
    }

    /// The ID of the camera terminal, if there is one.
    pub fn camera_terminal(&self) -> Option<u8> {
        self.entities.iter().find(|e| e.kind == EntityKind::CameraTerminal).map(|e| e.id)
    }

    /// The ID of the (first) processing unit, if there is one.
    pub fn processing_unit(&self) -> Option<u8> {
        self.entities.iter().find(|e| e.kind == EntityKind::ProcessingUnit).map(|e| e.id)
    }
}

/// How a frame's supported intervals are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameIntervals {
    /// A list of discrete intervals, in 100 ns units.
    Discrete(Vec<u32>),
    /// Any interval from `min` to `max` in steps of `step` (100 ns units).
    Continuous {
        /// Shortest interval.
        min: u32,
        /// Longest interval.
        max: u32,
        /// Granularity.
        step: u32,
    },
}

/// One frame size of a format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameDescriptor {
    /// `bFrameIndex`, used in stream negotiation.
    pub index: u8,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// `dwMaxVideoFrameBufferSize` (0 for frame-based formats, which do not
    /// declare one).
    pub max_frame_size: u32,
    /// Default frame interval, in 100 ns units (333333 is 30 fps).
    pub default_interval: u32,
    /// Supported frame intervals.
    pub intervals: FrameIntervals,
}

impl FrameDescriptor {
    fn parse(d: &[u8], frame_based: bool) -> Option<FrameDescriptor> {
        // Frame-based frames replace the buffer size with dwBytesPerLine
        // after the interval fields.
        let (default_at, type_at, list_at, max_frame_size) = if frame_based { (17, 21, 26, 0) } else { (21, 25, 26, le32(d, 17)) };
        if d.len() < list_at {
            return None;
        }
        let kind = d[type_at] as usize;
        let intervals = if kind == 0 {
            FrameIntervals::Continuous {
                min: le32(d, list_at),
                max: le32(d, list_at + 4),
                step: le32(d, list_at + 8),
            }
        } else {
            FrameIntervals::Discrete((0..kind).map(|i| le32(d, list_at + 4 * i)).take_while(|&v| v != 0).collect())
        };
        Some(FrameDescriptor {
            index: d[3],
            width: le16(d, 5),
            height: le16(d, 7),
            max_frame_size,
            default_interval: le32(d, default_at),
            intervals,
        })
    }

    /// Frames per second at an interval given in 100 ns units.
    pub fn fps(interval: u32) -> f64 {
        if interval == 0 { 0.0 } else { 10_000_000.0 / interval as f64 }
    }
}

/// The encoding of a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatKind {
    /// Raw pixels (YUY2, NV12, ...), identified by GUID.
    Uncompressed {
        /// `guidFormat`, in wire byte order.
        guid: [u8; 16],
        /// Bits per pixel.
        bits_per_pixel: u8,
    },
    /// Motion JPEG: each frame is a JPEG image.
    Mjpeg,
    /// Frame-based compressed video (H.264, HEVC, ...), identified by GUID.
    FrameBased {
        /// `guidFormat`, in wire byte order.
        guid: [u8; 16],
        /// Whether frames vary in size.
        variable_size: bool,
    },
}

/// One video format a streaming interface offers, with its frame sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatDescriptor {
    /// `bFormatIndex`, used in stream negotiation.
    pub index: u8,
    /// The encoding.
    pub kind: FormatKind,
    /// `bDefaultFrameIndex`.
    pub default_frame_index: u8,
    /// Frame sizes.
    pub frames: Vec<FrameDescriptor>,
}

impl FormatDescriptor {
    /// The format's FourCC: the first four bytes of the GUID for GUID-based
    /// formats (`YUY2`, `NV12`, `H264`, ...), `MJPG` for Motion JPEG.
    pub fn fourcc(&self) -> [u8; 4] {
        match self.kind {
            FormatKind::Mjpeg => *b"MJPG",
            FormatKind::Uncompressed { guid, .. } | FormatKind::FrameBased { guid, .. } => [guid[0], guid[1], guid[2], guid[3]],
        }
    }

    /// The frame with the given `bFrameIndex`.
    pub fn frame(&self, index: u8) -> Option<&FrameDescriptor> {
        self.frames.iter().find(|f| f.index == index)
    }
}

/// A VideoStreaming interface: its endpoint and formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingInterface {
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// `bEndpointAddress` of the video data endpoint.
    pub endpoint: u8,
    /// The formats offered, in descriptor order.
    pub formats: Vec<FormatDescriptor>,
}

impl StreamingInterface {
    /// Parses the class-specific descriptors of a VideoStreaming interface
    /// (alternate setting 0). `None` for output (host-to-device) streams
    /// and malformed descriptors.
    pub fn parse(iface: &InterfaceDescriptor) -> Option<StreamingInterface> {
        let mut out: Option<StreamingInterface> = None;
        for (subtype, d) in cs_interface(iface) {
            let target = out.as_mut();
            match (subtype, target) {
                (vs::INPUT_HEADER, None) if d.len() >= 7 => {
                    out = Some(StreamingInterface {
                        interface: iface.number,
                        endpoint: d[6],
                        formats: Vec::new(),
                    });
                }
                (vs::FORMAT_UNCOMPRESSED | vs::FORMAT_FRAME_BASED, Some(s)) if d.len() >= 23 => {
                    let mut guid = [0u8; 16];
                    guid.copy_from_slice(&d[5..21]);
                    let kind = if subtype == vs::FORMAT_UNCOMPRESSED {
                        FormatKind::Uncompressed {
                            guid,
                            bits_per_pixel: d[21],
                        }
                    } else {
                        FormatKind::FrameBased {
                            guid,
                            variable_size: d.get(27).is_some_and(|&v| v != 0),
                        }
                    };
                    s.formats.push(FormatDescriptor {
                        index: d[3],
                        kind,
                        default_frame_index: d[22],
                        frames: Vec::new(),
                    });
                }
                (vs::FORMAT_MJPEG, Some(s)) if d.len() >= 7 => s.formats.push(FormatDescriptor {
                    index: d[3],
                    kind: FormatKind::Mjpeg,
                    default_frame_index: d[6],
                    frames: Vec::new(),
                }),
                (vs::FRAME_UNCOMPRESSED | vs::FRAME_MJPEG | vs::FRAME_FRAME_BASED, Some(s)) => {
                    if let (Some(f), Some(fmt)) = (FrameDescriptor::parse(d, subtype == vs::FRAME_FRAME_BASED), s.formats.last_mut()) {
                        fmt.frames.push(f);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Finds a format by FourCC.
    pub fn format_by_fourcc(&self, fourcc: [u8; 4]) -> Option<&FormatDescriptor> {
        self.formats.iter().find(|f| f.fourcc() == fourcc)
    }

    /// The format with the given `bFormatIndex`.
    pub fn format(&self, index: u8) -> Option<&FormatDescriptor> {
        self.formats.iter().find(|f| f.index == index)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ConfigDescriptor;

    /// A trimmed-down webcam: VC interface 0 with camera terminal, processing
    /// unit and output terminal; VS interface 1 with YUY2 640x480 and MJPEG
    /// 1280x720, and an isochronous alternate setting.
    pub(crate) fn webcam_config() -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 250];
        c.extend([8, 0x0b, 0, 2, 0x0e, 3, 0, 0]); // IAD
        c.extend([9, 4, 0, 0, 1, 0x0e, 1, 0, 0]);
        c.extend([13, 0x24, 1, 0x10, 0x01, 0, 0, 0x80, 0x8d, 0x5b, 0x00, 1, 1]); // header, 6 MHz, VS 1
        c.extend([18, 0x24, 2, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0x0a, 0x00, 0x00]); // camera, 3-byte controls
        c.extend([11, 0x24, 5, 2, 1, 0, 0, 2, 0x7f, 0x15, 0]); // processing unit
        c.extend([9, 0x24, 3, 3, 0x01, 0x01, 0, 2, 0]); // output terminal
        c.extend([7, 5, 0x83, 3, 16, 0, 8]); // status endpoint
        c.extend([9, 4, 1, 0, 0, 0x0e, 2, 0, 0]);
        c.extend([14, 0x24, 1, 2, 0, 0, 0x81, 0, 3, 0, 0, 0, 1, 0]); // input header, ep 0x81
        let mut yuy2 = vec![27, 0x24, 4, 1, 1];
        yuy2.extend(b"YUY2");
        yuy2.extend([0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
        yuy2.extend([16, 1, 0, 0, 0, 0]);
        c.extend(yuy2);
        let mut frame = vec![34, 0x24, 5, 1, 0, 0x80, 0x02, 0xe0, 0x01];
        frame.extend(0u32.to_le_bytes());
        frame.extend(0u32.to_le_bytes());
        frame.extend((640u32 * 480 * 2).to_le_bytes());
        frame.extend(333_333u32.to_le_bytes());
        frame.push(2);
        frame.extend(333_333u32.to_le_bytes());
        frame.extend(666_666u32.to_le_bytes());
        c.extend(frame);
        c.extend([11, 0x24, 6, 2, 1, 1, 1, 0, 0, 0, 0]); // MJPEG format
        let mut frame = vec![38, 0x24, 7, 1, 0, 0x00, 0x05, 0xd0, 0x02];
        frame.extend([0u8; 8]);
        frame.extend((1280u32 * 720 * 2).to_le_bytes());
        frame.extend(333_333u32.to_le_bytes());
        frame.push(0); // continuous
        frame.extend(333_333u32.to_le_bytes());
        frame.extend(10_000_000u32.to_le_bytes());
        frame.extend(333_333u32.to_le_bytes());
        c.extend(frame);
        c.extend([9, 4, 1, 1, 1, 0x0e, 2, 0, 0]);
        c.extend([7, 5, 0x81, 5, 0x00, 0x14, 1]); // 1024 x 3 transactions
        let len = c.len() as u16;
        c[2..4].copy_from_slice(&len.to_le_bytes());
        c
    }

    #[test]
    fn parse_webcam() {
        let cfg = ConfigDescriptor::from_bytes(&webcam_config()).unwrap();
        let vc = ControlInterface::parse(cfg.interface(0).unwrap().first()).unwrap();
        assert_eq!(vc.uvc_version, 0x0110);
        assert_eq!(vc.clock_frequency, 6_000_000);
        assert_eq!(vc.streaming_interfaces, vec![1]);
        assert_eq!(vc.camera_terminal(), Some(1));
        assert_eq!(vc.processing_unit(), Some(2));
        assert_eq!(vc.entities[0].controls, 0x0a);
        assert_eq!(vc.entities[1].controls, 0x157f);
        assert_eq!(vc.entities[2].kind, EntityKind::OutputTerminal(0x0101));

        let vs = StreamingInterface::parse(cfg.interface(1).unwrap().first()).unwrap();
        assert_eq!(vs.endpoint, 0x81);
        assert_eq!(vs.formats.len(), 2);
        let yuy2 = vs.format_by_fourcc(*b"YUY2").unwrap();
        assert_eq!(
            yuy2.kind,
            FormatKind::Uncompressed {
                guid: yuy2_guid(),
                bits_per_pixel: 16
            }
        );
        let f = yuy2.frame(1).unwrap();
        assert_eq!((f.width, f.height, f.max_frame_size), (640, 480, 614_400));
        assert_eq!(f.intervals, FrameIntervals::Discrete(vec![333_333, 666_666]));
        let mjpeg = vs.format(2).unwrap();
        assert_eq!(mjpeg.fourcc(), *b"MJPG");
        assert_eq!(mjpeg.frames[0].width, 1280);
        assert_eq!(
            mjpeg.frames[0].intervals,
            FrameIntervals::Continuous {
                min: 333_333,
                max: 10_000_000,
                step: 333_333
            }
        );
        assert!((FrameDescriptor::fps(333_333) - 30.0).abs() < 0.001);
    }

    fn yuy2_guid() -> [u8; 16] {
        let mut g = [0u8; 16];
        g[..4].copy_from_slice(b"YUY2");
        g[4..].copy_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
        g
    }

    #[test]
    fn truncated_descriptors_are_skipped() {
        let mut iface = ConfigDescriptor::from_bytes(&webcam_config())
            .unwrap()
            .interface(1)
            .unwrap()
            .first()
            .clone();
        // Cut the tree in the middle of the first frame descriptor.
        iface.extra.truncate(14 + 27 + 20);
        let vs = StreamingInterface::parse(&iface).unwrap();
        assert_eq!(vs.formats.len(), 1);
        assert!(vs.formats[0].frames.is_empty());
    }
}
