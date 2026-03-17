#!/bin/bash
#
# Run the WebUSB test in headless Chromium.
#
# Usage: ./run-web-test.sh [-i|--interactive]
#
# Options:
#   -i, --interactive  Run with a visible browser window and skip policy
#                      installation (use the browser's USB permission dialog).
#
# Prerequisites:
#   - chromedriver and chromium-browser installed
#   - wasm-bindgen-test-runner installed (cargo install wasm-bindgen-cli)
#   - wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown)
#   - USB gadget support (e.g. dwc2/dummy_hcd kernel module)
#   - sudo access (needed for USB gadget and Chromium policy)
#

set -euo pipefail
cd "$(dirname "$0")"

INTERACTIVE=false
for arg in "$@"; do
    case "$arg" in
        -i|--interactive) INTERACTIVE=true ;;
    esac
done

# Cache root password.
sudo true

# Start device example in the background.
echo "Starting device example..."
cargo build --example device --features device --release
sudo -E ./target/release/examples/device &
DEVICE_PID=$!

# Wait for the USB gadget to appear.
echo "Waiting for USB gadget (VID=0004, PID=0005)..."
for i in $(seq 1 30); do
    if lsusb | grep -q "0004:0005"; then
        echo "USB gadget found."
        break
    fi
    if [[ $i -eq 30 ]]; then
        echo "ERROR: USB gadget did not appear within 30 seconds."
        exit 1
    fi
    sleep 1
done

POLICY_DIR="/etc/chromium/policies/managed"
POLICY_FILE="$POLICY_DIR/webusb-test.json"

cleanup() {
    sudo rm -f "$POLICY_FILE"
    sudo kill -INT "$DEVICE_PID" 2>/dev/null && wait "$DEVICE_PID" 2>/dev/null || true
}
trap cleanup EXIT

export WASM_BINDGEN_TEST_ADDRESS="127.0.0.1:8080"

if [[ "$INTERACTIVE" == false ]]; then
    # Install Chromium enterprise policy for WebUSB device pre-authorization.
    # This is the only supported mechanism on Linux — the policy directory
    # /etc/chromium/policies/ is hardcoded in the Chromium source code.
    echo "Installing temporary Chromium WebUSB policy..."
    sudo mkdir -p "$POLICY_DIR"
    sudo tee "$POLICY_FILE" > /dev/null << EOF
{
    "WebUsbAllowDevicesForUrls": [
        {
            "devices": [{ "vendor_id": 4, "product_id": 5 }],
            "urls": ["http://${WASM_BINDGEN_TEST_ADDRESS}"]
        }
    ]
}
EOF
else
    export NO_HEADLESS=1
fi

# Run the web test.
echo "Running web test..."
export CHROMEDRIVER=chromedriver
export WASM_BINDGEN_TEST_ONLY_WEB=1
export WASM_BINDGEN_TEST_TIMEOUT=300
cargo test --target wasm32-unknown-unknown --features web --test web --release --

