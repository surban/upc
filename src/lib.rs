#![doc = include_str!("../README.md")]
#![deny(unsafe_code)]
#![warn(missing_docs)]

#[cfg(any(feature = "host", feature = "device", feature = "web"))]
use std::{
    io::{Error, ErrorKind},
    time::Duration,
};

#[cfg(feature = "device")]
pub mod device;

#[cfg(any(feature = "host", feature = "web"))]
pub mod host;

mod trace;

/// Maximum info size.
pub const INFO_SIZE: usize = 4096;

/// Default maximum packet size.
pub const MAX_SIZE: usize = 16_777_216;

/// Maximum USB packets per transfer.
///
/// This limits the number of USB packets per IN and OUT transfer.
/// Setting this too high will cause issues with UDCs like dwc2;
/// it sometimes corrupts transfers if it has to split them up.
#[doc(hidden)]
pub const TRANSFER_PACKETS: usize = 128;

/// Length of send and receive queues.
const QUEUE_LEN: usize = 32;

/// USB interface class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class {
    /// Class code.
    pub class: u8,
    /// Subclass code.
    pub sub_class: u8,
    /// Protocol code.
    pub protocol: u8,
}

impl Class {
    /// Vendor specific class code.
    pub const VENDOR_SPECIFIC: u8 = 0xff;

    /// Creates a new USB device or interface class.
    pub const fn new(class: u8, sub_class: u8, protocol: u8) -> Self {
        Self { class, sub_class, protocol }
    }

    /// Creates a new USB device or interface class with vendor-specific class code.
    pub const fn vendor_specific(sub_class: u8, protocol: u8) -> Self {
        Self::new(Self::VENDOR_SPECIFIC, sub_class, protocol)
    }
}

#[cfg(feature = "device")]
impl From<Class> for usb_gadget::Class {
    fn from(Class { class, sub_class, protocol }: Class) -> Self {
        usb_gadget::Class { class, sub_class, protocol }
    }
}

/// Vendor-specific control requests (host → device).
#[allow(dead_code)]
mod ctrl_req {
    /// Probe whether the interface speaks UPC (device responds with [`PROBE_RESPONSE`]).
    pub const PROBE: u8 = 0;
    /// Open a connection.
    pub const OPEN: u8 = 1;
    /// Close the connection (both directions).
    pub const CLOSE: u8 = 2;
    /// Read device-provided information.
    pub const INFO: u8 = 3;
    /// Host is done sending (close send direction).
    pub const CLOSE_SEND: u8 = 4;
    /// Host is done receiving (close receive direction).
    pub const CLOSE_RECV: u8 = 5;
    /// Ping / status request (device responds with status bytes).
    pub const STATUS: u8 = 6;
    /// Query device capabilities (device-to-host) / set host capabilities (host-to-device).
    pub const CAPABILITIES: u8 = 7;

    /// Expected response to a PROBE request.
    pub const PROBE_RESPONSE: &[u8] = b"UPC";
}

/// Status response bytes returned by the device in reply to a STATUS control request.
#[allow(dead_code)]
mod status {
    /// Device receiver has been dropped (device done receiving, OUT direction closed).
    pub const RECV_CLOSED: u8 = 1;
    /// Maximum size of the status response.
    pub const MAX_SIZE: usize = 8;
}

/// TLV (tag-length-value) encoding helpers for capabilities.
#[allow(dead_code)]
mod tlv {
    use std::io::{Error, ErrorKind};

    /// Appends a TLV entry to the buffer.
    pub fn encode(buf: &mut Vec<u8>, tag: u8, value: &[u8]) {
        buf.push(tag);
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        buf.extend_from_slice(value);
    }

    /// Decodes TLV entries from a byte slice.
    ///
    /// Returns a list of `(tag, value)` pairs. Unknown tags are preserved
    /// so callers can skip them for forward compatibility.
    pub fn decode(data: &[u8]) -> std::io::Result<Vec<(u8, &[u8])>> {
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            if pos + 3 > data.len() {
                return Err(Error::new(ErrorKind::InvalidData, "capabilities data truncated"));
            }
            let tag = data[pos];
            let len = u16::from_le_bytes([data[pos + 1], data[pos + 2]]) as usize;
            pos += 3;
            if pos + len > data.len() {
                return Err(Error::new(ErrorKind::InvalidData, "capabilities data truncated"));
            }
            entries.push((tag, &data[pos..pos + len]));
            pos += len;
        }
        Ok(entries)
    }
}

/// Device capabilities.
///
/// Encoded using a TLV (tag-length-value) format for forward and backward
/// compatibility: unknown tags are silently skipped during decoding and
/// missing tags fall back to their default values.
#[cfg(any(feature = "host", feature = "device", feature = "web"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceCapabilities {
    /// Ping timeout duration.
    pub ping_timeout: Option<Duration>,
    /// Whether the device supports the STATUS control request.
    pub status_supported: bool,
    /// Maximum receive size.
    pub max_size: u64,
}

#[cfg(any(feature = "host", feature = "device", feature = "web"))]
impl Default for DeviceCapabilities {
    fn default() -> Self {
        Self { ping_timeout: None, status_supported: false, max_size: MAX_SIZE as u64 }
    }
}

#[cfg(any(feature = "host", feature = "device", feature = "web"))]
impl DeviceCapabilities {
    /// Maximum encoded capabilities size.
    #[cfg(any(feature = "host", feature = "web"))]
    const SIZE: usize = 256;

    /// TLV tag for ping timeout.
    const TAG_PING_TIMEOUT: u8 = 0x01;
    /// TLV tag for status supported.
    const TAG_STATUS_SUPPORTED: u8 = 0x02;
    /// TLV tag for max packet size.
    const TAG_MAX_PACKET_SIZE: u8 = 0x03;

    /// Encodes the capabilities into a byte vector using TLV encoding.
    #[cfg(feature = "device")]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Tag 0x01: ping_timeout as u32 millis (0 = None).
        let millis: u32 = self.ping_timeout.map_or(0, |d| d.as_millis().try_into().unwrap_or(u32::MAX));
        tlv::encode(&mut buf, Self::TAG_PING_TIMEOUT, &millis.to_le_bytes());

        // Tag 0x02: status_supported as u8 (0 = false, 1 = true).
        tlv::encode(&mut buf, Self::TAG_STATUS_SUPPORTED, &[u8::from(self.status_supported)]);

        // Tag 0x03: max_size as u64.
        tlv::encode(&mut buf, Self::TAG_MAX_PACKET_SIZE, &self.max_size.to_le_bytes());

        buf
    }

    /// Decodes capabilities from a TLV-encoded byte slice.
    ///
    /// Unknown tags are skipped. Missing tags keep their default values.
    #[cfg(any(feature = "host", feature = "web"))]
    pub fn decode(data: &[u8]) -> std::io::Result<Self> {
        let mut caps = Self::default();
        for (tag, value) in tlv::decode(data)? {
            match tag {
                Self::TAG_PING_TIMEOUT => {
                    if value.len() >= 4 {
                        let millis = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                        caps.ping_timeout =
                            if millis == 0 { None } else { Some(Duration::from_millis(millis.into())) };
                    }
                }

                Self::TAG_STATUS_SUPPORTED => {
                    if !value.is_empty() {
                        caps.status_supported = value[0] != 0;
                    }
                }

                Self::TAG_MAX_PACKET_SIZE => {
                    if value.len() >= 8 {
                        let size = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
                        ]);
                        caps.max_size = size;
                    }
                }

                _ => { /* unknown tag — skip for forward compatibility */ }
            }
        }
        Ok(caps)
    }
}

/// Host capabilities.
///
/// Encoded using a TLV (tag-length-value) format for forward and backward
/// compatibility: unknown tags are silently skipped during decoding and
/// missing tags fall back to their default values.
#[cfg(any(feature = "host", feature = "device", feature = "web"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostCapabilities {
    /// Maximum receive size.
    pub max_size: u64,
}

#[cfg(any(feature = "host", feature = "device", feature = "web"))]
impl Default for HostCapabilities {
    fn default() -> Self {
        Self { max_size: MAX_SIZE as u64 }
    }
}

#[cfg(any(feature = "host", feature = "device", feature = "web"))]
impl HostCapabilities {
    /// Maximum encoded capabilities size.
    #[allow(dead_code)]
    #[cfg(feature = "device")]
    const SIZE: usize = 256;

    /// TLV tag for max packet size.
    const TAG_MAX_PACKET_SIZE: u8 = 0x03;

    /// Encodes the capabilities into a byte vector using TLV encoding.
    #[cfg(any(feature = "host", feature = "web"))]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        tlv::encode(&mut buf, Self::TAG_MAX_PACKET_SIZE, &self.max_size.to_le_bytes());
        buf
    }

    /// Decodes capabilities from a TLV-encoded byte slice.
    ///
    /// Unknown tags are skipped. Missing tags keep their default values.
    #[cfg(feature = "device")]
    pub fn decode(data: &[u8]) -> std::io::Result<Self> {
        let mut caps = Self::default();
        for (tag, value) in tlv::decode(data)? {
            match tag {
                Self::TAG_MAX_PACKET_SIZE => {
                    if value.len() >= 8 {
                        let size = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
                        ]);
                        caps.max_size = size;
                    }
                }
                _ => { /* unknown tag — skip for forward compatibility */ }
            }
        }
        Ok(caps)
    }
}

/// Creates an I/O error for a closed UPC channel.
///
/// For [`ErrorKind::BrokenPipe`] this produces "UPC channel closed";
/// for other kinds it uses the standard error text.
#[cfg(any(feature = "host", feature = "device", feature = "web"))]
pub(crate) fn channel_error(kind: ErrorKind) -> Error {
    if kind == ErrorKind::BrokenPipe {
        Error::new(kind, "UPC channel closed")
    } else {
        kind.into()
    }
}
