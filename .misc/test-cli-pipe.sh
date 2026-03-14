#!/usr/bin/env bash
#
# Test the UPC CLI tool for proper UNIX pipe behavior.
#
# Verifies that `upc device` and `upc connect` handle stdin/stdout
# correctly: no deadlocks on EOF, broken pipe, or closed file descriptors.
#
# Usage:
#   .misc/test-cli-pipe.sh [path-to-upc-binary]
#
# Must be run inside a VM with dummy_hcd loaded, e.g.:
#   .misc/cargo-test-runner.sh .misc/test-cli-pipe.sh ./target/release/upc

set -euo pipefail

UPC="${1:-./target/release/upc}"

if [ ! -x "$UPC" ]; then
    echo "ERROR: upc binary not found at $UPC" >&2
    exit 1
fi

# Ensure USB gadget modules are loaded (idempotent).
modprobe configfs 2>/dev/null || true
modprobe libcomposite 2>/dev/null || true
modprobe dummy_hcd is_super_speed=Y 2>/dev/null || true

PASS=0
FAIL=0

cleanup() {
    jobs -p | xargs -r kill 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

pass() { PASS=$((PASS + 1)); echo "PASS"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL: $*"; }

# Wait until a UPC gadget is visible via scan.
wait_gadget() {
    local i
    for i in $(seq 1 50); do
        if "$UPC" scan 2>/dev/null | grep -q .; then
            sleep 0.3; return 0
        fi
        sleep 0.2
    done
    return 1
}

# Wait until no UPC gadget is present.
wait_no_gadget() {
    local i
    for i in $(seq 1 25); do
        if ! "$UPC" scan 2>/dev/null | grep -q .; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# Wait for a PID to finish (with timeout in seconds).
# Uses /proc to correctly detect zombie processes.
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

echo "UPC CLI pipe behavior tests"
echo "==========================="
echo "Binary: $UPC"
echo ""

# -- 1. Bidirectional line-mode transfer --
echo -n "  1. Bidirectional line transfer ... "
dev_out=$(mktemp); host_out=$(mktemp)
echo "from-device" | "$UPC" device --info t1 >"$dev_out" 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    echo "from-host" | "$UPC" connect --topic t1 >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        d=$(cat "$dev_out"); h=$(cat "$host_out")
        # Line mode preserves newlines.
        if [ "$d" = "from-host" ] && [ "$h" = "from-device" ]; then pass
        else fail "got d='$(echo "$d" | cat -v)' h='$(echo "$h" | cat -v)'"; fi
    fi
fi
rm -f "$dev_out" "$host_out"; wait_no_gadget || true

# -- 2. Host stdin EOF --
echo -n "  2. Host stdin EOF (no deadlock) ... "
echo "data" | "$UPC" device --info t2 >/dev/null 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    </dev/null "$UPC" connect --topic t2 >/dev/null 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"
    else pass; fi
fi
wait_no_gadget || true

# -- 3. Both stdin and stdout closed --
echo -n "  3. Both stdin/stdout closed ... "
</dev/null "$UPC" device --info t3 >/dev/null 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    </dev/null "$UPC" connect --topic t3 >/dev/null 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"
    else pass; fi
fi
wait_no_gadget || true

# -- 4. Host stdout closed early (broken pipe) --
echo -n "  4. Host stdout closed early (no deadlock) ... "
yes | "$UPC" device --info t4 --framing raw >/dev/null 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    echo "x" | "$UPC" connect --topic t4 --framing raw 2>/dev/null | head -c 1 >/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"
    else pass; fi
fi
wait_no_gadget || true

# -- 5. Device stdin EOF, host sends data --
echo -n "  5. Device stdin EOF, host sends data ... "
host_out=$(mktemp)
</dev/null "$UPC" device --info t5 >/dev/null 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    echo "hello" | "$UPC" connect --topic t5 >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        h=$(cat "$host_out")
        if [ -z "$h" ]; then pass
        else fail "expected empty, got '$h'"; fi
    fi
fi
rm -f "$host_out"; wait_no_gadget || true

# -- 6. Multi-line transfer --
echo -n "  6. Multi-line transfer ... "
dev_out=$(mktemp); host_out=$(mktemp)
printf 'A\nB\nC\n' | "$UPC" device --info t6 >"$dev_out" 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    printf '1\n2\n3\n' | "$UPC" connect --topic t6 >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        d=$(cat "$dev_out"); h=$(cat "$host_out")
        # Line mode preserves newlines in each packet.
        expected_d=$(printf '1\n2\n3\n')
        expected_h=$(printf 'A\nB\nC\n')
        if [ "$d" = "$expected_d" ] && [ "$h" = "$expected_h" ]; then pass
        else fail "got d='$(echo "$d" | cat -v)' h='$(echo "$h" | cat -v)'"; fi
    fi
fi
rm -f "$dev_out" "$host_out"; wait_no_gadget || true

# -- 7. Raw framing transfer --
echo -n "  7. Raw framing transfer ... "
host_out=$(mktemp)
printf 'line1\nline2\n' | "$UPC" device --info t7 --framing raw >/dev/null 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    printf 'hello\n' | "$UPC" connect --topic t7 --framing raw >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        h=$(cat "$host_out")
        expected=$(printf 'line1\nline2\n')
        if [ "$h" = "$expected" ]; then pass
        else fail "got '$(echo "$h" | cat -v)'"; fi
    fi
fi
rm -f "$host_out"; wait_no_gadget || true

# -- 8. Device stdout closed early (broken pipe) --
echo -n "  8. Device stdout closed early (no deadlock) ... "
(
    "$UPC" device --info t8 --framing raw </dev/null 2>/dev/null | head -c 1 >/dev/null
) &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    (yes 2>/dev/null || true) | "$UPC" connect --topic t8 --framing raw >/dev/null 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"
    else pass; fi
fi
wait_no_gadget || true

# -- 9. Bidirectional raw transfer --
echo -n "  9. Bidirectional raw transfer ... "
dev_out=$(mktemp); host_out=$(mktemp)
printf 'from-device' | "$UPC" device --info t9 --framing raw >"$dev_out" 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    printf 'from-host' | "$UPC" connect --topic t9 --framing raw >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        d=$(cat "$dev_out"); h=$(cat "$host_out")
        if [ "$d" = "from-host" ] && [ "$h" = "from-device" ]; then pass
        else fail "got d='$(echo "$d" | cat -v)' h='$(echo "$h" | cat -v)'"; fi
    fi
fi
rm -f "$dev_out" "$host_out"; wait_no_gadget || true

# -- 10. Large line-mode transfer --
echo -n " 10. Large line-mode transfer ... "
dev_in=$(mktemp); host_in=$(mktemp)
dev_out=$(mktemp); host_out=$(mktemp)
seq 1 5000 | sed 's/^/D/' > "$dev_in"
seq 1 5000 | sed 's/^/H/' > "$host_in"
"$UPC" device --info t10 <"$dev_in" >"$dev_out" 2>/dev/null &
dp=$!
if ! wait_gadget; then kill_wait $dp; fail "gadget timeout"; else
    "$UPC" connect --topic t10 <"$host_in" >"$host_out" 2>/dev/null
    if ! wait_exit $dp 15; then kill_wait $dp; fail "device deadlock"; else
        if cmp -s "$host_in" "$dev_out" && cmp -s "$dev_in" "$host_out"; then pass
        else fail "data mismatch (dev_in=$(wc -c <"$dev_in")B host_in=$(wc -c <"$host_in")B dev_out=$(wc -c <"$dev_out")B host_out=$(wc -c <"$host_out")B)"; fi
    fi
fi
rm -f "$dev_in" "$host_in" "$dev_out" "$host_out"; wait_no_gadget || true

# -- Summary --
echo ""
total=$((PASS + FAIL))
echo "Results: $PASS/$total passed"
[ "$FAIL" -eq 0 ]
