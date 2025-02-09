#![cfg(all(feature = "web", target_arch = "wasm32"))]

use std::rc::Rc;
use tokio::sync::oneshot;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_test::wasm_bindgen_test;

use upc::host::{connect, find_interface, info};
use webusb_web::{Usb, UsbDeviceFilter};

#[path = "../examples/common.rs"]
mod common;
use common::*;

mod util;
use util::{wait_for_interaction, ResultExt};

const FILTER: UsbDeviceFilter = UsbDeviceFilter::new().with_vendor_id(VID).with_product_id(PID);

#[wasm_bindgen_test]
async fn web() {
    log!("Getting WebUSB API");
    let usb = Usb::new().expect_log("cannot get WebUSB API");
    log!("Obtained WebUSB API");

    wait_for_interaction(
        "\
        Please connect a Linux device supporting USB gadget mode and run the \
        <pre>device</pre> example on it.
        <br>\
        Then click here to enable permission requests and continue.\
        Then select the USB device from the popup shown by your browser.\
    ",
    )
    .await;

    log!("Requesting device");
    let dev = usb.request_device([FILTER]).await.expect_log("no device obtained");
    log!("Using device: {dev:?}");

    log!("Finding interface...");
    let iface = find_interface(&dev, CLASS).expect_log("cannot find interface");
    log!("Using interface {iface}");

    log!("Getting info...");
    let hnd = dev.open().await.expect_log("cannot open device");
    let info = info(&hnd, iface).await.expect_log("cannot get info");
    log!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    log!("Connecting...");
    let (tx, mut rx) = connect(Rc::new(hnd), iface, TOPIC).await.expect_log("cannot connect");

    let (rx_task_done_tx, rx_task_done_rx) = oneshot::channel();
    spawn_local(async move {
        let mut rx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
        let mut rx_delay = TestDelayer::new(DEVICE_SEED);

        let start = js_sys::Date::now();
        let mut total = 0;

        log!("Receiving...");
        for n in 0..TEST_PACKETS {
            let data = rx.recv().await.expect_log("receive failed");
            log!("Recv {n}: {} bytes", data.len());
            total += data.len();
            rx_testdata.validate(&data);
            rx_delay.delay().await;
        }

        let elapsed = (js_sys::Date::now() - start) / 1000.;
        log!("Received {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f64 / elapsed / 1_048_576.);

        log!("Waiting for receiver close");
        rx.recv().await.expect_err("receiver not closed");
        log!("Receiver closed");

        rx_task_done_tx.send(()).unwrap();
    });

    let mut tx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
    let mut tx_delay = TestDelayer::new(HOST_SEED);

    let start = js_sys::Date::now();
    let mut total = 0;

    log!("Sending");
    for n in 0..TEST_PACKETS {
        let data = tx_testdata.generate();
        let len = data.len();
        tx.send(data.into()).await.expect_log("send failed");
        total += len;
        tx_delay.delay().await;

        log!("Send {n}: {len} bytes");
    }

    let elapsed = (js_sys::Date::now() - start) / 1000.;
    log!("Sent {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f64 / elapsed / 1_048_576.);

    log!("Disconnecting...");
    drop(tx);
    rx_task_done_rx.await.unwrap();
    log!("Disconnected");
}
