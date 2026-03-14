//! Simple host-side example from the README.

use upc::{
    host::{connect, find_interface},
    Class,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let class = Class::vendor_specific(0x01, 0);

    // Find and open the USB device.
    let dev_info = nusb::list_devices()
        .await?
        .find(|d| d.vendor_id() == 0x1209 && d.product_id() == 0x0001)
        .expect("device not found");
    let iface = find_interface(&dev_info, class)?;
    let dev = dev_info.open().await?;

    // Connect and exchange packets.
    let (tx, mut rx) = connect(dev, iface, b"hello").await?;
    tx.send(b"ping"[..].into()).await?;
    let reply = rx.recv().await?;
    println!("received: {:?}", reply);
    Ok(())
}
