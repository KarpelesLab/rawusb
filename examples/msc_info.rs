//! Identifies a USB mass-storage device and prints its partition table's
//! first sector signature. Read-only.
//!
//! Run with `cargo run --features msc --example msc_info -- 0781:5567`. On
//! Linux the device disappears from the block layer while this runs: never
//! use it on a mounted drive.

use rawusb::msc::MassStorage;
use std::io::Read;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).ok_or("usage: msc_info vvvv:pppp")?;
    let (v, p) = arg.split_once(':').ok_or("expected vvvv:pppp")?;
    let ctx = rawusb::Context::new()?;
    let dev = ctx
        .find_device(u16::from_str_radix(v, 16)?, u16::from_str_radix(p, 16)?)?
        .ok_or("device not found")?;
    let msc = MassStorage::open(&dev)?;
    for lun in 0..=msc.max_lun() {
        let info = msc.inquiry(lun)?;
        print!(
            "LUN {lun}: {} {} {} (type {}, removable {})",
            info.vendor, info.product, info.revision, info.device_type, info.removable
        );
        match msc.block_device(lun) {
            Ok(mut disk) => {
                let cap = disk.capacity();
                let mut sector = vec![0u8; cap.block_size as usize];
                disk.read_exact(&mut sector)?;
                let signature = sector.len() >= 512 && sector[510] == 0x55 && sector[511] == 0xaa;
                println!(
                    ": {} x {} bytes = {:.1} GB, write protected {:?}, boot signature {}",
                    cap.block_count,
                    cap.block_size,
                    cap.bytes() as f64 / 1e9,
                    msc.is_write_protected(lun).ok(),
                    if signature { "present" } else { "absent" }
                );
            }
            Err(e) => println!(": not ready ({e})"),
        }
    }
    Ok(())
}
