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

The minimum support Rust version (MSRV) is 1.85.

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

    cargo install upc --features cli,host         # host side only
    cargo install upc --features cli,device       # device side only
    cargo install upc --features cli,host,device  # both

### Device side

Start a USB gadget and wait for a connection (requires root and a UDC):

    upc device

This creates a USB gadget with default VID/PID and waits for a host to connect.
Data received from the host is written to stdout; data read from stdin is sent
to the host. Customize with options:

    upc device --serial "my-device" --info "sensor v1" --subclass 01

### Host side

Probe for connected UPC devices:

    upc probe

This outputs one tab-separated line per UPC interface:

    1209:0001	001:005	my-device	0	01	sensor v1

The columns are: VID:PID, bus:address, serial, interface, subclass, info.
Use `--all` for a human-readable listing of all USB devices.

Connect to a UPC device and forward stdin/stdout:

    upc connect

If multiple UPC devices are connected, narrow down with filters:

    upc connect --serial "my-device"
    upc connect --vid 1209 --pid 0001
    upc connect --subclass 01
    upc connect --bus 1 --address 5

### Framing

By default, each line on stdin becomes one UPC packet (`--framing line`).
Use `--framing raw` for binary data where each read becomes a packet.

### Debugging

Set `RUST_LOG` to enable diagnostic output on stderr:

    RUST_LOG=debug upc connect
    RUST_LOG=upc=trace upc device

License
-------

upc is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/upc/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in upc by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
