#!/usr/bin/env bash
#
# Run the WebUSB test inside a VM with dummy_hcd loaded.
#
# Usage:
#   .misc/cargo-test-runner.sh .misc/test-web.sh <build-dir> <wasm-binary>
#
# The WASM test binary must be pre-built on the host (the VM has no network):
#   cargo test --target wasm32-unknown-unknown --features web --test web --release --no-run
#
# Example:
#   .misc/cargo-test-runner.sh .misc/test-web.sh ./target/release \
#     ./target/wasm32-unknown-unknown/release/deps/web-HASH.wasm

set -euo pipefail

# Mount tmpfs on /tmp so that Chrome's shared memory files (MAP_SHARED mmap)
# work correctly. The 9p filesystem used by virtme-ng does not support
# MAP_SHARED mmap, which causes Chrome to crash in multi-process mode.
mount -t tmpfs -o size=1G tmpfs /tmp

BUILD_DIR="${1:-./target/release}"
WASM_BINARY="${2:-}"
DEVICE_BIN="$BUILD_DIR/examples/device"

if [ ! -x "$DEVICE_BIN" ]; then
    echo "ERROR: device binary not found at $DEVICE_BIN" >&2
    exit 1
fi

if [ -z "$WASM_BINARY" ] || [ ! -f "$WASM_BINARY" ]; then
    echo "ERROR: WASM binary not found at $WASM_BINARY" >&2
    echo "Build it first: cargo test --target wasm32-unknown-unknown --features web --test web --release --no-run" >&2
    exit 1
fi

# Ensure USB gadget modules are loaded.
modprobe configfs 2>/dev/null || true
modprobe libcomposite 2>/dev/null || true
modprobe dummy_hcd is_super_speed=Y 2>/dev/null || true

POLICY_FILE=""

cleanup() {
    [ -n "$POLICY_FILE" ] && rm -f "$POLICY_FILE" 2>/dev/null || true
    jobs -p | xargs -r kill 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

# Start device example in the background.
echo "Starting device example..."
"$DEVICE_BIN" &
DEVICE_PID=$!

# Wait for USB gadget to appear.
echo "Waiting for USB gadget (VID=0004, PID=0005)..."
for i in $(seq 1 30); do
    if lsusb 2>/dev/null | grep -q "0004:0005"; then
        echo "USB gadget found."
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: USB gadget did not appear within 30 seconds." >&2
        exit 1
    fi
    sleep 1
done

# Install Chrome enterprise policy for WebUSB device pre-authorization.
POLICY_DIR="/etc/opt/chrome/policies/managed"
POLICY_FILE="$POLICY_DIR/webusb-test.json"
export WASM_BINDGEN_TEST_ADDRESS="127.0.0.1:8080"

# Install Chrome policy (skip if already pre-installed on host, e.g. in CI).
if [ -f "$POLICY_FILE" ]; then
    echo "Chrome WebUSB policy already present."
else
    mkdir -p "$POLICY_DIR"
    cat > "$POLICY_FILE" <<POLICY
{
    "WebUsbAllowDevicesForUrls": [
        {
            "devices": [{ "vendor_id": 4, "product_id": 5 }],
            "urls": ["http://${WASM_BINDGEN_TEST_ADDRESS}"]
        }
    ]
}
POLICY
    echo "Chrome WebUSB policy installed."
fi

# Run the web test using the pre-built WASM binary directly.
# This avoids running cargo inside the VM (no network/DNS available).
echo "Running web test with: $WASM_BINARY"
export CHROMEDRIVER=chromedriver

# Provide WebDriver capabilities with --no-sandbox since Chrome's sandbox
# does not work inside the minimal VM environment.
# Specify the Google Chrome binary explicitly (pre-installed deb on the runner).
# --no-zygote is required because Chrome's zygote process management does not
# work reliably inside virtme-ng VMs.
WEBDRIVER_JSON="/tmp/webdriver.json"
cat > "$WEBDRIVER_JSON" <<WDJSON
{
    "goog:chromeOptions": {
        "binary": "/usr/bin/google-chrome-stable",
        "args": ["--headless=new", "--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu", "--no-zygote"]
    }
}
WDJSON
export WASM_BINDGEN_TEST_WEBDRIVER_JSON="$WEBDRIVER_JSON"

export WASM_BINDGEN_TEST_ONLY_WEB=1
export WASM_BINDGEN_TEST_TIMEOUT=300

# Locate wasm-bindgen-test-runner. Inside a virtme-ng VM the HOME is
# /run/tmp/roothome, so we search common locations.
WBTR=""
for candidate in \
    "$(command -v wasm-bindgen-test-runner 2>/dev/null || true)" \
    "$HOME/.cargo/bin/wasm-bindgen-test-runner" \
    /home/*/.cargo/bin/wasm-bindgen-test-runner \
    /root/.cargo/bin/wasm-bindgen-test-runner
do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        WBTR="$candidate"
        break
    fi
done
if [ -z "$WBTR" ]; then
    echo "ERROR: wasm-bindgen-test-runner not found" >&2
    exit 1
fi
echo "Using wasm-bindgen-test-runner: $WBTR"
"$WBTR" "$WASM_BINARY" 2>&1
