USB packet channel (UPC)
========================

[![crates.io page](https://img.shields.io/crates/v/upc)](https://crates.io/crates/upc)
[![docs.rs page](https://docs.rs/upc/badge.svg)](https://docs.rs/upc)
[![Apache 2.0 license](https://img.shields.io/crates/l/upc)](https://github.com/surban/upc/blob/master/LICENSE)

This library provides a reliable, packet-based transport over a physical USB connection with an asynchronous API.

Features
--------

This crate provides the following main features:

* `host` enables the native host-side part,
* `web` enables the web host-side part using [WebUSB] for device access and targeting WebAssembly,
* `device` enables the device-side part.

To be useful, at least one of these features must be enabled.

Additionally, the feature `trace-packets` can be enabled to log USB packets at log level trace.

[WebUSB]: https://developer.mozilla.org/en-US/docs/Web/API/WebUSB_API

Requirements
------------

The minimum support Rust version (MSRV) is 1.80.

The native host-side part supports any operating system supported by `libusb`.

The device-side part requires Linux and a USB device controller (UDC).

License
-------

upc is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/upc/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in upc by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
