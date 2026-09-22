//! Watches for USB devices being plugged in and unplugged.
//!
//! Run with `cargo run --features hotplug --example hotplug`, optionally
//! passing a vendor id in hex to narrow it down:
//! `cargo run --features hotplug --example hotplug -- 046d`

fn main() -> rawusb::Result<()> {
    let ctx = rawusb::Context::new()?;
    let mut builder = ctx.hotplug().enumerate_existing(true);
    if let Some(arg) = std::env::args().nth(1) {
        let vendor_id = u16::from_str_radix(arg.trim_start_matches("0x"), 16).expect("vendor id in hex");
        builder = builder.vendor_id(vendor_id);
        println!("watching vendor {vendor_id:04x}; plug something in or out (ctrl-c to stop)");
    } else {
        println!("watching every device; plug something in or out (ctrl-c to stop)");
    }

    for event in builder.watch()?.iter() {
        let dev = event.device();
        let d = dev.device_descriptor();
        let verb = if event.is_arrival() { "arrived" } else { "left  " };
        println!(
            "{verb} bus {:03} device {:03}  {:04x}:{:04x}  {}",
            dev.bus_number(),
            dev.address(),
            d.vendor_id,
            d.product_id,
            // The product string needs the device to still be there, so only
            // try for arrivals.
            if event.is_arrival() {
                dev.open()
                    .ok()
                    .and_then(|h| h.read_product_string().ok().flatten())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        );
    }
    Ok(())
}
