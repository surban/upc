use std::time::Duration;
use tokio::time::sleep;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use usb_packet::Class;

const VID: u16 = 4;
const PID: u16 = 5;

const DEVICE_CLASS: Class = Class::vendor_specific(22, 0);
const CLASS: Class = Class::vendor_specific(22, 3);
const NAME: &str = "USB-PACKET-TEST";
const TOPIC: &[u8] = b"TEST TOPIC";
const INFO: &[u8] = b"TEST INFO";

#[cfg(feature = "host")]
#[tokio::test]
#[ignore = "device-side companion test required"]
async fn host() {
    use usb_packet::host::{connect, find_interface, info};

    tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();

    let dev = rusb::open_device_with_vid_pid(VID, PID).expect("device not found").device();
    let iface = find_interface(&dev, CLASS, Some(NAME)).expect("cannot find interface");

    println!("Getting info...");
    let info = info(&dev, iface).expect("cannot get info");
    println!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    println!("Connecting...");
    let (tx, mut rx) = connect(&dev, iface, TOPIC).await.expect("cannot connect");

    let rx_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            println!("Received {} bytes: {:x?}", data.len(), &data);
        }
        println!("Receiver closed");
    });

    tx.send(b"hallo".to_vec()).await.unwrap();

    sleep(Duration::from_secs(20)).await;

    println!("Disconnecting...");
    drop(tx);
    rx_task.await.unwrap();
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}

#[cfg(feature = "device")]
#[tokio::test]
#[ignore = "host-side companion test required"]
async fn device() {
    use usb_gadget::{default_udc, Config, Gadget, Id, Strings};
    use usb_packet::device::UpcFunction;

    tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();

    usb_gadget::remove_all().expect("cannot remove all USB gadgets");

    println!("Creating UPC function...");
    let (mut upc, hnd) = UpcFunction::new(CLASS, NAME);

    println!("Registering gadget...");
    let udc = default_udc().expect("cannot get UDC");
    let reg = Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("usb-packet", "test", "0"))
        .with_config(Config::new("config").with_function(hnd))
        .bind(&udc)
        .expect("cannot bind to UDC");
    assert!(reg.is_attached());

    println!("Waiting for connection...");
    let (tx, mut rx) = upc.accept().await.expect("accept failed");

    let rx_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            println!("Received {} bytes: {:x?}", data.len(), &data);
        }
        println!("Receiver closed");
    });

    tx.send(b"device hallo".to_vec()).await.unwrap();

    println!("Waiting for receiver...");
    rx_task.await.unwrap();
    drop(tx);

    sleep(Duration::from_secs(1)).await;
}
