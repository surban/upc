//! USB loopback integration test.
//!
//! This test requires a USB Device Controller (UDC) connected back to a USB
//! host port on the same machine via a loopback cable.
//!
//! Run with:
//!   cargo test --test loopback --features host,device -- --nocapture

#![cfg(all(feature = "host", feature = "device"))]

use std::time::{Duration, Instant};
use bytes::Bytes;
use rand::{prelude::*, rngs::SmallRng};
use tokio::{sync::oneshot, time::sleep};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};
use uuid::uuid;
use serial_test::serial;

use upc::{
    device::{InterfaceId, UpcFunction},
    host::{connect, find_interface, info},
    Class,
};

// ── Constants ────────────────────────────────────────────────────────────────

const VID: u16 = 4;
const PID: u16 = 5;

const CLASS: Class = Class::vendor_specific(22, 3);
const DEVICE_CLASS: Class = Class::vendor_specific(0xff, 0);
const TOPIC: &[u8] = b"LOOPBACK TEST TOPIC";
const INFO: &[u8] = b"LOOPBACK TEST INFO";

const HOST_SEED: u64 = 77701;
const DEVICE_SEED: u64 = 88802;
const TEST_PACKETS: usize = 200;
const TEST_PACKET_MAX_SIZE: usize = 500_000;

const GUID: uuid::Uuid = uuid!("3bf77270-42d2-42c6-a475-490227a9cc89");

// ── Test-data helpers ────────────────────────────────────────────────────────

struct TestData {
    rng: SmallRng,
    max_length: usize,
    pre_lengths: std::collections::VecDeque<usize>,
}

impl TestData {
    fn new(seed: u64, max_length: usize) -> Self {
        Self {
            rng: SmallRng::seed_from_u64(seed),
            max_length,
            pre_lengths: [
                0, 1, 2, 3, 511, 512, 513, 1023, 1024, 1025, 0, 2000, 2048, 0, 4096, 5000, 8191, 8192, 8193, 0,
                8193,
            ]
            .into(),
        }
    }

    fn generate(&mut self) -> Vec<u8> {
        let len = match self.pre_lengths.pop_front() {
            Some(len) => len,
            None => self.rng.random_range(0..self.max_length),
        };
        let mut data = vec![0u8; len];
        self.rng.fill_bytes(&mut data);
        data
    }

    fn validate(&mut self, data: &[u8]) {
        let expected = self.generate();
        assert_eq!(data.len(), expected.len(), "data length mismatch");
        assert_eq!(data, &expected[..], "data content mismatch");
    }
}

// ── Logging initializer ─────────────────────────────────────────────────────

fn init_log() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
        tracing_log::LogTracer::init().unwrap();
    });
}

// ── The actual loopback test ─────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn loopback() {
    init_log();

    // ------------------------------------------------------------------
    // 1. Set up the device (gadget) side
    // ------------------------------------------------------------------
    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("[loopback] Creating UPC function (device side)…");
    let (mut upc_fn, hnd) =
        UpcFunction::new(InterfaceId::new(CLASS).with_name("USB LOOPBACK TEST").with_guid(GUID));
    upc_fn.set_info(INFO.to_vec()).await;

    println!("[loopback] Registering gadget…");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget =
        Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("upc-loopback", "test", "0"))
            .with_config(Config::new("config").with_function(hnd))
            .with_os_descriptor(OsDescriptor::microsoft());
    gadget.device_release = 0x0110;
    let reg = gadget.bind(&udc).expect("cannot bind to UDC");
    assert!(reg.is_attached(), "gadget is not attached");

    // Give the host time to enumerate the new device.
    println!("[loopback] Waiting for host enumeration…");
    sleep(Duration::from_secs(3)).await;

    // ------------------------------------------------------------------
    // 2. Spawn the device-side handler (accept + send/recv)
    // ------------------------------------------------------------------
    let device_task = tokio::spawn(async move {
        println!("[device] Waiting for connection…");
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("device accept failed");
        assert_eq!(dev_rx.topic(), TOPIC, "wrong topic on device side");

        // Receiver task on device side: receives what the host sent.
        let dev_recv_task = tokio::spawn(async move {
            let mut rx_td = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);

            println!("[device-rx] Receiving…");
            for n in 0..TEST_PACKETS {
                let data = dev_rx.recv().await.expect("device recv failed").expect("unexpected EOF");
                if n % 50 == 0 {
                    println!("[device-rx] packet {n}: {} bytes", data.len());
                }
                rx_td.validate(&data);
            }
            println!("[device-rx] All {} packets received and validated", TEST_PACKETS);

            // Wait for sender to close.
            assert_eq!(dev_rx.recv().await.unwrap(), None, "device receiver not closed");
            println!("[device-rx] Receiver closed");
        });

        // Sender on device side: sends data for the host to receive.
        let mut tx_td = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);

        let start = Instant::now();
        let mut total = 0usize;

        println!("[device-tx] Sending…");
        for n in 0..TEST_PACKETS {
            let data = tx_td.generate();
            let len = data.len();
            dev_tx.send(Bytes::from(data)).await.expect("device send failed");
            total += len;
            if n % 50 == 0 {
                println!("[device-tx] packet {n}: {len} bytes");
            }
        }

        let elapsed = start.elapsed().as_secs_f32();
        println!(
            "[device-tx] Sent {total} bytes in {elapsed:.2}s ({:.2} MB/s)",
            total as f32 / elapsed / 1_048_576.
        );

        // Wait a bit so the host can finish receiving, then wait for the
        // device receiver to confirm it got everything.
        sleep(Duration::from_secs(3)).await;
        println!("[device] Waiting for device receiver to finish…");
        dev_recv_task.await.unwrap();
        drop(dev_tx);
        println!("[device] Done");

        // Keep registration and upc_fn alive so the gadget stays connected
        // and the device task can still respond on the endpoints.
        (reg, upc_fn)
    });

    // ------------------------------------------------------------------
    // 3. Host side: find the device, connect, send/recv
    // ------------------------------------------------------------------
    println!("[host] Enumerating USB devices…");
    let dev_info = {
        let mut found = None;
        // Retry a few times in case enumeration is slow.
        for attempt in 0..10 {
            if let Some(di) = nusb::list_devices()
                .await
                .expect("cannot enumerate USB devices")
                .find(|c| c.vendor_id() == VID && c.product_id() == PID)
            {
                found = Some(di);
                break;
            }
            println!("[host] Device not found yet (attempt {attempt}), retrying…");
            sleep(Duration::from_secs(1)).await;
        }
        found.expect("loopback device not found on USB bus")
    };
    println!("[host] Found device: {dev_info:?}");

    println!("[host] Finding interface…");
    let iface_num = find_interface(&dev_info, CLASS).expect("cannot find interface");
    println!("[host] Using interface {iface_num}");

    println!("[host] Getting info…");
    let dev = dev_info.open().await.expect("cannot open device");
    let dev_info_data = info(&dev, iface_num).await.expect("cannot get info");
    println!("[host] Info: {}", String::from_utf8_lossy(&dev_info_data));
    assert_eq!(dev_info_data, INFO, "info mismatch");

    println!("[host] Connecting…");
    let (host_tx, mut host_rx) = connect(dev, iface_num, TOPIC).await.expect("host connect failed");

    // Channel so we know the host receiver is done with all data packets
    // before we drop the sender.
    let (rx_done_tx, rx_done_rx) = oneshot::channel::<()>();

    // Host receiver task: receives what the device sent.
    let host_recv_task = tokio::spawn(async move {
        let mut rx_td = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);

        let start = Instant::now();
        let mut total = 0usize;

        println!("[host-rx] Receiving…");
        for n in 0..TEST_PACKETS {
            let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
            if n % 50 == 0 {
                println!("[host-rx] packet {n}: {} bytes", data.len());
            }
            total += data.len();
            rx_td.validate(&data);
        }

        let elapsed = start.elapsed().as_secs_f32();
        println!(
            "[host-rx] Received {total} bytes in {elapsed:.2}s ({:.2} MB/s)",
            total as f32 / elapsed / 1_048_576.
        );

        rx_done_tx.send(()).unwrap();

        // Wait for receiver to close.
        assert_eq!(host_rx.recv().await.unwrap(), None, "host receiver not closed");
        println!("[host-rx] Receiver closed");
    });

    // Host sender: sends data for the device to receive.
    let mut tx_td = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);

    let start = Instant::now();
    let mut total = 0usize;

    println!("[host-tx] Sending…");
    for n in 0..TEST_PACKETS {
        let data = tx_td.generate();
        let len = data.len();
        host_tx.send(Bytes::from(data)).await.expect("host send failed");
        total += len;
        if n % 50 == 0 {
            println!("[host-tx] packet {n}: {len} bytes");
        }
    }

    let elapsed = start.elapsed().as_secs_f32();
    println!("[host-tx] Sent {total} bytes in {elapsed:.2}s ({:.2} MB/s)", total as f32 / elapsed / 1_048_576.);

    // Wait for host receiver to finish validating all packets.
    rx_done_rx.await.unwrap();
    sleep(Duration::from_secs(3)).await;

    println!("[host] Disconnecting…");
    drop(host_tx);
    host_recv_task.await.unwrap();
    println!("[host] Disconnected");

    // ------------------------------------------------------------------
    // 4. Wait for the device side to finish
    // ------------------------------------------------------------------
    println!("[loopback] Waiting for device task…");
    let (_reg, _upc_fn) = device_task.await.unwrap();
    // Keep `_reg` and `_upc_fn` alive until here so the gadget stays
    // registered and the device task can process the clean shutdown.

    sleep(Duration::from_secs(1)).await;
    println!("[loopback] ✅ Loopback test passed!");
}
