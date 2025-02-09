//! Host-side example.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::time::sleep;
use upc::host::{connect, find_interface, info};

mod common;
use common::*;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_log();

    let dev = rusb::open_device_with_vid_pid(VID, PID).expect("device not found").device();
    println!("Using device: {dev:?}");

    println!("Finding interface...");
    let iface = find_interface(&dev, CLASS).expect("cannot find interface");
    println!("Using interface {iface}");

    println!("Getting info...");
    let hnd = dev.open().expect("cannot open device");
    let info = info(&hnd, iface).await.expect("cannot get info");
    println!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    println!("Connecting...");
    let (tx, mut rx) = connect(Arc::new(hnd), iface, TOPIC).await.expect("cannot connect");

    let rx_task = tokio::spawn(async move {
        let mut rx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
        let mut rx_delay = TestDelayer::new(DEVICE_SEED);

        let start = Instant::now();
        let mut total = 0;

        println!("Receiving...");
        for n in 0..TEST_PACKETS {
            let data = rx.recv().await.expect("receive failed");
            eprintln!("Recv {n}: {} bytes", data.len());
            total += data.len();
            rx_testdata.validate(&data);
            rx_delay.delay().await;
        }

        let elapsed = start.elapsed().as_secs_f32();
        println!("Received {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

        println!("Waiting for receiver close");
        rx.recv().await.expect_err("receiver not closed");
        println!("Receiver closed");
    });

    let mut tx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
    let mut tx_delay = TestDelayer::new(HOST_SEED);

    let start = Instant::now();
    let mut total = 0;

    println!("Sending");
    for n in 0..TEST_PACKETS {
        let data = tx_testdata.generate();
        let len = data.len();
        tx.send(data.into()).await.expect("send failed");
        total += len;
        tx_delay.delay().await;

        eprintln!("Send {n}: {len} bytes");
    }

    let elapsed = start.elapsed().as_secs_f32();
    println!("Sent {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

    sleep(Duration::from_secs(1)).await;

    println!("Disconnecting...");
    drop(tx);
    rx_task.await.unwrap();
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}
