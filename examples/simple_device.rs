//! Simple device-side example from the README.

use std::time::Duration;
use upc::{
    device::{InterfaceId, UpcFunction},
    Class,
};
use usb_gadget::{default_udc, Config, Gadget, Id, Strings};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let class = Class::vendor_specific(0x01, 0);

    // Create a USB gadget with a UPC function.
    let (mut upc, hnd) = UpcFunction::new(InterfaceId::new(class));
    upc.set_info(b"my device".to_vec()).await;

    let udc = default_udc().expect("no UDC available");
    let gadget = Gadget::new(class.into(), Id::new(0x1209, 0x0001), Strings::new("mfr", "product", "serial"))
        .with_config(Config::new("config").with_function(hnd));
    let _reg = gadget.bind(&udc).expect("cannot bind to UDC");

    // Accept a connection and exchange packets.
    let (tx, mut rx) = upc.accept().await?;
    if let Some(data) = rx.recv().await? {
        println!("received: {:?}", data);
        tx.send(b"pong"[..].into()).await?;
    }

    // Allow USB transport to flush before teardown.
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
