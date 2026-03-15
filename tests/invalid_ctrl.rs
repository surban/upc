//! Invalid control request tests.
//!
//! These tests verify that the device properly stalls unknown vendor control
//! requests (both IN and OUT directions) and continues to work correctly
//! afterwards.
//!
//! Run with:
//!   cargo test --test invalid_ctrl --features host,device -- --nocapture --test-threads=1

#![cfg(all(feature = "host", feature = "device"))]

mod util;

use bytes::Bytes;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError};
use serial_test::serial;
use std::time::Duration;
use tokio::time::sleep;
use upc::host::{info, probe};
use util::*;

const TIMEOUT: Duration = Duration::from_secs(1);

async fn setup(test_name: &str) -> (upc::device::UpcFunction, nusb::DeviceInfo, u8, TestSetup) {
    setup_gadget(test_name, "INVALID-CTRL TEST", "upc-invalid-ctrl").await
}

fn invalid_control_in(request: u8, index: u16) -> ControlIn {
    ControlIn {
        control_type: ControlType::Vendor,
        recipient: Recipient::Interface,
        request,
        value: 0,
        index,
        length: 64,
    }
}

fn invalid_control_out(request: u8, index: u16, data: &[u8]) -> ControlOut<'_> {
    ControlOut {
        control_type: ControlType::Vendor,
        recipient: Recipient::Interface,
        request,
        value: 0,
        index,
        data,
    }
}

/// Send invalid control requests (IN and OUT) and verify that each one is stalled.
/// Then establish a normal UPC connection and verify bidirectional data transfer works.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn invalid_control_requests() {
    init_log();
    let (mut upc_fn, dev_info, iface_num, _setup) = setup("invalid_ctrl").await;

    for round in 0..ROUNDS {
        println!("\n[invalid_ctrl] ── Round {round} ──");

        // ── Phase 1: Send invalid control requests ──────────────────────

        println!("[invalid_ctrl] Opening device for raw control transfers…");
        let dev = dev_info.open().await.expect("cannot open device");
        let iface = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                match dev.claim_interface(iface_num).await {
                    Ok(iface) => break iface,
                    Err(_) if tokio::time::Instant::now() < deadline => {
                        sleep(Duration::from_millis(50)).await;
                    }
                    Err(err) => panic!("cannot claim interface: {err}"),
                }
            }
        };

        // Invalid IN request with unknown request code.
        println!("[invalid_ctrl] Sending invalid IN control request (0xF0)…");
        let result = iface.control_in(invalid_control_in(0xF0, iface_num.into()), TIMEOUT).await;
        println!("[invalid_ctrl] Invalid IN result: {result:?}");
        assert!(
            matches!(result, Err(TransferError::Stall)),
            "expected Stall for invalid IN request, got: {result:?}"
        );

        // Invalid OUT request with unknown request code.
        println!("[invalid_ctrl] Sending invalid OUT control request (0xF1)…");
        let result = iface.control_out(invalid_control_out(0xF1, iface_num.into(), b"bogus"), TIMEOUT).await;
        println!("[invalid_ctrl] Invalid OUT result: {result:?}");
        assert!(
            matches!(result, Err(TransferError::Stall)),
            "expected Stall for invalid OUT request, got: {result:?}"
        );

        // Another invalid IN with a different code to make sure repeated stalls work.
        println!("[invalid_ctrl] Sending another invalid IN control request (0xFE)…");
        let result = iface.control_in(invalid_control_in(0xFE, iface_num.into()), TIMEOUT).await;
        println!("[invalid_ctrl] Second invalid IN result: {result:?}");
        assert!(
            matches!(result, Err(TransferError::Stall)),
            "expected Stall for second invalid IN request, got: {result:?}"
        );

        drop(iface);

        // Verify that valid requests still work after the stalls,
        // using the public UPC API.
        println!("[invalid_ctrl] Verifying probe() still works…");
        let is_upc = probe(&dev, iface_num).await.expect("probe failed");
        assert!(is_upc, "probe should return true after stalls");

        println!("[invalid_ctrl] Verifying info() still works…");
        let _info = info(&dev, iface_num).await.expect("info failed");

        drop(dev);

        // ── Phase 2: Normal UPC connection should work fine ─────────────

        println!("[invalid_ctrl] Establishing normal UPC connection…");
        let (host_tx, mut host_rx) = host_connect(&dev_info, iface_num, b"invalid-ctrl-test").await;
        let (dev_tx, mut dev_rx) = upc_fn.accept().await.expect("accept failed");

        let device_task = tokio::spawn(async move {
            // Receive from host.
            for i in 0u32..5 {
                let pkt = dev_rx.recv().await.expect("device recv error").expect("unexpected EOF");
                assert_eq!(pkt.as_ref(), &i.to_le_bytes(), "device received wrong data");
            }
            println!("[device] Received all host packets");

            // Send to host.
            for i in 100u32..105 {
                dev_tx.send(Bytes::copy_from_slice(&i.to_le_bytes())).await.expect("device send error");
            }
            println!("[device] Sent all device packets");
            (dev_tx, dev_rx)
        });

        // Host sends to device.
        for i in 0u32..5 {
            host_tx.send(Bytes::copy_from_slice(&i.to_le_bytes())).await.expect("host send error");
        }
        println!("[host] Sent all host packets");

        // Host receives from device.
        for i in 100u32..105 {
            let pkt = host_rx.recv().await.expect("host recv error").expect("unexpected EOF");
            assert_eq!(pkt.as_ref(), &i.to_le_bytes(), "host received wrong data");
        }
        println!("[host] Received all device packets");

        // Clean up.
        drop(host_tx);
        drop(host_rx);
        let (_dev_tx, _dev_rx) = device_task.await.expect("device task panicked");
        println!("[invalid_ctrl] Round {round} passed ✓");
    }
}
