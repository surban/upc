Development
===========

Testing
-------

### Loopback test with dummy_hcd (VM-based)

The loopback integration test runs both the USB device and host sides inside
a QEMU VM using the kernel's `dummy_hcd` driver (virtual USB host controller).
No real USB hardware is needed.

**Prerequisites**

1. Install [virtme-ng](https://github.com/arighi/virtme-ng):

       pip3 install --user virtme-ng

2. Install QEMU and zstd:

       sudo apt-get install qemu-system-x86 zstd busybox-static

3. Build the kernel tarball (only needed once; cached afterward):

       .misc/build-kernel.sh

   This downloads and builds a Linux kernel with USB gadget support.
   The resulting tarball is saved to `.misc/kernel-*.tar.zst`.

   Building the kernel also requires standard kernel build deps:

       sudo apt-get install build-essential flex bison libelf-dev libssl-dev bc

**Running the test**

    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=".misc/cargo-test-runner.sh" \
        cargo test --release --features host,device -- --nocapture

The cargo test runner script boots a VM, loads the USB gadget modules, and
executes the test binary inside it. The VM shares the host filesystem via
virtme-ng, so no separate rootfs image is needed.

### Native loopback test (real hardware)

If the machine has a USB Device Controller (UDC) connected back to one of its
own USB host ports via a loopback cable, you can run the loopback integration
test directly without a VM:

    cargo test --release --test loopback --features host,device -- --nocapture

### Native tests with two machines

Testing with two separate machines requires a USB cable connecting a Linux
device with USB gadget support to a host machine.

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
