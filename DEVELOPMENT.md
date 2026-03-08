Development
===========

Testing
-------

### Native tests

Testing requires two machines connected via USB: a Linux device with USB gadget
support running the device side, and a host machine running the host side.

1. On the **Linux gadget device**, start the device example:

       cargo run --example device --features device

2. On the **host machine**, run the host example:

       cargo run --example host --features host

### Web (WebUSB) test

The `tests/web.rs` integration test targets WebAssembly and exercises the WebUSB
host-side implementation in a browser. It requires user interaction (granting USB
permission and selecting a device) so it cannot run headlessly.

**Prerequisites**

1. Install the [`wasm-bindgen-cli`] toolchain component:

       cargo install wasm-bindgen-cli

2. Add the `wasm32-unknown-unknown` target:

       rustup target add wasm32-unknown-unknown

3. Have a Chromium-based browser (Chrome or Edge) available.

4. Connect a Linux device with USB gadget support and run the `device` example
   on it:

       cargo run --example device --features device

[`wasm-bindgen-cli`]: https://rustwasm.github.io/wasm-bindgen/wasm-bindgen-test/usage.html

**Running the test**

Run the test:

    NO_HEADLESS=1 cargo test --target wasm32-unknown-unknown --features web

On Windows (PowerShell):

    $env:NO_HEADLESS=1; cargo test --target wasm32-unknown-unknown --features web

`NO_HEADLESS=1` opens a webserver, which is required for the
interactive WebUSB permission dialog.

This compiles the test to WebAssembly and launches a webserver via
`wasm-bindgen-test-runner` (configured in `.cargo/config.toml`).
Open the browser at `http://127.0.0.1:8000`, click the page to trigger
the WebUSB permission dialog and select the USB device.
When testing is done, stop the server with Ctrl-C.

See the [wasm-bindgen-test guide] for additional runner options.

[wasm-bindgen-test guide]: https://rustwasm.github.io/wasm-bindgen/wasm-bindgen-test/browsers.html
