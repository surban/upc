UPC Protocol Specification
==========================

* **Version**: 1.0
* **Date**: 2026-03-14
* **Author**: Sebastian Urban \<surban@surban.net\>
* **License**: Apache-2.0
* **Status**: Stable
* **Reference implementation**: <https://github.com/surban/upc>

This document describes the USB Packet Channel (UPC) wire protocol.
It is intended for implementers building UPC-compatible devices or hosts
in any language or on any platform, including bare-metal microcontrollers.

Terminology
-----------

The key words **must**, **must not**, **should**, **should not**, and **may**
are used throughout this document to indicate requirement levels.

* **Host**: the USB host (computer, single-board computer, browser, etc.).
* **Device**: the USB device (gadget, microcontroller, etc.).
* **Application packet**: a single message at the application level,
  which may span one or more USB transfers.

1\. USB Interface
-----------------

UPC uses a single USB interface with:

* **Class**: `0xFF` (vendor-specific)
* **SubClass**: user-defined (application-specific)
* **Protocol**: user-defined (application-specific)

The interface **must** contain exactly two bulk endpoints:

* **Bulk IN** (device → host): carries application data.
* **Bulk OUT** (host → device): carries application data.

Endpoint addresses are not fixed — they are read from the interface's
endpoint descriptors at runtime.

All connection management is performed through **vendor-specific control
transfers** on the default control endpoint (EP0). The control transfers use:

* **Type**: Vendor
* **Recipient**: Interface
* **wIndex**: interface number
* **wValue**: 0 (reserved for future use)

### Microsoft OS Descriptors (optional)

To enable driverless operation on Windows, the device **should** include:

* A Microsoft OS 2.0 compatible ID descriptor marking the interface as
  WinUSB-compatible.
* A device interface GUID property so Windows can enumerate the device.

These are standard Microsoft OS descriptor mechanisms and are not part of
the UPC protocol itself.

2\. Control Requests
--------------------

All control requests use vendor type, interface recipient, with
`wIndex` = interface number and `wValue` = 0.

| ID     | Name           | Direction | Data                        | Required |
|--------|----------------|-----------|-----------------------------|----------|
| `0x00` | PROBE          | IN        | 3 bytes: `"UPC"`            | Optional |
| `0x01` | OPEN           | OUT       | 0–4096 bytes: topic         | Required |
| `0x02` | CLOSE          | OUT       | empty                       | Required |
| `0x03` | INFO           | IN        | 0–4096 bytes: info          | Optional |
| `0x04` | CLOSE_SEND     | OUT       | 8 bytes: byte count (u64 LE)| Optional |
| `0x05` | CLOSE_RECV     | OUT       | empty                       | Optional |
| `0x06` | STATUS         | IN        | 0–8 bytes: status flags     | Optional |
| `0x07` | CAPABILITIES   | IN or OUT | TLV-encoded capabilities    | Optional |
| `0x08` | ECHO           | OUT       | 0–MPS bytes: marker data    | Optional |

### PROBE (0x00) — Device → Host

The host sends a PROBE request to check whether the interface implements
UPC.

* The device **should** respond with the 3-byte ASCII string `"UPC"`
  (bytes `0x55 0x50 0x43`).
* If the interface does not support UPC, the device **must** stall the
  request.
* A stall is not an error — the host interprets it as "not a UPC
  interface."

PROBE is intended for auto-discovery. It **may** be sent at any time,
even before OPEN.

### OPEN (0x01) — Host → Device

Opens a new connection.

* The host **must** send OPEN before transmitting data on the bulk
  endpoints.
* The data payload contains a user-defined **topic** (0 to 4096 bytes).
  The topic is delivered to the device application and may carry
  connection metadata (e.g. a service name).
* An empty topic (0 bytes) is valid.
* The device **must** accept the OPEN request and prepare to exchange
  data on the bulk endpoints. OPEN always succeeds at the protocol
  level — topic-based filtering is an application concern. If the
  device application does not recognize the topic, it **may**
  immediately close the connection after accepting.
* If OPEN is received while a connection is already active, the device
  **must** close the previous connection (as if CLOSE were received)
  and open a new one.

### CLOSE (0x02) — Host → Device

Closes the connection in both directions.

* The host **must** send CLOSE when the connection is fully torn down
  (both directions closed).
* The host **should** also send CLOSE at the start of a new connection
  (before OPEN) to ensure any stale state from a previous connection is
  cleaned up.
* On receiving CLOSE, the device **must** stop all bulk endpoint
  activity and reset its connection state.

### INFO (0x03) — Device → Host

Reads device-provided information.

* The device **may** respond with up to 4096 bytes of free-form data,
  typically describing the device or service.
* This request **may** be sent at any time, including before OPEN.
* If not supported, the device **should** stall the request.

### CLOSE_SEND (0x04) — Host → Device

The host signals that it is done sending data (half-close of the
host → device direction).

* The payload is 8 bytes containing the total number of bytes sent
  by the host as a **little-endian u64**.
* The device **should** use this byte count to drain any remaining
  buffered data from the OUT endpoint before considering the direction
  closed. The device continues receiving from the OUT endpoint until
  the total number of bytes received equals the byte count or the
  connection is terminated by other means (e.g. CLOSE or
  disconnection). This is necessary because USB bulk transfers may be
  buffered in the device controller and the byte count lets the device
  know when all data has actually arrived.
* If the device does not support half-close, it **may** ignore this
  request.

### CLOSE_RECV (0x05) — Host → Device

The host signals that it is done receiving data (half-close of the
device → host direction).

* The payload is empty.
* On receiving this, the device **should** stop sending data and halt
  the IN endpoint.
* If the device does not support half-close, it **may** ignore this
  request.

### STATUS (0x06) — Device → Host

Liveness check and status query.

* The device **must** respond with 0 to 8 bytes. Each byte is a status
  flag.
* An empty response (0 bytes) means the device is alive and everything
  is normal.
* Defined status bytes:

  | Value  | Name          | Meaning                                    |
  |--------|---------------|--------------------------------------------|
  | `0x01` | RECV_CLOSED   | Device has closed its receive direction.    |

  Unknown status bytes **should** be ignored by the host for forward
  compatibility.

* The device advertises support for STATUS via the `status_supported`
  capability (see section 4). If the device does not advertise support,
  the host **must not** send STATUS requests.
* The host uses STATUS as a liveness ping. The device uses it to detect
  host disconnection (see section 5).

### CAPABILITIES (0x07) — Bidirectional

Exchanges capability information between host and device.

* **Device → Host (IN)**: the host queries the device's capabilities.
  The device responds with TLV-encoded data (up to 256 bytes).
* **Host → Device (OUT)**: the host sends its own capabilities as
  TLV-encoded data (up to 256 bytes).
* If the device does not support CAPABILITIES, it **should** stall the
  request. The host falls back to default values.
* CAPABILITIES **should** be exchanged directly before OPEN.
* If the CAPABILITIES exchange is not performed or fails, both sides
  **must** use the default capability values defined in section 4.

See section 4 for the TLV encoding and capability definitions.

### ECHO (0x08) — Host → Device

Synchronization barrier for flushing stale bulk IN data.

* The host sends an ECHO request with a marker payload
  (up to (MPS-1) bytes of random data).
* The device **must** only accept ECHO when no connection is open.
  If a connection is open, the device **should** silently ignore the
  request (accept the control transfer but discard the data).
* On receiving ECHO, the device sends the marker payload back to the
  host via the **bulk IN endpoint** using standard short-packet
  framing.
* The host reads from the bulk IN endpoint, discarding any stale data,
  until it receives a transfer matching the marker. At that point all
  stale data has been drained and the IN endpoint is clean.
* ECHO is intended to be used between CLOSE and OPEN during connection
  setup, as a deterministic alternative to timeout-based flushing.
  This is particularly useful on platforms (e.g. WebUSB) where
  cancelling pending IN transfers is unreliable.
* The device advertises support for ECHO via the `echo_supported`
  capability (see section 4). If the device does not advertise support,
  the host **must not** send ECHO requests.

3\. Data Framing
----------------

UPC uses **USB short-packet framing** to delimit application-level
messages on the bulk endpoints. There is no length header — message
boundaries are determined entirely by the USB short-packet mechanism.

### Background

USB bulk endpoints have a **maximum packet size (MPS)** determined by
the endpoint descriptor (typically 512 bytes for USB 2.0 High Speed,
1024 bytes for USB 3.x SuperSpeed). At the USB wire level, data is
transmitted in packets of up to MPS bytes. A packet shorter than MPS
(including a zero-length packet) is called a **short packet** and
signals the end of a transfer.

Application code typically interacts with USB through **transfers**,
where a single transfer may span many USB packets. The USB controller
handles packetization transparently:

* **OUT (send):** the controller splits the transfer data into
  MPS-sized USB packets automatically.
* **IN (receive):** the controller accumulates incoming USB packets into
  the transfer buffer until a short packet arrives or the buffer is
  full.

### Sending

To send an application packet of N bytes:

1. Submit the data as one or more USB OUT transfers. The USB controller
   splits each transfer into MPS-sized USB packets automatically.
   If the data is split across multiple transfers, **all transfers
   except the last must have a size that is a positive multiple of
   MPS**. Otherwise the USB controller would emit a short packet at
   the end of an intermediate transfer, which the receiver would
   incorrectly interpret as end-of-message.
2. **If** N is a non-zero multiple of MPS, send an additional
   **zero-length packet (ZLP)** as a separate transfer to signal the
   end of the message. Without the ZLP, the last USB packet on the wire
   is full-sized and the receiver cannot distinguish end-of-message from
   more data to follow.
3. **If** N is not a multiple of MPS, the last USB packet on the wire is
   naturally shorter than MPS. This short packet signals end-of-message
   and the sender **must not** send a ZLP — doing so would be
   interpreted as a separate, empty application packet.

**Empty application packets** (0 bytes) are sent as a single ZLP.

### Receiving

To receive an application packet:

1. Submit USB IN transfers with a buffer size that is a **positive
   multiple of MPS**. The larger the buffer, the fewer transfer
   completions are needed, improving throughput. The minimum buffer
   size is MPS.
2. Accumulate received data from completed transfers.
3. When a transfer completes with **fewer bytes than the buffer size**
   (including a ZLP completing with 0 bytes), the USB controller
   received a short packet — the message is complete. Return the
   accumulated data as one application packet.
4. When a transfer completes with **exactly the buffer size**, the
   buffer was filled without a short packet arriving — the message
   may not yet be complete. Submit another transfer and continue
   accumulating.

If the accumulated data exceeds the negotiated maximum application
packet size, the receiver **should** discard the data and report an
error.

### Implementation Note

The reference implementation uses a receive buffer size of
MPS × 128 bytes. This balances throughput against memory usage and
avoids issues with certain USB device controllers that corrupt
overly large transfers. Implementations are free to choose any
buffer size that is a positive multiple of MPS.

### Examples

Sending a 1025-byte application packet with MPS = 512:

| USB Packet | Size | Type                          |
|------------|------|-------------------------------|
| 1          | 512  | full packet                   |
| 2          | 512  | full packet                   |
| 3          | 1    | short packet (end of message) |

No ZLP is needed because the last packet is short.

Sending a 1024-byte application packet with MPS = 512:

| USB Packet | Size | Type                          |
|------------|------|-------------------------------|
| 1          | 512  | full packet                   |
| 2          | 512  | full packet                   |
| 3          | 0    | ZLP (end of message)          |

A ZLP is required because 1024 is a multiple of 512.

4\. Capabilities
----------------

Capabilities are encoded using a **TLV (tag-length-value)** format.

### TLV Encoding

Each entry consists of:

```
[tag: 1 byte] [length: 2 bytes, little-endian] [value: `length` bytes]
```

Entries are concatenated with no separators or padding. The entire
encoded payload must not exceed 256 bytes.

* Unknown tags **must** be silently skipped during decoding. This
  enables forward compatibility — new capabilities can be added in
  future versions without breaking existing implementations.
* Missing tags fall back to their default values.

### Device Capabilities

Sent by the device in response to a CAPABILITIES IN request.

| Tag    | Name              | Value Format          | Default      | Description |
|--------|-------------------|-----------------------|--------------|-------------|
| `0x01` | ping_timeout      | u32 LE (milliseconds) | 0 (disabled) | If non-zero, the device expects STATUS requests within this interval. If no STATUS request arrives in time, the device considers the host dead and closes the connection. 0 disables the timeout. Default: 10,000 ms (10 seconds) when supported. |
| `0x02` | status_supported  | u8 (0 or 1)          | 0 (false)    | Whether the device handles STATUS requests. The host must not send STATUS if this is 0. |
| `0x03` | max_size          | u64 LE                | 16,777,216   | Maximum application-level packet size the device can receive (in bytes). Default: 16 MiB. |
| `0x04` | echo_supported    | u8 (0 or 1)          | 0 (false)    | Whether the device handles ECHO requests. The host must not send ECHO if this is 0. |

### Host Capabilities

Sent by the host via a CAPABILITIES OUT request.

| Tag    | Name              | Value Format          | Default      | Description |
|--------|-------------------|-----------------------|--------------|-------------|
| `0x03` | max_size          | u64 LE                | 16,777,216   | Maximum application-level packet size the host can receive (in bytes). Default: 16 MiB. |

The effective maximum size for each direction is the minimum of
the sender's local limit and the receiver's advertised `max_size`.

5\. Status Polling and Liveness
-------------------------------

STATUS requests serve dual purposes: the host can read device status
flags, and the device uses them as a liveness signal from the host.

### Host Polling

If the device advertises `status_supported = 1`, the host **should**
periodically send STATUS requests. The recommended polling interval is:

```
interval = min(host_ping_interval, device_ping_timeout / 2)
```

Default values:
* Host ping interval: 5 seconds
* Device ping timeout: 10 seconds
* Resulting default polling interval: 5 seconds

### Device Timeout

If the device has `ping_timeout` configured and a connection is open,
it tracks when the last STATUS request was received. If `ping_timeout`
elapses without a STATUS request, the device **should** consider the
host dead and close the connection.

### Status Response

The STATUS response is a variable-length byte array (0–8 bytes) where
each byte is a status flag. An empty response means everything is
normal. Unknown status bytes **should** be ignored by the host.

6\. Connection Lifecycle
------------------------

This section describes the full sequence of events for a typical
connection. Steps marked *(optional)* may be skipped.

### Establishing a Connection (host side)

```
 Host                                        Device
  │                                            │
  │──── PROBE (optional) ─────────────────────>│
  │<─── "UPC" ────────────────────────────────│
  │                                            │
  │──── INFO (optional) ──────────────────────>│
  │<─── info bytes ───────────────────────────│
  │                                            │
  │  [claim interface, clear halt on both EPs] │
  │                                            │
  │──── CLOSE (reset previous state) ────────>│
  │                                            │
  │──── CAPABILITIES IN (optional) ───────────>│
  │<─── device capabilities (TLV) ────────────│
  │                                            │
  │──── CAPABILITIES OUT (optional) ──────────>│
  │                                            │
  │  [if echo_supported:]                      │
  │──── ECHO (marker) ────────────────────────>│
  │<─── marker via bulk IN ───────────────────│
  │  [discard stale IN data until marker seen] │
  │                                            │
  │──── OPEN (topic bytes) ───────────────────>│
  │                                            │
  │<═══ bulk data ════════════════════════════>│
  │     (framed with short packets / ZLPs)     │
  │                                            │
```

Detailed steps:

1. *(Optional)* Send **PROBE** to verify the interface speaks UPC.
2. *(Optional)* Send **INFO** to read device information.
3. Claim the USB interface. Clear halt condition on both bulk endpoints.
4. Send **CLOSE** to reset any state from a previous connection.
5. *(Optional)* Send **CAPABILITIES IN** to query device capabilities.
   On failure, use defaults.
6. *(Optional)* Send **CAPABILITIES OUT** to inform the device of host
   capabilities.
7. Flush stale data from the IN endpoint. Two methods:
   * **ECHO barrier** *(recommended, requires `echo_supported`)*: send
     **ECHO** with a random marker, then read from the IN endpoint
     until the marker is received. All stale data is deterministically
     drained. If the echo response does not arrive within a short
     timeout, generate a new random marker and resend.
   * **Timeout flush**: read and discard from the IN endpoint until a
     short timeout elapses with no data received (recommended: 100 ms)
     or the endpoint stalls. Used as fallback when the device does not
     advertise `echo_supported`.
8. Send **OPEN** with the topic payload. The connection is now active.
9. Exchange data over the bulk endpoints using the framing described
   in section 3.

### Data Exchange

Once the connection is open, the host and device exchange application
packets over the bulk endpoints. Both directions are independent and
may be active simultaneously.

If status polling is enabled (device advertises `status_supported`),
the host periodically sends STATUS requests during data exchange.

### Closing a Connection

A connection can be closed cleanly in several ways:

**Full close (host-initiated):**
The host sends CLOSE. The device stops all bulk activity and resets.

**Half-close (host-initiated):**
* Host done sending → host sends **CLOSE_SEND** with the total byte
  count. Device drains remaining OUT endpoint data and considers the
  host → device direction closed.
* Host done receiving → host sends **CLOSE_RECV**. Device stops
  sending and halts the IN endpoint.

**Half-close (device-initiated):**
* Device done sending → device halts the IN endpoint. The host detects
  the stall and considers the device → host direction closed.
* Device done receiving → device halts the OUT endpoint and includes
  **RECV_CLOSED** in STATUS responses. The host detects the stall or
  the status flag and considers the host → device direction closed.

**Full close after half-close:**
When both directions are closed (regardless of which side initiated
each half-close), the host sends a final **CLOSE** to fully tear down
the connection.

**Endpoint halt persistence:**
Endpoint halt conditions set during half-close (e.g. device halting
the IN or OUT endpoint) persist until the next connection. The host
clears halt conditions on both endpoints as part of establishing a
new connection (step 3 above).

```
 Host                                        Device
  │                                            │
  │  [host drops sender]                       │
  │──── CLOSE_SEND (total bytes) ─────────────>│
  │                                  [drains OUT endpoint]
  │                                            │
  │                       [device drops sender]│
  │              [IN endpoint halted] <────────│
  │  [detects stall, recv direction closed]    │
  │                                            │
  │  [both directions closed]                  │
  │──── CLOSE ────────────────────────────────>│
  │                                            │
```

7\. Minimal Implementation Guide
---------------------------------

A minimal UPC device needs to support only:

1. **Descriptors**: one vendor-specific interface with two bulk
   endpoints (IN and OUT).
2. **OPEN** (0x01): accept the request, prepare to exchange data.
3. **CLOSE** (0x02): stop bulk activity, reset state.
4. **Data framing**: send and receive with short-packet / ZLP
   boundaries as described in section 3.

Everything else is optional and can be added incrementally:

* Add **PROBE** (0x00) for auto-discovery support.
* Add **INFO** (0x03) to provide a device description string.
* Add **CAPABILITIES** (0x07) to negotiate max sizes.
* Add **STATUS** (0x06) for liveness detection.
* Add **CLOSE_SEND** / **CLOSE_RECV** (0x04, 0x05) for half-close.
* Add **Microsoft OS descriptors** for driverless Windows support.
