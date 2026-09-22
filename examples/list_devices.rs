//! Lists every USB device on the system, `lsusb` style, and reads the
//! product strings of those we are allowed to open.

use rawusb::Context;

fn main() -> rawusb::Result<()> {
    let ctx = Context::new()?;
    for dev in ctx.devices()? {
        let d = dev.device_descriptor();
        let ports: Vec<String> = dev.port_numbers().iter().map(|p| p.to_string()).collect();
        print!(
            "Bus {:03} Device {:03}: ID {:04x}:{:04x} (ports {}, {}, USB {}, {} config{})",
            dev.bus_number(),
            dev.address(),
            d.vendor_id,
            d.product_id,
            if ports.is_empty() { "root".to_string() } else { ports.join(".") },
            dev.speed(),
            d.usb_version,
            dev.num_configurations(),
            if dev.num_configurations() == 1 { "" } else { "s" },
        );
        match dev.open() {
            Ok(h) => {
                let product = h.read_product_string().ok().flatten().unwrap_or_default();
                let manufacturer = h.read_manufacturer_string().ok().flatten().unwrap_or_default();
                let serial = h.read_serial_number_string().ok().flatten();
                print!(" {manufacturer} {product}");
                if let Some(s) = serial {
                    print!(" [{s}]");
                }
                if let Ok(c) = h.active_configuration() {
                    print!(" cfg={c}");
                }
            }
            Err(e) => print!(" (not opened: {e})"),
        }
        println!();
        if std::env::args().any(|a| a == "-v") {
            for i in 0..dev.num_configurations() {
                let cfg = dev.config_descriptor(i)?;
                println!(
                    "  Configuration {} ({} mA, {} interfaces)",
                    cfg.configuration_value,
                    cfg.max_power_ma(),
                    cfg.interfaces.len()
                );
                for iface in &cfg.interfaces {
                    for alt in &iface.alt_settings {
                        println!(
                            "    Interface {} alt {}: class {:02x}/{:02x}/{:02x}, {} endpoints",
                            alt.number,
                            alt.alternate_setting,
                            alt.class,
                            alt.sub_class,
                            alt.protocol,
                            alt.endpoints.len()
                        );
                        for ep in &alt.endpoints {
                            println!(
                                "      EP {:#04x} {:?} {:?} max {} interval {}",
                                ep.address,
                                ep.direction(),
                                ep.transfer_type(),
                                ep.max_packet_size(),
                                ep.interval
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
