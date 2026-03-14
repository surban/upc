//! Failure mode integration tests.
//!
//! These tests verify that the device correctly detects a hung host
//! via ping timeout.
//!
//! Each side (host and device) runs in its own single-threaded Tokio
//! runtime on a dedicated thread, so dropping the host runtime truly
//! kills all host-side tasks without sending a graceful CLOSE.
//!
//! Run with:
//!   cargo test --test failure --features host,device -- --nocapture --test-threads=1

#![cfg(all(feature = "host", feature = "device"))]

mod util;

use bytes::Bytes;
use serial_test::serial;
use std::time::Duration;
use util::*;

// ── Test: Host runtime dies, device detects via ping timeout ────────────────

#[test]
#[serial]
fn host_hung() {
    init_log();

    // Device runtime (single-threaded).
    let dev_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build device runtime");

    // Setup gadget on device runtime.
    let (mut upc_fn, dev_info, iface_num, _setup) =
        dev_rt.block_on(setup_gadget("host_hung", "FAILURE TEST", "upc-failure"));

    // Shorten ping timeout to 1 s for a faster test.
    dev_rt.block_on(upc_fn.set_ping_timeout(Some(Duration::from_secs(1))));

    // Host runtime (single-threaded).
    let host_rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("failed to build host runtime");

    // Device accept and host connect must overlap.
    // Run device accept on a background thread.
    let accept_thread = std::thread::spawn(move || {
        let conn = dev_rt.block_on(upc_fn.accept()).expect("accept failed");
        (dev_rt, upc_fn, conn)
    });

    let (_host_tx, _host_rx) = host_rt.block_on(host_connect(&dev_info, iface_num, b"failure"));

    let (dev_rt, _upc_fn, (dev_tx, mut dev_rx)) = accept_thread.join().unwrap();

    // Drop host runtime to simulate a hung/crashed host.
    // This aborts the status-polling task — no graceful CLOSE is sent.
    println!("[host] Dropping host runtime (simulating hung host)…");
    drop(host_rt);
    drop(_host_tx);
    drop(_host_rx);

    // Device should detect the missing STATUS polls as a ping timeout.
    println!("[device] Waiting for device to detect host timeout…");
    let res = dev_rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), dev_rx.recv())
            .await
            .expect("device did not detect host timeout within 5s")
    });
    println!("[device] Recv result: {res:?}");
    assert!(res.is_err(), "device recv should fail with timeout error");

    // Device send should also fail.
    println!("[device] Trying to send…");
    let res = dev_rt.block_on(dev_tx.send(Bytes::from_static(b"should fail")));
    println!("[device] Send result: {res:?}");
    assert!(res.is_err(), "device send should fail after host timeout");

    println!("[host_hung] PASSED");
}

// ── Test: Device hangs, host detects via STATUS poll timeout ────────────────

#[test]
#[serial]
fn device_hung() {
    init_log();

    // Device runtime (single-threaded).
    let dev_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build device runtime");

    // Setup gadget on device runtime.
    let (mut upc_fn, dev_info, iface_num, _setup) =
        dev_rt.block_on(setup_gadget("device_hung", "FAILURE TEST", "upc-failure"));

    // Shorten ping timeout to 1 s so the host polls STATUS every 0.5 s.
    dev_rt.block_on(upc_fn.set_ping_timeout(Some(Duration::from_secs(1))));

    // Host runtime (single-threaded).
    let host_rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("failed to build host runtime");

    // Device accept and host connect must overlap.
    let accept_thread = std::thread::spawn(move || {
        let conn = dev_rt.block_on(upc_fn.accept()).expect("accept failed");
        (dev_rt, upc_fn, conn)
    });

    let (host_tx, mut host_rx) = host_rt.block_on(host_connect(&dev_info, iface_num, b"failure"));

    let (dev_rt, _upc_fn, (_dev_tx, _dev_rx)) = accept_thread.join().unwrap();

    // Stop driving the device runtime — the internal task cannot process
    // STATUS requests anymore, simulating a hung device process.
    // Keep dev_rt alive so the USB gadget stays registered.
    println!("[device] Stopping device runtime (simulating hung device)…");

    // Host should detect the hung device via STATUS poll timeout.
    println!("[host] Waiting for host to detect device timeout…");
    let res = host_rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), host_rx.recv())
            .await
            .expect("host did not detect device timeout within 15s")
    });
    println!("[host] Recv result: {res:?}");
    match res {
        Ok(None) => println!("[host] Got clean EOF"),
        Err(e) => println!("[host] Got error: {e}"),
        Ok(Some(data)) => panic!("unexpected data from hung device: {data:?}"),
    }

    // Wait for the out_task to also detect the dead status and drop the channel.
    // This can take up to TIMEOUT (1s) because close_if_both() sends CTRL_REQ_CLOSE
    // which blocks until the hung device times out.
    host_rt.block_on(host_tx.closed());

    // Host send should also fail.
    println!("[host] Trying to send…");
    let res = host_rt.block_on(host_tx.send(Bytes::from_static(b"should fail")));
    println!("[host] Send result: {res:?}");
    assert!(res.is_err(), "host send should fail after device hung");

    // Cleanup.
    drop(_dev_tx);
    drop(_dev_rx);
    drop(_upc_fn);
    drop(dev_rt);

    println!("[device_hung] PASSED");
}
