#![cfg(all(feature = "web", target_arch = "wasm32"))]

use wasm_bindgen_test::wasm_bindgen_test_configure;
wasm_bindgen_test_configure!(run_in_browser);

use std::rc::Rc;
use tokio::sync::oneshot;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_test::wasm_bindgen_test;

use upc::host::{connect, find_interface, info};
use webusb_web::{Usb, UsbDevice, UsbDeviceFilter};

#[path = "../examples/common.rs"]
mod common;
use common::*;

mod web_util;
use web_util::{sleep, wait_for_interaction, ResultExt};

async fn find_device(usb: &Usb) -> UsbDevice {
    // Try pre-authorized devices first (headless / enterprise policy).
    for _ in 0..3 {
        if let Some(dev) = usb.devices().await.into_iter().find(|d| d.vendor_id() == VID && d.product_id() == PID)
        {
            return dev;
        }
        sleep(500).await;
    }

    // Fall back to interactive requestDevice (requires user gesture).
    log!("No pre-authorized device found, requesting interactively...");
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
    let filter = UsbDeviceFilter::new().with_vendor_id(VID).with_product_id(PID);
    usb.request_device([filter]).await.expect_log("requestDevice failed")
}

#[wasm_bindgen_test]
async fn web() {
    log!("Getting WebUSB API");
    let usb = Usb::new().expect_log("cannot get WebUSB API");
    log!("Obtained WebUSB API");

    log!("Waiting for device (VID={VID:#06x}, PID={PID:#06x})...");
    let dev = find_device(&usb).await;
    log!("Using device: {dev:?}");

    log!("Finding interface...");
    let iface = find_interface(&dev, CLASS).expect_log("cannot find interface");
    log!("Using interface {iface}");

    log!("Getting info...");
    let hnd = dev.open().await.expect_log("cannot open device");
    let info = info(&hnd, iface).await.expect_log("cannot get info");
    log!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    let hnd = Rc::new(hnd);

    for round in 0..3 {
        log!("=== Round {} ===", round + 1);

        log!("Connecting...");
        let (tx, mut rx) = connect(hnd.clone(), iface, TOPIC).await.expect_log("cannot connect");

        let overall_start = js_sys::Date::now();

        let (rx_task_done_tx, rx_task_done_rx) = oneshot::channel();
        let (rx_task_finished_tx, rx_task_finished_rx) = oneshot::channel();
        spawn_local(async move {
            let mut rx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
            let mut rx_delay = TestDelayer::new(DEVICE_SEED);

            let start = js_sys::Date::now();
            let mut total = 0usize;

            log!("Receiving...");
            for n in 0..TEST_PACKETS {
                let data = rx.recv().await.expect_log("receive failed").expect("unexpected EOF");
                log!("Recv {n}: {} bytes", data.len());
                total += data.len();
                rx_testdata.validate(&data);
                rx_delay.delay().await;
            }

            let elapsed = (js_sys::Date::now() - start) / 1000.;
            log!("Received {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f64 / elapsed / 1_048_576.);

            rx_task_done_tx.send(total).unwrap();

            log!("Waiting for receiver close");
            assert_eq!(rx.recv().await.unwrap(), None, "receiver not closed");
            log!("Receiver closed");

            rx_task_finished_tx.send(()).unwrap();
        });

        let mut tx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
        let mut tx_delay = TestDelayer::new(HOST_SEED);

        let start = js_sys::Date::now();
        let mut tx_total = 0;

        log!("Sending");
        for n in 0..TEST_PACKETS {
            let data = tx_testdata.generate();
            let len = data.len();
            tx.send(data.into()).await.expect_log("send failed");
            tx_total += len;
            tx_delay.delay().await;

            log!("Send {n}: {len} bytes");
        }

        let elapsed = (js_sys::Date::now() - start) / 1000.;
        log!("Sent {tx_total} bytes in {elapsed:.2} seconds: {} MB/s", tx_total as f64 / elapsed / 1_048_576.);

        let rx_total = rx_task_done_rx.await.unwrap();

        let overall_elapsed = (js_sys::Date::now() - overall_start) / 1000.;
        let total_bytes = tx_total + rx_total;
        log!(
            "Total throughput: {} bytes in {overall_elapsed:.2} seconds: {:.2} MB/s",
            total_bytes,
            total_bytes as f64 / overall_elapsed / 1_048_576.
        );

        log!("Disconnecting...");
        drop(tx);
        rx_task_finished_rx.await.unwrap();
        log!("Disconnected");
    }
}
