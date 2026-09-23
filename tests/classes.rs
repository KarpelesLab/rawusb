//! Class-helper tests against real devices. Each one skips itself unless a
//! device is nominated through the environment, because opening a class
//! helper detaches the kernel driver for as long as the test runs:
//!
//! - `RAWUSB_TEST_HID=vvvv:pppp`: any HID device that is safe to take from
//!   its driver for a moment (not your keyboard).
//! - `RAWUSB_TEST_MSC=vvvv:pppp`: an *unmounted* mass-storage device; only
//!   read commands are sent.
//! - `RAWUSB_TEST_SERIAL=vvvv:pppp[:iface]`: a USB serial adapter (CDC-ACM or
//!   FTDI; `iface` picks an FTDI port). Line settings change; nothing is
//!   sent. Add `RAWUSB_TEST_SERIAL_LINES=1` to also toggle DTR/RTS and send
//!   a break, which some boards wire to their reset circuitry.
//! - `RAWUSB_TEST_UVC=vvvv:pppp`: a webcam; a few frames are captured.
//! - `RAWUSB_TEST_NET=vvvv:pppp`: a USB network function (a phone with USB
//!   tethering, a gadget-mode board). Waits for a frame from the network and
//!   sends a few broadcast frames with the IEEE local experimental EtherType
//!   (0x88b5), which every stack ignores.

#![cfg(any(feature = "hid", feature = "msc", feature = "net", feature = "serial", feature = "uvc"))]

use rawusb::{Context, Device, ErrorKind};
#[allow(unused_imports)]
use std::time::Duration;

fn env_device(var: &str) -> Option<(Device, Vec<String>)> {
    let spec = std::env::var(var).ok()?;
    let parts: Vec<String> = spec.split(':').map(str::to_string).collect();
    let vid = u16::from_str_radix(&parts[0], 16).unwrap();
    let pid = u16::from_str_radix(&parts[1], 16).unwrap();
    let ctx = match Context::new() {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotSupported => return None,
        Err(e) => panic!("{e}"),
    };
    let dev = ctx
        .find_device(vid, pid)
        .unwrap()
        .unwrap_or_else(|| panic!("{var}: {spec} is not plugged in"));
    Some((dev, parts[2..].to_vec()))
}

#[cfg(feature = "hid")]
#[test]
fn hid_report_descriptor_and_input() {
    use rawusb::hid::{HidDevice, ReportType};
    let Some((dev, _)) = env_device("RAWUSB_TEST_HID") else { return };
    let hid = HidDevice::open(&dev).unwrap();
    let d = hid.report_descriptor();
    eprintln!(
        "HID {:?}: {} byte report descriptor, application usages {:04x?}, ids {}",
        hid.hid_descriptor().hid_version,
        hid.raw_report_descriptor().len(),
        d.application_usages,
        d.uses_report_ids()
    );
    assert_eq!(
        hid.raw_report_descriptor().len(),
        hid.hid_descriptor().report_descriptor_length().unwrap() as usize
    );
    assert!(!d.fields.is_empty());
    for kind in [ReportType::Input, ReportType::Output, ReportType::Feature] {
        for id in d.report_ids(kind) {
            eprintln!("  {kind:?} report {id}: {} bytes", d.report_len(kind, id).unwrap());
        }
    }
    // Many devices only report on change; a timeout is as good as a report.
    let mut buf = vec![0u8; hid.max_input_report_len()];
    match hid.read(&mut buf, Duration::from_millis(300)) {
        Ok(n) => eprintln!("  input: {:02x?}", &buf[..n]),
        Err(e) => assert_eq!(e.kind(), ErrorKind::Timeout, "{e}"),
    }
    let handle = hid.handle().clone();
    let iface = hid.interface_number();
    drop(hid);
    // Dropping the helper gives the interface back to the kernel driver.
    std::thread::sleep(Duration::from_millis(200));
    assert!(handle.claimed_interfaces().is_empty());
    if cfg!(target_os = "linux") {
        assert!(handle.kernel_driver_active(iface).unwrap());
    }
}

#[cfg(feature = "msc")]
#[test]
fn msc_inquiry_capacity_and_read() {
    use rawusb::msc::{DataPhase, MassStorage};
    use std::io::{Read, Seek, SeekFrom};
    let Some((dev, _)) = env_device("RAWUSB_TEST_MSC") else { return };
    let msc = MassStorage::open(&dev).unwrap();
    let info = msc.inquiry(0).unwrap();
    eprintln!("{} {} {} (max LUN {})", info.vendor, info.product, info.revision, msc.max_lun());
    let mut disk = msc.block_device(0).unwrap();
    let cap = disk.capacity();
    eprintln!(
        "{} blocks of {} bytes, write protected: {:?}",
        cap.block_count,
        cap.block_size,
        msc.is_write_protected(0)
    );
    assert!(cap.block_count > 0);

    // Block reads, direct and through the stream, agree.
    let bs = cap.block_size as usize;
    let mut direct = vec![0u8; bs * 4];
    disk.read_blocks(0, &mut direct).unwrap();
    let mut streamed = vec![0u8; bs * 4];
    disk.read_exact(&mut streamed[..7]).unwrap();
    disk.read_exact(&mut streamed[7..]).unwrap();
    assert_eq!(direct, streamed);

    // Unaligned read across a block boundary.
    disk.seek(SeekFrom::Start(bs as u64 - 3)).unwrap();
    let mut six = [0u8; 6];
    disk.read_exact(&mut six).unwrap();
    assert_eq!(&six[..], &direct[bs - 3..bs + 3]);

    // Reading at the end returns EOF, and a block past the end is refused.
    disk.seek(SeekFrom::End(-2)).unwrap();
    let mut tail = [0u8; 8];
    assert_eq!(disk.read(&mut tail).unwrap(), 2);
    assert_eq!(disk.read(&mut tail).unwrap(), 0);
    assert!(disk.read_blocks(cap.block_count, &mut direct[..bs]).is_err());

    // A command the device rejects comes back as a failed status plus sense,
    // and the transport stays usable afterwards.
    let r = msc.execute(0, &[0xff, 0, 0, 0, 0, 0], DataPhase::None).unwrap();
    assert!(!r.passed);
    let sense = msc.request_sense(0).unwrap();
    eprintln!("bogus opcode: {sense}");
    assert_eq!(sense.key, rawusb::msc::SenseKey::IllegalRequest);
    msc.test_unit_ready(0).unwrap();
}

#[cfg(feature = "serial")]
#[test]
fn serial_configuration() {
    use rawusb::serial::{DataBits, FlowControl, LineConfig, Parity, SerialKind, SerialPort, StopBits};
    let Some((dev, rest)) = env_device("RAWUSB_TEST_SERIAL") else {
        return;
    };
    let ports = rawusb::serial::ports(&dev).unwrap();
    eprintln!("ports: {ports:?}");
    assert!(!ports.is_empty());
    let port = match rest.first() {
        Some(i) => SerialPort::open_interface(dev.open().unwrap(), i.parse().unwrap()).unwrap(),
        None => SerialPort::open(&dev).unwrap(),
    };
    eprintln!("{:?}", port.kind());

    let config = LineConfig {
        baud_rate: 57_600,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
    };
    port.set_line_config(&config).unwrap();
    assert_eq!(port.line_config().unwrap(), Some(config));
    port.set_baud_rate(115_200).unwrap();
    assert_eq!(port.line_config().unwrap().unwrap().baud_rate, 115_200);
    eprintln!("{:?}", port.modem_status().unwrap());

    if let SerialKind::Ftdi(chip) = port.kind() {
        assert!(port.set_baud_rate(chip.max_baud_rate() + 1).is_err());
        port.set_baud_rate(chip.max_baud_rate()).unwrap();
        port.set_flow_control(FlowControl::RtsCts).unwrap();
        port.set_flow_control(FlowControl::XonXoff { xon: 0x11, xoff: 0x13 }).unwrap();
        port.set_flow_control(FlowControl::None).unwrap();
        port.set_latency_timer(2).unwrap();
        assert_eq!(port.latency_timer().unwrap(), 2);
        port.set_latency_timer(16).unwrap();
        port.purge(true, true).unwrap();
        // Nothing is connected to talk back: the chip keeps sending status
        // packets, which must be swallowed until the timeout.
        let mut buf = [0u8; 16];
        let start = std::time::Instant::now();
        match port.read_with_timeout(&mut buf, Duration::from_millis(200)) {
            Ok(n) => eprintln!("line had data: {:02x?}", &buf[..n]),
            Err(e) => assert_eq!(e.kind(), ErrorKind::Timeout, "{e}"),
        }
        assert!(start.elapsed() < Duration::from_secs(2));
    } else {
        assert_eq!(
            port.set_flow_control(FlowControl::RtsCts).unwrap_err().kind(),
            ErrorKind::NotSupported
        );
    }

    if std::env::var_os("RAWUSB_TEST_SERIAL_LINES").is_some() {
        port.set_dtr(true).unwrap();
        port.set_rts(true).unwrap();
        port.send_break(Duration::from_millis(50)).unwrap();
        port.set_dtr(false).unwrap();
        port.set_rts(false).unwrap();
    }

    let handle = port.handle().clone();
    drop(port);
    assert!(handle.claimed_interfaces().is_empty());
}

#[cfg(feature = "uvc")]
#[test]
fn uvc_capture() {
    use rawusb::uvc::{Camera, ControlRequest, FormatKind};
    let Some((dev, _)) = env_device("RAWUSB_TEST_UVC") else { return };
    let cam = Camera::open(&dev).unwrap();
    let vc = cam.control_interface();
    eprintln!(
        "UVC {:#06x}, clock {} Hz, entities {:?}",
        vc.uvc_version, vc.clock_frequency, vc.entities
    );
    if let Some(pu) = vc.processing_unit() {
        let cur = cam.get_control_int(pu, rawusb::uvc::pu::BRIGHTNESS, ControlRequest::Cur, 2, true);
        let def = cam.get_control_int(pu, rawusb::uvc::pu::BRIGHTNESS, ControlRequest::Def, 2, true);
        eprintln!("brightness {cur:?} (default {def:?})");
    }
    let vs = &cam.streaming_interfaces()[0];
    // Prefer an uncompressed format (sizes are predictable), smallest frame.
    let format = vs
        .formats
        .iter()
        .find(|f| matches!(f.kind, FormatKind::Uncompressed { .. }))
        .unwrap_or(&vs.formats[0]);
    let frame = format.frames.iter().min_by_key(|f| f.width as u32 * f.height as u32).unwrap();
    eprintln!(
        "streaming {} {}x{}",
        String::from_utf8_lossy(&format.fourcc()),
        frame.width,
        frame.height
    );
    let request = cam.find_format(format.fourcc(), frame.width, frame.height).unwrap();
    let stream = cam.start(&request).unwrap();
    eprintln!("committed {:?}", stream.control());
    let mut last = None;
    for _ in 0..5 {
        let f = stream.next_frame(Duration::from_secs(5)).unwrap();
        eprintln!("frame {} {} bytes error={}", f.sequence, f.data.len(), f.error);
        if let FormatKind::Uncompressed { bits_per_pixel, .. } = format.kind
            && !f.error
        {
            assert_eq!(
                f.data.len(),
                frame.width as usize * frame.height as usize * bits_per_pixel as usize / 8
            );
        }
        if let Some(prev) = last {
            assert!(f.sequence > prev);
        }
        last = Some(f.sequence);
    }
    drop(stream);
    // A second stream on the same camera works after the first stopped.
    let stream = cam.start(&request).unwrap();
    stream.next_frame(Duration::from_secs(5)).unwrap();
}

/// The "take the device, then run helpers on it" model: helpers borrow the
/// handle's claims, never share an interface, and leave the device taken.
#[cfg(feature = "hid")]
#[test]
fn hid_on_a_taken_device() {
    use rawusb::hid::{self, HidDevice};
    let Some((dev, _)) = env_device("RAWUSB_TEST_HID") else { return };
    let handle = dev.open().unwrap();
    handle.claim_all_interfaces().unwrap();
    let all: Vec<u8> = dev
        .active_config_descriptor()
        .unwrap()
        .interfaces
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(handle.claimed_interfaces().len(), all.len());
    let first = hid::interfaces(&dev).unwrap()[0].number;
    assert!(!handle.kernel_driver_active(first).unwrap());

    let a = HidDevice::open_interface(handle.clone(), first).unwrap();
    let err = HidDevice::open_interface(handle.clone(), first).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Busy, "{err}");
    drop(a);
    // The helper borrowed the claim: the device is still taken.
    assert!(handle.is_claimed(first));
    assert!(!handle.kernel_driver_active(first).unwrap());

    // All of them at once, then a second batch after the first is gone.
    let hids = HidDevice::open_all(&handle).unwrap();
    assert_eq!(hids.len(), hid::interfaces(&dev).unwrap().len());
    assert_eq!(HidDevice::open_all(&handle).unwrap_err().kind(), ErrorKind::Busy);
    drop(hids);
    let hids = HidDevice::open_all(&handle).unwrap();
    drop(hids);

    // Dropping the last handle gives the device back to the kernel.
    drop(handle);
    std::thread::sleep(Duration::from_millis(300));
    if cfg!(target_os = "linux") {
        let h = dev.open().unwrap();
        assert!(h.kernel_driver_active(first).unwrap());
    }
}

#[cfg(feature = "net")]
#[test]
fn net_send_and_receive() {
    use rawusb::net::{self, NetDevice};
    let Some((dev, _)) = env_device("RAWUSB_TEST_NET") else { return };
    eprintln!("functions: {:?}", net::interfaces(&dev).unwrap());
    let nic = NetDevice::open(&dev).unwrap();
    let mac = nic.mac_address();
    eprintln!(
        "{:?} {mac:02x?} max frame {} link {:?}",
        nic.kind(),
        nic.max_frame_size(),
        nic.link()
    );

    // Something always shows up on a live link (ARP, IPv6 RA, mDNS, ...).
    let f = nic.recv(Duration::from_secs(20)).expect("no frame within 20 s");
    eprintln!("received {} bytes, ethertype {:02x}{:02x}", f.len(), f[12], f[13]);

    let mut frame = vec![0xffu8; 6];
    frame.extend_from_slice(&mac);
    frame.extend_from_slice(&[0x88, 0xb5]);
    frame.extend_from_slice(b"rawusb net test");
    frame.resize(60, 0);
    for _ in 0..4 {
        nic.send(&frame).unwrap();
    }
    // Every frame size across a packet boundary goes through the padding
    // logic of the three framings.
    for len in [510, 511, 512, 513, 1024, nic.max_frame_size()] {
        let mut big = frame.clone();
        big.resize(len, 0x5a);
        nic.send(&big).unwrap();
    }
    std::thread::sleep(Duration::from_millis(500));
    let s = nic.stats();
    eprintln!("{s:?}");
    assert_eq!(s.tx_frames, 10);
    assert_eq!(s.tx_dropped, 0);
    assert!(nic.send(&frame[..10]).is_err(), "runt frames are refused");
    nic.close().unwrap();
    assert_eq!(nic.recv(Duration::from_millis(10)).unwrap_err().kind(), ErrorKind::NoDevice);
}

#[cfg(feature = "pktkit")]
#[test]
fn net_as_pktkit_device() {
    use pktkit::L2Device;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Usable wherever pktkit takes a device.
    fn takes_device(_: Arc<dyn L2Device>) {}
    let Some((dev, _)) = env_device("RAWUSB_TEST_NET") else { return };
    let nic = Arc::new(rawusb::net::NetDevice::open(&dev).unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    nic.set_handler(Arc::new(move |f: &pktkit::Frame| {
        assert!(f.src_mac().is_some());
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }));
    let start = std::time::Instant::now();
    while seen.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(seen.load(Ordering::Relaxed) > 0, "no frame within 20 s");
    assert_eq!(nic.hw_addr().0, nic.mac_address());
    assert!(L2Device::stats(&*nic).unwrap().snapshot().rx_packets > 0);
    takes_device(nic.clone());
    L2Device::close(&*nic).unwrap();
}
