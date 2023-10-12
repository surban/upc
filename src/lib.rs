//! USB packet library

#[cfg(feature = "device")]
pub mod device;

#[cfg(feature = "host")]
pub mod host;

const CTRL_REQ_OPEN: u8 = 1;
const CTRL_REQ_CLOSE: u8 = 2;
const CTRL_REQ_INFO: u8 = 3;

const BUFFER_SIZE: usize = 65_536;

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
