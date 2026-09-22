//! Tests that talk to real devices. They skip themselves when nothing
//! suitable is attached, so they are safe to run anywhere (including CI
//! runners with no USB at all).
//!
//! Two of them need a device you nominate through the environment, because
//! they claim an interface and therefore detach the kernel driver:
//!
//! - `RAWUSB_TEST_MSC=vvvv:pppp` names an *unmounted* USB mass-storage
//!   device; the test sends a SCSI INQUIRY over the bulk-only transport.
//! - `RAWUSB_TEST_BULK_IN=vvvv:pppp:ee` names a device and a bulk IN
//!   endpoint on interface 0 that stays silent unless asked; the test checks
//!   timeouts, cancellation and the async completion future against it.

use rawusb::{Context, Device, DeviceHandle, ErrorKind, Transfer, TransferStatus};
use std::time::{Duration, Instant};

fn context() -> Option<Context> {
    match Context::new() {
        Ok(c) => Some(c),
        Err(e) if e.kind() == ErrorKind::NotSupported => None,
        Err(e) => panic!("Context::new failed: {e}"),
    }
}

fn devices(ctx: &Context) -> Vec<Device> {
    match ctx.devices() {
        Ok(d) => d,
        Err(e) if matches!(e.kind(), ErrorKind::Access | ErrorKind::NotFound | ErrorKind::NotSupported) => Vec::new(),
        Err(e) => panic!("enumeration failed: {e}"),
    }
}

fn parse_id(s: &str) -> (u16, u16) {
    let mut it = s.split(':');
    let v = u16::from_str_radix(it.next().unwrap(), 16).unwrap();
    let p = u16::from_str_radix(it.next().unwrap(), 16).unwrap();
    (v, p)
}

fn open_env_device(var: &str) -> Option<(DeviceHandle, String)> {
    let spec = std::env::var(var).ok()?;
    let (vid, pid) = parse_id(&spec);
    let ctx = context()?;
    let dev = ctx.find_device(vid, pid).unwrap()?;
    let h = dev.open().unwrap();
    Some((h, spec))
}

#[test]
fn enumerate_and_read_descriptors() {
    let Some(ctx) = context() else { return };
    let devs = devices(&ctx);
    for dev in &devs {
        let d = dev.device_descriptor();
        assert!(d.num_configurations >= 1, "{dev:?} has no configurations");
        assert!(d.max_packet_size_0 > 0);
        let cfg = dev.active_config_descriptor().unwrap();
        assert!(cfg.configuration_value >= 1);
        // Every endpoint must belong to a parsed interface, and the raw tree
        // must round-trip through the parser.
        for ep in cfg.all_endpoints() {
            assert!(ep.number() <= 15);
        }
        assert_eq!(rawusb::ConfigDescriptor::from_bytes(cfg.raw()).unwrap(), cfg);
        // Ports are consistent with the device's role.
        if dev.port_numbers().is_empty() {
            assert_eq!(dev.address(), 1, "only root hubs sit at address 1 with no ports");
        }
    }
}

#[test]
fn open_and_read_strings() {
    let Some(ctx) = context() else { return };
    let mut opened = 0;
    for dev in devices(&ctx) {
        let h = match dev.open() {
            Ok(h) => h,
            Err(e) if e.kind() == ErrorKind::Access => continue,
            Err(e) => panic!("open {dev:?}: {e}"),
        };
        opened += 1;
        let d = dev.device_descriptor();
        if d.product_string_index != 0 {
            let s = h.read_product_string().unwrap();
            assert!(s.is_some());
        }
        let langs = h.read_languages(Duration::from_secs(1));
        if let Ok(langs) = langs {
            assert!(!langs.is_empty());
        }
        let cfg = h.active_configuration().unwrap();
        let expected = dev.active_config_descriptor().unwrap().configuration_value;
        assert_eq!(cfg, expected, "{dev:?}");
        // Fetching the device descriptor over the wire must agree with sysfs.
        let mut buf = [0u8; 18];
        let n = h
            .read_descriptor(rawusb::types::descriptor_type::DEVICE, 0, 0, &mut buf, Duration::from_secs(1))
            .unwrap();
        assert_eq!(n, 18);
        let wire = rawusb::DeviceDescriptor::from_bytes(&buf).unwrap();
        assert_eq!(wire.vendor_id, d.vendor_id);
        assert_eq!(wire.product_id, d.product_id);
        // A bogus string index must come back as a stall, not hang.
        let err = h.read_string_descriptor(0x0409, 250, Duration::from_millis(500));
        if let Err(e) = err {
            assert!(matches!(e.kind(), ErrorKind::Pipe | ErrorKind::Io | ErrorKind::Timeout), "{e:?}");
        }
    }
    eprintln!("opened {opened} devices");
}

#[test]
fn concurrent_control_transfers() {
    let Some(ctx) = context() else { return };
    let Some(dev) = devices(&ctx)
        .into_iter()
        .find(|d| d.open().is_ok() && d.device_descriptor().product_string_index != 0)
    else {
        return;
    };
    let h = dev.open().unwrap();
    let expected = h.read_product_string().unwrap().unwrap();
    // Several threads hammering endpoint 0 through the same handle.
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let h = h.clone();
            let expected = expected.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    assert_eq!(h.read_product_string().unwrap().unwrap(), expected);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    // And a batch of transfers submitted together, then awaited.
    let idx = dev.device_descriptor().product_string_index;
    let setup = rawusb::ControlSetup::new(
        rawusb::Direction::In,
        rawusb::ControlType::Standard,
        rawusb::Recipient::Device,
        rawusb::types::request::GET_DESCRIPTOR,
        (rawusb::types::descriptor_type::STRING as u16) << 8 | idx as u16,
        0x0409,
        255,
    );
    let transfers: Vec<Transfer> = (0..8).map(|_| Transfer::control(&h, setup, &[])).collect();
    for t in &transfers {
        t.set_timeout(Duration::from_secs(2)).unwrap();
        t.submit().unwrap();
        assert!(t.submit().is_err(), "double submit must fail");
    }
    for t in &transfers {
        assert_eq!(t.wait(Some(Duration::from_secs(5))).unwrap(), TransferStatus::Completed);
        let data = t.data().unwrap();
        assert_eq!(rawusb::descriptors::decode_string_descriptor(&data).unwrap(), expected);
    }
}

#[test]
fn callback_and_future() {
    let Some(ctx) = context() else { return };
    let Some(dev) = devices(&ctx).into_iter().find(|d| d.open().is_ok()) else {
        return;
    };
    let h = dev.open().unwrap();
    let setup = rawusb::ControlSetup::new(
        rawusb::Direction::In,
        rawusb::ControlType::Standard,
        rawusb::Recipient::Device,
        rawusb::types::request::GET_DESCRIPTOR,
        (rawusb::types::descriptor_type::DEVICE as u16) << 8,
        0,
        18,
    );
    let t = Transfer::control(&h, setup, &[]);
    let (tx, rx) = std::sync::mpsc::channel();
    t.set_callback(move |t| {
        tx.send((t.status(), t.actual_length())).unwrap();
    })
    .unwrap();
    t.submit().unwrap();
    let (status, len) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(status, TransferStatus::Completed);
    assert_eq!(len, 18);

    // The same transfer, resubmitted and awaited through the future.
    let status = block_on(t.completion());
    assert_eq!(status, TransferStatus::Completed, "not in flight: resolves immediately");
    t.submit().unwrap();
    let status = block_on(t.completion());
    assert_eq!(status, TransferStatus::Completed);
    assert_eq!(t.data().unwrap().len(), 18);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

/// Minimal executor: enough to drive one future to completion.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::{Arc, Condvar, Mutex};
    use std::task::{Context, Poll, Wake, Waker};
    struct Signal(Mutex<bool>, Condvar);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            *self.0.lock().unwrap() = true;
            self.1.notify_one();
        }
    }
    let signal = Arc::new(Signal(Mutex::new(false), Condvar::new()));
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        let mut ready = signal.0.lock().unwrap();
        while !*ready {
            ready = signal.1.wait_timeout(ready, Duration::from_secs(10)).unwrap().0;
        }
        *ready = false;
    }
}

#[test]
fn bulk_in_timeout_cancel_and_future() {
    let Some((h, spec)) = open_env_device("RAWUSB_TEST_BULK_IN") else {
        return;
    };
    let ep = u8::from_str_radix(spec.rsplit(':').next().unwrap(), 16).unwrap();
    h.set_auto_detach_kernel_driver(true);
    h.claim_interface(0).unwrap();
    assert_eq!(h.claimed_interfaces(), vec![0]);

    // Synchronous read: nothing arrives, so the timeout fires.
    let mut buf = [0u8; 512];
    let start = Instant::now();
    let err = h.bulk_read(ep, &mut buf, Duration::from_millis(200)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Timeout, "{err}");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(190) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );

    // Async: timeout status.
    let t = Transfer::bulk(&h, ep, vec![0u8; 512]);
    t.set_timeout(Duration::from_millis(100)).unwrap();
    t.submit().unwrap();
    assert!(t.is_pending());
    assert!(t.buffer().is_err(), "buffer is locked while in flight");
    assert_eq!(t.wait(None).unwrap(), TransferStatus::TimedOut);
    assert!(!t.is_pending());

    // Async: explicit cancel, no timeout.
    t.set_timeout(Duration::ZERO).unwrap();
    t.submit().unwrap();
    assert_eq!(t.wait(Some(Duration::from_millis(100))).unwrap_err().kind(), ErrorKind::Timeout);
    t.cancel().unwrap();
    assert_eq!(t.wait(Some(Duration::from_secs(2))).unwrap(), TransferStatus::Cancelled);
    assert_eq!(t.cancel().unwrap_err().kind(), ErrorKind::NotFound);

    // Async: awaited future that resolves on cancel from another thread.
    t.submit().unwrap();
    let t2 = t.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        t2.cancel().unwrap();
    });
    assert_eq!(block_on(t.completion()), TransferStatus::Cancelled);
    canceller.join().unwrap();

    // Large multi-URB read on an old-kernel code path is not testable here,
    // but a big buffer must at least be accepted and time out cleanly.
    let big = Transfer::bulk(&h, ep, vec![0u8; 1 << 20]);
    big.set_timeout(Duration::from_millis(100)).unwrap();
    big.submit().unwrap();
    assert_eq!(big.wait(None).unwrap(), TransferStatus::TimedOut);
    assert_eq!(big.actual_length(), 0);

    // Dropping the handle with a transfer in flight cancels it.
    let t = Transfer::bulk(&h, ep, vec![0u8; 64]);
    t.submit().unwrap();
    drop(h);
    assert!(t.handle().is_none());
    assert_eq!(t.wait(Some(Duration::from_secs(2))).unwrap(), TransferStatus::Cancelled);
}

#[test]
fn mass_storage_inquiry() {
    let Some((h, _)) = open_env_device("RAWUSB_TEST_MSC") else { return };
    let cfg = h.device().active_config_descriptor().unwrap();
    let iface = cfg
        .all_alt_settings()
        .find(|a| a.class == rawusb::types::class::MASS_STORAGE && a.protocol == 0x50)
        .expect("bulk-only mass storage interface");
    let ep_in = iface
        .endpoints
        .iter()
        .find(|e| e.direction() == rawusb::Direction::In)
        .unwrap()
        .address;
    let ep_out = iface
        .endpoints
        .iter()
        .find(|e| e.direction() == rawusb::Direction::Out)
        .unwrap()
        .address;

    h.set_auto_detach_kernel_driver(true);
    let had_driver = h.kernel_driver_active(iface.number).unwrap();
    h.claim_interface(iface.number).unwrap();
    assert!(!h.kernel_driver_active(iface.number).unwrap());

    // Bulk-only transport: CBW, data, CSW.
    let tag = 0x12345678u32;
    let mut cbw = [0u8; 31];
    cbw[..4].copy_from_slice(b"USBC");
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&36u32.to_le_bytes());
    cbw[12] = 0x80; // data in
    cbw[13] = 0; // LUN
    cbw[14] = 6; // CDB length
    cbw[15] = 0x12; // INQUIRY
    cbw[19] = 36; // allocation length
    let to = Duration::from_secs(2);
    assert_eq!(h.bulk_write(ep_out, &cbw, to).unwrap(), 31);
    let mut data = [0u8; 36];
    let n = h.bulk_read(ep_in, &mut data, to).unwrap();
    assert_eq!(n, 36);
    let mut csw = [0u8; 13];
    assert_eq!(h.bulk_read(ep_in, &mut csw, to).unwrap(), 13);
    assert_eq!(&csw[..4], b"USBS");
    assert_eq!(u32::from_le_bytes(csw[4..8].try_into().unwrap()), tag);
    assert_eq!(csw[12], 0, "command status");
    let vendor = String::from_utf8_lossy(&data[8..16]).replace('\0', " ");
    let product = String::from_utf8_lossy(&data[16..32]).replace('\0', " ");
    eprintln!("INQUIRY: {} {}", vendor.trim(), product.trim());
    assert!(vendor.trim().chars().all(|c| c.is_ascii_graphic() || c == ' '));
    assert_eq!(data[0] & 0x1f, 0, "peripheral device type: direct access");

    // Short read: ask for more than the device will send.
    assert_eq!(h.bulk_write(ep_out, &cbw, to).unwrap(), 31);
    let mut big = vec![0u8; 4096];
    assert_eq!(h.bulk_read(ep_in, &mut big, to).unwrap(), 36);
    let _ = h.bulk_read(ep_in, &mut csw, to).unwrap();

    h.release_interface(iface.number).unwrap();
    assert!(h.claimed_interfaces().is_empty());
    if had_driver {
        // release re-attached the kernel driver
        std::thread::sleep(Duration::from_millis(200));
        assert!(h.kernel_driver_active(iface.number).unwrap());
    }
}
