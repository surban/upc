USB packet channel (UPC)
========================

[![crates.io page](https://img.shields.io/crates/v/upc)](https://crates.io/crates/upc)
[![docs.rs page](https://docs.rs/upc/badge.svg)](https://docs.rs/upc)
[![Apache 2.0 license](https://img.shields.io/crates/l/upc)](https://github.com/surban/upc/blob/master/LICENSE)

UPC provides a reliable, packet-based transport over a physical USB connection.
It uses a vendor-specific USB interface with bulk endpoints and works with any
USB device controller (UDC) on the device side and any operating system on the
host side, including web browsers via [WebUSB].

The library offers an asynchronous Rust API for both the host and device side,
making it easy to build custom USB communication into your application.
A [command-line tool](#cli-tool) is also included for testing, debugging and
standalone data transfer.

[WebUSB]: https://developer.mozilla.org/en-US/docs/Web/API/WebUSB_API

Usage
-----

Add `upc` to your `Cargo.toml` with the features you need:

```toml
[dependencies]
upc = { version = "1", features = ["host"] }    # host side
# or
upc = { version = "1", features = ["device"] }  # device side
```

### Host side

```rust,no_run
use upc::{host::{connect, find_interface}, Class};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let class = Class::vendor_specific(0x01, 0);

    // Find and open the USB device.
    let dev_info = nusb::list_devices().await?
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
```

### Device side

```rust,no_run
use std::time::Duration;
use upc::{device::{InterfaceId, UpcFunction}, Class};
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
```

See the [examples](examples/) directory and [API documentation](https://docs.rs/upc) for more details.

Features
--------

This crate provides the following main features:

* `host` enables the native host-side part,
* `web` enables the web host-side part using [WebUSB] for device access and targeting WebAssembly,
* `device` enables the device-side part.

To be useful, at least one of these features must be enabled.

Additionally, the feature `trace-packets` can be enabled to log USB packets at log level trace.

Requirements
------------

The minimum supported Rust version (MSRV) is 1.85.

The native host-side part supports any operating system supported by [nusb].

The device-side part requires Linux and a USB device controller (UDC).

[nusb]: https://crates.io/crates/nusb

CLI tool
--------

The `upc` command-line tool can act as either the host or device side of a
UPC connection, forwarding data between stdin/stdout and the USB channel.
Use it to test and debug devices that use the UPC library, or as a standalone
tool for transferring data over USB.

Install it with:

```console
cargo install upc --features cli,host         # host side only
cargo install upc --features cli,device       # device side only
cargo install upc --features cli,host,device  # both
```

### Device side

Start a USB gadget and wait for a connection (requires root and a UDC):

```console
upc device
```

This creates a USB gadget with default VID/PID and waits for a host to connect.
Data received from the host is written to stdout; data read from stdin is sent
to the host. 

### Host side

Probe for connected UPC devices:

```console
upc probe
```

This outputs one tab-separated line per UPC interface:

```text
1209:0001	001:005	my-device	0	01	sensor v1
```

The columns are: VID:PID, bus:address, serial, interface, subclass, info.
Use `--all` for a human-readable listing of all USB devices.

Connect to a UPC device and forward stdin/stdout:

```console
upc connect
```

### Debugging

Set `RUST_LOG` to enable diagnostic output on stderr:

```console
RUST_LOG=debug upc connect
RUST_LOG=upc=trace upc device
```

License
-------

upc is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/upc/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in upc by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
