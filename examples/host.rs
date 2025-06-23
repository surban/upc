//! Host-side example.

use std::time::{Duration, Instant};

use tokio::{sync::oneshot, time::sleep};
use upc::host::{connect, find_interface, info};

mod common;
use common::*;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_log();

    let dev_info = nusb::list_devices()
        .await
        .expect("cannot enumerate USB devices")
        .find(|cand| cand.vendor_id() == VID && cand.product_id() == PID)
        .expect("device not found");
    println!("Using device: {dev_info:?}");

    println!("Finding interface...");
    let iface = find_interface(&dev_info, CLASS).expect("cannot find interface");
    println!("Using interface {iface}");

    println!("Getting info...");
    let dev = dev_info.open().await.expect("cannot open device");
    let info = info(&dev, iface).await.expect("cannot get info");
    println!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    println!("Connecting...");
    let (tx, mut rx) = connect(dev, iface, TOPIC).await.expect("cannot connect");

    let (rx_task_done_tx, rx_task_done_rx) = oneshot::channel();
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

        rx_task_done_tx.send(()).unwrap();

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

    rx_task_done_rx.await.unwrap();
    sleep(Duration::from_secs(3)).await;

    println!("Disconnecting...");
    drop(tx);
    rx_task.await.unwrap();
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}
