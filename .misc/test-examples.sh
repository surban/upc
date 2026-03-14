#!/usr/bin/env bash
#
# Run the simple_device and simple_host examples against each other
# inside a VM with dummy_hcd loaded.
#
# Usage:
#   .misc/cargo-test-runner.sh .misc/test-examples.sh <build-dir>
#
# Example:
#   .misc/cargo-test-runner.sh .misc/test-examples.sh ./target/release

set -euo pipefail

BUILD_DIR="${1:-./target/release}"
SIMPLE_DEVICE="$BUILD_DIR/examples/simple_device"
SIMPLE_HOST="$BUILD_DIR/examples/simple_host"

for bin in "$SIMPLE_DEVICE" "$SIMPLE_HOST"; do
    if [ ! -x "$bin" ]; then
        echo "ERROR: binary not found at $bin" >&2
        exit 1
    fi
done

# Ensure USB gadget modules are loaded.
modprobe configfs 2>/dev/null || true
modprobe libcomposite 2>/dev/null || true
modprobe dummy_hcd is_super_speed=Y 2>/dev/null || true

cleanup() {
    jobs -p | xargs -r kill 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  PASS"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $*"; }

# Wait until a UPC device is visible via nusb (simple_host will find it).
wait_gadget() {
    local i
    for i in $(seq 1 50); do
        if lsusb 2>/dev/null | grep -q "1209:0001"; then
            sleep 0.5; return 0
        fi
        sleep 0.2
    done
    return 1
}

wait_exit() {
    local pid=$1 secs=$2 i
    for i in $(seq 1 $((secs * 10))); do
        if [ ! -e "/proc/$pid/status" ] \
            || grep -q '^State:.*Z' "/proc/$pid/status" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            return 0
        fi
        sleep 0.1
    done
    return 1
}

kill_wait() { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }

echo "Example execution tests"
echo "======================="
echo ""

# -- simple_device + simple_host --
echo -n "  simple_device + simple_host ..."
dev_out=$(mktemp); host_out=$(mktemp)

"$SIMPLE_DEVICE" >"$dev_out" 2>&1 &
dp=$!

if ! wait_gadget; then
    kill_wait $dp
    fail "gadget did not appear"
else
    "$SIMPLE_HOST" >"$host_out" 2>&1
    host_rc=$?

    if ! wait_exit $dp 15; then
        kill_wait $dp
        fail "device did not exit"
    else
        wait "$dp" 2>/dev/null
        dev_rc=$?

        d=$(cat "$dev_out")
        h=$(cat "$host_out")

        if [ $host_rc -ne 0 ]; then
            fail "host exited with $host_rc: $h"
        elif [ $dev_rc -ne 0 ]; then
            fail "device exited with $dev_rc: $d"
        elif echo "$d" | grep -q 'received: b"ping"' \
          && echo "$h" | grep -q 'received: Some(b"pong")'; then
            pass
        else
            fail "unexpected output: device='$d' host='$h'"
        fi
    fi
fi

rm -f "$dev_out" "$host_out"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
