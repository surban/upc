//! Device-side example.

use std::time::{Duration, Instant};
use tokio::time::sleep;
use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};
use uuid::uuid;

use upc::device::{InterfaceId, UpcFunction};
use upc::Class;

mod common;
use common::*;


const DEVICE_CLASS: Class = Class::vendor_specific(0xff, 0);

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_log();

    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("Creating UPC function...");
    let (mut upc, hnd) = UpcFunction::new(
        InterfaceId::new(CLASS).with_name(NAME).with_guid(uuid!("3bf77270-42d2-42c6-a475-490227a9cc89")),
    );
    upc.set_info(INFO.to_vec()).await;

    println!("Registering gadget...");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget = Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("usb-packet", "test", "0"))
        .with_config(Config::new("config").with_function(hnd))
        .with_os_descriptor(OsDescriptor::microsoft());
    gadget.device_release = 0x0110;
    let reg = gadget.bind(&udc).expect("cannot bind to UDC");
    assert!(reg.is_attached());

    println!("Waiting for connection...");
    let (tx, mut rx) = upc.accept().await.expect("accept failed");
    assert_eq!(rx.topic(), TOPIC, "wrong topic");

    let rx_task = tokio::spawn(async move {
        let mut rx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
        let mut rx_delay = TestDelayer::new(HOST_SEED);

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

    let mut tx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
    let mut tx_delay = TestDelayer::new(DEVICE_SEED);

    let start = Instant::now();
    let mut total = 0;

    println!("Sending");
    for n in 0..TEST_PACKETS {
        let data = tx_testdata.generate();
        let len = data.len();
        tx.send(data).await.expect("send failed");
        total += len;
        tx_delay.delay().await;

        eprintln!("Send {n}: {len} bytes");
    }

    let elapsed = start.elapsed().as_secs_f32();
    println!("Sent {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

    sleep(Duration::from_secs(1)).await;

    println!("Waiting for receiver...");
    rx_task.await.unwrap();
    drop(tx);
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}
