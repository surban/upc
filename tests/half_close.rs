//! Half-close integration tests.
//!
//! These tests verify that each direction of a UPC channel can be closed
//! independently (half-close) without disturbing the other direction.
//! Each test runs multiple rounds to verify reconnection works correctly.
//!
//! Run with:
//!   cargo test --test half_close --features host,device -- --nocapture --test-threads=1

#![cfg(all(feature = "host", feature = "device"))]

use std::time::Duration;
use bytes::Bytes;
use tokio::time::{sleep, timeout};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};
use serial_test::serial;

use upc::{
    device::{InterfaceId, UpcFunction},
    host::{connect, find_interface},
    Class,
};

// ── Constants ────────────────────────────────────────────────────────────────

const VID: u16 = 4;
const PID: u16 = 5;
const ROUNDS: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

const CLASS: Class = Class::vendor_specific(22, 3);
const DEVICE_CLASS: Class = Class::vendor_specific(0xff, 0);

// ── Logging initializer ─────────────────────────────────────────────────────

fn init_log() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
        tracing_log::LogTracer::init().unwrap();
    });
}

// ── Helpers: set up gadget, then connect per round ───────────────────────────

struct TestSetup {
    _reg: Box<dyn std::any::Any + Send>,
}

async fn setup_gadget(test_name: &str) -> (UpcFunction, nusb::DeviceInfo, u8, TestSetup) {
    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("[{test_name}] Creating UPC function…");
    let (upc_fn, hnd) = UpcFunction::new(InterfaceId::new(CLASS).with_name("HALF-CLOSE TEST"));

    println!("[{test_name}] Registering gadget…");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget =
        Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("upc-halfclose", "test", "0"))
            .with_config(Config::new("config").with_function(hnd))
            .with_os_descriptor(OsDescriptor::microsoft());
    gadget.device_release = 0x0110;
    let reg = gadget.bind(&udc).expect("cannot bind to UDC");
    assert!(reg.is_attached(), "gadget is not attached");

    println!("[{test_name}] Waiting for host enumeration…");
    sleep(Duration::from_secs(3)).await;

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
            println!("[{test_name}] Device not found yet (attempt {attempt}), retrying…");
            sleep(Duration::from_secs(1)).await;
        }
        found.expect("device not found on USB bus")
    };

    println!("[{test_name}] Finding interface…");
    let iface_num = find_interface(&dev_info, CLASS).expect("cannot find interface");

    (upc_fn, dev_info, iface_num, TestSetup { _reg: Box::new(reg) })
}

async fn host_connect(
    dev_info: &nusb::DeviceInfo, iface_num: u8,
) -> (upc::host::UpcSender, upc::host::UpcReceiver) {
    let dev = dev_info.open().await.expect("cannot open device");
    connect(dev, iface_num, b"halfclose").await.expect("connect failed")
}

// ── Test 1: Host drops sender, device can still send ────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn host_close_send() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup_gadget("host_close_send").await;

    for round in 0..ROUNDS {
        println!("\n[host_close_send] ── Round {round} ──");
        let (host_tx, mut host_rx) = host_connect(&dev_info, iface_num).await;
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            println!("[device] Waiting for recv to close…");
            let res = dev_rx.recv().await;
            println!("[device] Recv result after host close send: {res:?}");
            assert_eq!(res.unwrap(), None, "device recv should get EOF after host drops sender");

            println!("[device] Sending data to host…");
            for i in 0u32..10 {
                dev_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("device send should succeed");
            }
            println!("[device] Done sending");
            sleep(Duration::from_secs(1)).await;
            drop(dev_tx);
        });

        println!("[host] Dropping sender…");
        drop(host_tx);
        sleep(Duration::from_secs(1)).await;

        println!("[host] Receiving data…");
        for i in 0u32..10 {
            let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
            let expected = i.to_le_bytes();
            assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
        }
        println!("[host-rx] All packets received");

        let res = host_rx.recv().await;
        println!("[host] Recv after device close: {res:?}");
        assert_eq!(res.unwrap(), None, "host recv should get EOF after device drops sender");

        device_task.await.unwrap();
    }

    println!("[host_close_send] PASSED");
}

// ── Test 2: Host drops receiver, device can still receive ───────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn host_close_recv() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup_gadget("host_close_recv").await;

    for round in 0..ROUNDS {
        println!("\n[host_close_recv] ── Round {round} ──");
        let (host_tx, host_rx) = host_connect(&dev_info, iface_num).await;
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            println!("[device] Receiving data from host…");
            for i in 0u32..10 {
                let data = dev_rx.recv().await.expect("device recv failed").expect("unexpected EOF");
                let expected = i.to_le_bytes();
                assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
            }
            println!("[device-rx] All packets received");

            let res = dev_rx.recv().await;
            println!("[device] Recv result after host close: {res:?}");
            assert_eq!(res.unwrap(), None, "device recv should get EOF after host drops sender");

            println!("[device] Trying to send after host closed recv…");
            sleep(Duration::from_secs(1)).await;
            let res = dev_tx.send(Bytes::from_static(b"should fail")).await;
            println!("[device] Send result: {res:?}");
            assert!(res.is_err(), "device send should fail after host drops receiver");
        });

        println!("[host] Dropping receiver…");
        drop(host_rx);
        sleep(Duration::from_secs(1)).await;

        println!("[host] Sending data…");
        for i in 0u32..10 {
            host_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("host send should succeed");
        }
        println!("[host-tx] All packets sent");

        sleep(Duration::from_secs(1)).await;
        println!("[host] Dropping sender…");
        drop(host_tx);

        device_task.await.unwrap();
    }

    println!("[host_close_recv] PASSED");
}

// ── Test 3: Device drops sender, host recv gets EOF ─────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn device_close_send() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup_gadget("device_close_send").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_send] ── Round {round} ──");
        let (host_tx, mut host_rx) = host_connect(&dev_info, iface_num).await;
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            println!("[device] Sending data and closing sender…");
            for i in 0u32..10 {
                dev_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("device send should succeed");
            }
            sleep(Duration::from_secs(1)).await;
            println!("[device] Dropping sender…");
            drop(dev_tx);

            println!("[device] Receiving data from host…");
            for i in 0u32..10 {
                let data = dev_rx.recv().await.expect("device recv failed").expect("unexpected EOF");
                let expected = i.to_le_bytes();
                assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
            }
            println!("[device-rx] All packets received");
        });

        println!("[host] Receiving data…");
        for i in 0u32..10 {
            let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
            let expected = i.to_le_bytes();
            assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
        }
        println!("[host-rx] All packets received");

        let res = host_rx.recv().await;
        println!("[host] Recv after device close send: {res:?}");
        assert_eq!(res.unwrap(), None, "host recv should get EOF after device drops sender");

        println!("[host] Sending data to device…");
        sleep(Duration::from_secs(1)).await;
        for i in 0u32..10 {
            host_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("host send should succeed");
        }
        println!("[host-tx] All packets sent");

        device_task.await.unwrap();
    }

    println!("[device_close_send] PASSED");
}

// ── Test 4: Device drops receiver, host send gets error ─────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn device_close_recv() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup_gadget("device_close_recv").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_recv] ── Round {round} ──");
        let (host_tx, mut host_rx) = host_connect(&dev_info, iface_num).await;
        let (dev_tx, dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            println!("[device] Dropping receiver…");
            drop(dev_rx);

            sleep(Duration::from_secs(1)).await;
            println!("[device] Sending data to host…");
            for i in 0u32..10 {
                dev_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("device send should succeed");
            }
            println!("[device-tx] All packets sent");
            sleep(Duration::from_secs(1)).await;
            drop(dev_tx);
        });

        sleep(Duration::from_secs(2)).await;

        println!("[host] Trying to send after device closed recv…");
        let mut send_failed = false;
        for i in 0u32..100 {
            match host_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await {
                Ok(()) => (),
                Err(_) => {
                    println!("[host] Send failed at packet {i} (expected)");
                    send_failed = true;
                    break;
                }
            }
        }
        assert!(send_failed, "host send should eventually fail after device drops receiver");

        println!("[host] Receiving data from device…");
        for i in 0u32..10 {
            let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
            let expected = i.to_le_bytes();
            assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
        }
        println!("[host-rx] All packets received");

        let res = host_rx.recv().await;
        println!("[host] Recv after device close: {res:?}");
        assert_eq!(res.unwrap(), None, "host recv should get EOF after device done");

        device_task.await.unwrap();
    }

    println!("[device_close_recv] PASSED");
}

// ── Test 5: Device drops receiver, host detects via closed() without sending ─

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn device_close_recv_notify() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup_gadget("device_close_recv_notify").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_recv_notify] ── Round {round} ──");
        let (host_tx, mut host_rx) = host_connect(&dev_info, iface_num).await;
        let (dev_tx, dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            // Device drops receiver — this should trigger CLOSE_RECV notification
            // via the interrupt endpoint so the idle host sender can detect it.
            println!("[device] Dropping receiver…");
            drop(dev_rx);

            // Device can still send data to host.
            sleep(Duration::from_secs(1)).await;
            println!("[device] Sending data to host…");
            for i in 0u32..10 {
                dev_tx.send(Bytes::from(i.to_le_bytes().to_vec())).await.expect("device send should succeed");
            }
            println!("[device-tx] All packets sent");
            sleep(Duration::from_secs(1)).await;
            drop(dev_tx);
        });

        // Host does NOT send any data — instead it waits for the closed() signal
        // which must arrive via the interrupt notification endpoint.
        println!("[host] Waiting for sender closed() event…");
        timeout(TIMEOUT, host_tx.closed()).await.expect("closed() timed out — notification not received");
        println!("[host] Sender closed() returned");
        drop(host_tx);

        // Host should still receive data sent by the device.
        println!("[host] Receiving data from device…");
        for i in 0u32..10 {
            let data = host_rx.recv().await.expect("host recv failed").expect("unexpected EOF");
            let expected = i.to_le_bytes();
            assert_eq!(&data[..], &expected[..], "data mismatch at packet {i}");
        }
        println!("[host-rx] All packets received");

        let res = host_rx.recv().await;
        println!("[host] Recv after device close: {res:?}");
        assert_eq!(res.unwrap(), None, "host recv should get EOF after device done");

        device_task.await.unwrap();
    }

    println!("[device_close_recv_notify] PASSED");
}
