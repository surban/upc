//! Half-close integration tests.
//!
//! These tests verify that each direction of a UPC channel can be closed
//! independently (half-close) without disturbing the other direction.
//! Each test runs multiple rounds to verify reconnection works correctly.
//!
//! Run with:
//!   cargo test --test half_close --features host,device -- --nocapture --test-threads=1

#![cfg(all(feature = "host", feature = "device"))]

mod util;

use bytes::Bytes;
use serial_test::serial;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use util::*;

const TIMEOUT: Duration = Duration::from_secs(10);

async fn setup(test_name: &str) -> (upc::device::UpcFunction, nusb::DeviceInfo, u8, TestSetup) {
    setup_gadget(test_name, "HALF-CLOSE TEST", "upc-halfclose").await
}

async fn connect(dev_info: &nusb::DeviceInfo, iface_num: u8) -> (upc::host::UpcSender, upc::host::UpcReceiver) {
    host_connect(dev_info, iface_num, b"halfclose").await
}

// ── Test 1: Host drops sender, device can still send ────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn host_close_send() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("host_close_send").await;

    for round in 0..ROUNDS {
        println!("\n[host_close_send] ── Round {round} ──");
        let (host_tx, mut host_rx) = connect(&dev_info, iface_num).await;
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
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("host_close_recv").await;

    for round in 0..ROUNDS {
        println!("\n[host_close_recv] ── Round {round} ──");
        let (host_tx, host_rx) = connect(&dev_info, iface_num).await;
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
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("device_close_send").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_send] ── Round {round} ──");
        let (host_tx, mut host_rx) = connect(&dev_info, iface_num).await;
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
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("device_close_recv").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_recv] ── Round {round} ──");
        let (host_tx, mut host_rx) = connect(&dev_info, iface_num).await;
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
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("device_close_recv_notify").await;

    for round in 0..ROUNDS {
        println!("\n[device_close_recv_notify] ── Round {round} ──");
        let (host_tx, mut host_rx) = connect(&dev_info, iface_num).await;
        let (dev_tx, dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            // Device drops receiver — this should trigger CLOSE_RECV status
            // via the next STATUS poll so the idle host sender can detect it.
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
        // which must arrive via the STATUS control request poll.
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
