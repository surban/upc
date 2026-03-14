//! USB loopback integration test.
//!
//! This test requires a USB Device Controller (UDC) connected back to a USB
//! host port on the same machine via a loopback cable.
//!
//! Run with:
//!   cargo test --test loopback --features host,device -- --nocapture

#![cfg(all(feature = "host", feature = "device"))]

mod util;

use bytes::Bytes;
use rand::{prelude::*, rngs::SmallRng};
use serial_test::serial;
use std::time::Duration;
use tokio::{
    sync::oneshot,
    time::{sleep, Instant},
};
use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};
use util::*;
use uuid::uuid;

use upc::{
    device::{InterfaceId, UpcFunction},
    host::{connect, connect_with, find_interface, info, UpcOptions},
    TRANSFER_PACKETS,
};

// ── Constants ────────────────────────────────────────────────────────────────

const TOPIC: &[u8] = b"LOOPBACK TEST TOPIC";
const INFO: &[u8] = b"LOOPBACK TEST INFO";

const HOST_SEED: u64 = 77701;
const DEVICE_SEED: u64 = 88802;
const TEST_PACKETS: usize = 2000;
const TEST_PACKET_MAX_SIZE: usize = 1_500_000;

const GUID: uuid::Uuid = uuid!("3bf77270-42d2-42c6-a475-490227a9cc89");

// High-speed bulk max packet size.
const MPS: usize = 512;
const BIG: usize = MPS * TRANSFER_PACKETS;

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
                0,
                1,
                2,
                3,
                511,
                512,
                513,
                1023,
                1024,
                1025,
                0,
                2000,
                2048,
                0,
                4096,
                5000,
                8191,
                8192,
                8193,
                0,
                8193,
                // Around MPS * (TRANSFER_PACKETS - 1)
                BIG - MPS - 1,
                BIG - MPS,
                BIG - MPS + 1,
                // Around MPS * TRANSFER_PACKETS
                BIG - 1,
                BIG,
                BIG + 1,
                // Around MPS * (TRANSFER_PACKETS + 1)
                BIG + MPS - 1,
                BIG + MPS,
                BIG + MPS + 1,
                0,
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

    #[track_caller]
    fn validate(&mut self, data: &[u8]) {
        let expected = self.generate();
        assert_eq!(data.len(), expected.len(), "data length mismatch");
        assert_eq!(data, &expected[..], "data content mismatch");
    }
}

// ── The actual loopback test ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
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
    let (dev_rx_done_tx, dev_rx_done_rx) = oneshot::channel::<(usize, f64)>();

    let device_task = tokio::spawn(async move {
        println!("[device] Waiting for connection…");
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("device accept failed");
        assert_eq!(dev_rx.topic(), TOPIC, "wrong topic on device side");

        // Receiver task on device side: receives what the host sent.
        let dev_recv_task = tokio::spawn(async move {
            let rx_done_tx = dev_rx_done_tx;
            let mut rx_td = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);

            let start = Instant::now();
            let mut total = 0usize;

            println!("[device-rx] Receiving…");
            for n in 0..TEST_PACKETS {
                let data = dev_rx.recv().await.expect("device recv failed").expect("unexpected EOF");
                if n % 50 == 0 {
                    println!("[device-rx] packet {n}: {} bytes", data.len());
                }
                total += data.len();
                rx_td.validate(&data);
            }

            let elapsed = start.elapsed().as_secs_f64();
            let mb_s = total as f64 / elapsed / 1_048_576.;
            println!("[device-rx] Received {total} bytes in {elapsed:.2}s ({mb_s:.2} MB/s)");

            let _ = rx_done_tx.send((total, elapsed));

            // Wait for sender to close.
            assert_eq!(dev_rx.recv().await.unwrap(), None, "device receiver not closed");
            println!("[device-rx] Receiver closed");
        });

        // Sender on device side: sends data for the host to receive.
        let mut tx_td = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);

        println!("[device-tx] Sending…");
        for n in 0..TEST_PACKETS {
            let data = tx_td.generate();
            let len = data.len();
            dev_tx.send(Bytes::from(data)).await.expect("device send failed");
            if n % 50 == 0 {
                println!("[device-tx] packet {n}: {len} bytes");
            }
        }

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

    // Wall-clock start for the entire data transfer phase (after connection setup).
    let transfer_start = Instant::now();

    // Channel so we know the host receiver is done with all data packets.
    let (host_rx_done_tx, host_rx_done_rx) = oneshot::channel::<(usize, f64)>();

    // Host receiver task: receives what the device sent.
    let host_recv_task = tokio::spawn(async move {
        let rx_done_tx = host_rx_done_tx;
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

        let elapsed = start.elapsed().as_secs_f64();
        let mb_s = total as f64 / elapsed / 1_048_576.;
        println!("[host-rx] Received {total} bytes in {elapsed:.2}s ({mb_s:.2} MB/s)");

        rx_done_tx.send((total, elapsed)).unwrap();

        // Wait for receiver to close.
        assert_eq!(host_rx.recv().await.unwrap(), None, "host receiver not closed");
        println!("[host-rx] Receiver closed");
    });

    // Host sender: sends data for the device to receive.
    let mut tx_td = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);

    println!("[host-tx] Sending…");
    for n in 0..TEST_PACKETS {
        let data = tx_td.generate();
        let len = data.len();
        host_tx.send(Bytes::from(data)).await.expect("host send failed");
        if n % 50 == 0 {
            println!("[host-tx] packet {n}: {len} bytes");
        }
    }

    // Wait for both receivers to finish validating all packets and
    // capture wall-clock time before cleanup sleeps inflate it.
    let (host_rx_bytes, host_rx_elapsed) = host_rx_done_rx.await.unwrap();
    let (dev_rx_bytes, dev_rx_elapsed) = dev_rx_done_rx.await.unwrap();
    let total_elapsed = transfer_start.elapsed().as_secs_f64();
    let total_bytes = host_rx_bytes + dev_rx_bytes;

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

    let host_rx_mb_s = host_rx_bytes as f64 / host_rx_elapsed / 1_048_576.;
    let dev_rx_mb_s = dev_rx_bytes as f64 / dev_rx_elapsed / 1_048_576.;
    let combined_mb_s = total_bytes as f64 / total_elapsed / 1_048_576.;

    println!();
    println!("device_to_host: {:.2} MB/s ({} bytes in {:.2} s)", host_rx_mb_s, host_rx_bytes, host_rx_elapsed);
    println!("host_to_device: {:.2} MB/s ({} bytes in {:.2} s)", dev_rx_mb_s, dev_rx_bytes, dev_rx_elapsed);
    println!("combined:       {:.2} MB/s ({} bytes in {:.2} s)", combined_mb_s, total_bytes, total_elapsed);
    println!();

    sleep(Duration::from_secs(1)).await;
    println!("[loopback] ✅ Loopback test passed!");
}

// ── Max packet size exchange test ────────────────────────────────────────────

const DEVICE_MAX_SIZE: u64 = 1_000_000;
const HOST_MAX_SIZE: usize = 2_000_000;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn max_packet_size_exchange() {
    init_log();

    // ------------------------------------------------------------------
    // 1. Set up the device (gadget) side with a custom max_size
    // ------------------------------------------------------------------
    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("[max_size] Creating UPC function…");
    let (mut upc_fn, hnd) =
        UpcFunction::new(InterfaceId::new(CLASS).with_name("USB MAX SIZE TEST").with_guid(GUID));

    // Set custom device max packet size.
    upc_fn.set_max_size(DEVICE_MAX_SIZE).await;
    assert_eq!(upc_fn.max_size().await, DEVICE_MAX_SIZE);

    println!("[max_size] Registering gadget…");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget =
        Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("upc-maxsize", "test", "0"))
            .with_config(Config::new("config").with_function(hnd))
            .with_os_descriptor(OsDescriptor::microsoft());
    gadget.device_release = 0x0110;
    let reg = gadget.bind(&udc).expect("cannot bind to UDC");
    assert!(reg.is_attached(), "gadget is not attached");

    println!("[max_size] Waiting for host enumeration…");
    sleep(Duration::from_secs(3)).await;

    // ------------------------------------------------------------------
    // 2. Device side: accept and verify max_size values
    // ------------------------------------------------------------------
    let device_task = tokio::spawn(async move {
        println!("[device] Waiting for connection…");
        let (dev_tx, dev_rx) = upc_fn.accept().await.expect("device accept failed");

        // Device sender should have the host's max_size.
        println!("[device] dev_tx.max_size() = {}", dev_tx.max_size());
        assert_eq!(dev_tx.max_size(), HOST_MAX_SIZE, "device sender should have host's max_size");

        // Device receiver should have the device's own max_size.
        println!("[device] dev_rx.max_size() = {}", dev_rx.max_size());
        assert_eq!(dev_rx.max_size(), DEVICE_MAX_SIZE as usize, "device receiver should have device's max_size");

        // Sending a packet exceeding host's max_size should fail.
        let big_packet = Bytes::from(vec![0u8; HOST_MAX_SIZE + 1]);
        let res = dev_tx.send(big_packet).await;
        assert!(res.is_err(), "device send exceeding host max_size should fail");
        println!("[device] Oversized send correctly rejected: {}", res.unwrap_err());

        // Send a small packet to confirm the connection works.
        dev_tx.send(Bytes::from_static(b"hello from device")).await.expect("device send failed");
        println!("[device] Sent test packet");

        // Keep alive.
        (reg, upc_fn)
    });

    // ------------------------------------------------------------------
    // 3. Host side: connect with custom max_size, verify
    // ------------------------------------------------------------------
    println!("[host] Enumerating USB devices…");
    let dev_info = {
        let mut found = None;
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
        found.expect("device not found on USB bus")
    };

    let iface_num = find_interface(&dev_info, CLASS).expect("cannot find interface");
    let dev = dev_info.open().await.expect("cannot open device");

    println!("[host] Connecting with max_size={HOST_MAX_SIZE}…");
    let options = UpcOptions::new().with_topic(b"maxsize".to_vec()).with_max_size(HOST_MAX_SIZE);
    let (host_tx, mut host_rx) = connect_with(dev, iface_num, options).await.expect("host connect failed");

    // Host sender should have the device's max_size.
    println!("[host] host_tx.max_size() = {}", host_tx.max_size());
    assert_eq!(host_tx.max_size(), DEVICE_MAX_SIZE as usize, "host sender should have device's max_size");

    // Host receiver should have the host's own max_size.
    println!("[host] host_rx.max_size() = {}", host_rx.max_size());
    assert_eq!(host_rx.max_size(), HOST_MAX_SIZE, "host receiver should have host's max_size");

    // Sending a packet exceeding device's max_size should fail.
    let big_packet = Bytes::from(vec![0u8; DEVICE_MAX_SIZE as usize + 1]);
    let res = host_tx.send(big_packet).await;
    assert!(res.is_err(), "host send exceeding device max_size should fail");
    println!("[host] Oversized send correctly rejected: {}", res.unwrap_err());

    // Receive the test packet from device.
    let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
    assert_eq!(&data[..], b"hello from device");
    println!("[host] Received test packet");

    // ------------------------------------------------------------------
    // 4. Clean up
    // ------------------------------------------------------------------
    drop(host_tx);
    drop(host_rx);
    sleep(Duration::from_secs(1)).await;

    let (_reg, _upc_fn) = device_task.await.unwrap();
    sleep(Duration::from_secs(1)).await;
    println!("[max_size] ✅ Max packet size exchange test passed!");
}
