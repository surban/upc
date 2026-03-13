//! Shared test utilities for integration tests.

#![allow(dead_code)]

use std::time::Duration;
use tokio::time::sleep;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};

use upc::{
    device::{InterfaceId, UpcFunction},
    host::{connect, find_interface},
    Class,
};

// ── Constants ────────────────────────────────────────────────────────────────

pub const VID: u16 = 4;
pub const PID: u16 = 5;
pub const ROUNDS: usize = 3;

pub const CLASS: Class = Class::vendor_specific(22, 3);
pub const DEVICE_CLASS: Class = Class::vendor_specific(0xff, 0);

// ── Logging initializer ─────────────────────────────────────────────────────

pub fn init_log() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
        tracing_log::LogTracer::init().unwrap();
    });
}

// ── Helpers: set up gadget, then connect per round ───────────────────────────

pub struct TestSetup {
    pub _reg: Box<dyn std::any::Any + Send>,
}

pub async fn setup_gadget(
    test_name: &str, iface_name: &str, gadget_name: &str,
) -> (UpcFunction, nusb::DeviceInfo, u8, TestSetup) {
    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("[{test_name}] Creating UPC function…");
    let (upc_fn, hnd) = UpcFunction::new(InterfaceId::new(CLASS).with_name(iface_name));

    println!("[{test_name}] Registering gadget…");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget = Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new(gadget_name, "test", "0"))
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

pub async fn host_connect(
    dev_info: &nusb::DeviceInfo, iface_num: u8, topic: &[u8],
) -> (upc::host::UpcSender, upc::host::UpcReceiver) {
    let dev = dev_info.open().await.expect("cannot open device");
    connect(dev, iface_num, topic).await.expect("connect failed")
}
