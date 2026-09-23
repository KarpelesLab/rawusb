//! A minimal serial monitor: prints what a USB serial adapter receives and
//! sends each line typed on stdin.
//!
//! Run with `cargo run --features serial --example serial_monitor -- 0403:6001 115200`.
//! For a multi-port FTDI chip, add the interface: `0403:6010:1`. Quit with
//! ctrl-D: on Linux the kernel serial driver is re-attached on a normal exit,
//! but not when the process is killed with ctrl-C.

use rawusb::serial::{LineConfig, SerialPort};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let spec = args.next().ok_or("usage: serial_monitor vvvv:pppp[:iface] [baud]")?;
    let baud: u32 = args.next().map_or(Ok(115_200), |b| b.parse())?;
    let parts: Vec<&str> = spec.split(':').collect();
    let (vid, pid) = (
        u16::from_str_radix(parts[0], 16)?,
        u16::from_str_radix(parts.get(1).ok_or("expected vvvv:pppp")?, 16)?,
    );

    let ctx = rawusb::Context::new()?;
    let dev = ctx.find_device(vid, pid)?.ok_or("device not found")?;
    let port = match parts.get(2) {
        Some(i) if vid == rawusb::serial::ftdi::VENDOR_ID => SerialPort::open_ftdi(dev.open()?, i.parse()?)?,
        Some(i) => SerialPort::open_cdc_acm(dev.open()?, i.parse()?)?,
        None => SerialPort::open(&dev)?,
    };
    port.set_line_config(&LineConfig::new(baud))?;
    port.set_dtr(true)?;
    port.set_rts(true)?;
    eprintln!("{:?} at {baud} baud; type lines to send, ctrl-D to quit", port.kind());

    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut buf = [0u8; 512];
            let mut out = std::io::stdout();
            while !done.load(Ordering::Relaxed) {
                match port.read_with_timeout(&mut buf, Duration::from_millis(200)) {
                    Ok(n) => {
                        let _ = out.write_all(&buf[..n]);
                        let _ = out.flush();
                    }
                    Err(e) if e.is_timeout() => {}
                    Err(e) => {
                        eprintln!("read failed: {e}");
                        return;
                    }
                }
            }
        });
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            for line in std::io::stdin().lock().lines() {
                let mut line = line?;
                line.push_str("\r\n");
                port.write_with_timeout(line.as_bytes(), Duration::from_secs(2))?;
            }
            Ok(())
        })();
        // Stop the reader; the scope joins it, then the port is dropped and
        // the kernel driver re-attached.
        done.store(true, Ordering::Relaxed);
        result
    })
}
