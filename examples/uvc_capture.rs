//! Lists a webcam's formats and saves one frame.
//!
//! Run with `cargo run --features uvc --example uvc_capture -- 046d:0825`
//! to list, or add `MJPG 640 480` to capture a frame into `frame.mjpg` (or
//! `frame.yuy2`, ...).

use rawusb::uvc::{Camera, FrameDescriptor, FrameIntervals};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (v, p) = args
        .first()
        .and_then(|s| s.split_once(':'))
        .ok_or("usage: uvc_capture vvvv:pppp [FOURCC W H]")?;
    let ctx = rawusb::Context::new()?;
    let dev = ctx
        .find_device(u16::from_str_radix(v, 16)?, u16::from_str_radix(p, 16)?)?
        .ok_or("device not found")?;
    let cam = Camera::open(&dev)?;
    println!("UVC {:#06x}", cam.control_interface().uvc_version);
    for vs in cam.streaming_interfaces() {
        println!("streaming interface {} (endpoint {:#04x})", vs.interface, vs.endpoint);
        for f in &vs.formats {
            println!("  {}", String::from_utf8_lossy(&f.fourcc()));
            for fr in &f.frames {
                let rates: Vec<String> = match &fr.intervals {
                    FrameIntervals::Discrete(v) => v.iter().map(|&i| format!("{:.1}", FrameDescriptor::fps(i))).collect(),
                    FrameIntervals::Continuous { min, max, .. } => {
                        vec![format!("{:.1}-{:.1}", FrameDescriptor::fps(*max), FrameDescriptor::fps(*min))]
                    }
                };
                println!("    {}x{} @ {} fps", fr.width, fr.height, rates.join(", "));
            }
        }
    }
    let [_, fourcc, w, h] = args.as_slice() else { return Ok(()) };
    let fourcc: [u8; 4] = fourcc.as_bytes().try_into().map_err(|_| "FourCC must be 4 characters")?;
    let request = cam.find_format(fourcc, w.parse()?, h.parse()?).ok_or("no such format and size")?;
    let stream = cam.start(&request)?;
    // Skip a few frames: auto-exposure needs a moment to settle.
    let mut frame = stream.next_frame(Duration::from_secs(5))?;
    for _ in 0..10 {
        frame = stream.next_frame(Duration::from_secs(5))?;
    }
    let name = format!("frame.{}", String::from_utf8_lossy(&fourcc).trim().to_lowercase());
    std::fs::write(&name, &frame.data)?;
    println!("saved {} bytes to {name} (error flag {})", frame.data.len(), frame.error);
    Ok(())
}
