//! What USB says over the wire: the standard requests, the descriptor
//! layouts, and the hub class's ports.
//!
//! Descriptors are read out of a byte buffer at named offsets rather than
//! cast from one. A packed struct over a DMA buffer is a misaligned read
//! waiting to happen, and every field here is little-endian whatever the
//! machine is.

/* ---- device speeds (the xHCI port-speed / slot-context encoding) ---- */

pub const SPEED_INVALID: u8 = 0;
pub const SPEED_FULL: u8 = 1; /* 12 Mb/s */
pub const SPEED_LOW: u8 = 2; /* 1.5 Mb/s */
pub const SPEED_HIGH: u8 = 3; /* 480 Mb/s */
pub const SPEED_SUPER: u8 = 4; /* 5 Gb/s */
pub const SPEED_SUPER_PLUS: u8 = 5; /* 10 Gb/s */

pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        SPEED_FULL => "full",
        SPEED_LOW => "low",
        SPEED_HIGH => "high",
        SPEED_SUPER => "super",
        SPEED_SUPER_PLUS => "super+",
        _ => "unknown",
    }
}

/* ---- setup packet: bmRequestType ---- */

pub const DIR_OUT: u8 = 0x00;
pub const DIR_IN: u8 = 0x80;
pub const TYPE_STANDARD: u8 = 0x00;
pub const TYPE_CLASS: u8 = 0x20;
pub const RECIP_DEVICE: u8 = 0x00;
pub const RECIP_INTERFACE: u8 = 0x01;
pub const RECIP_OTHER: u8 = 0x03;

/* ---- standard requests ---- */

pub const REQ_CLEAR_FEATURE: u8 = 0x01;
pub const REQ_SET_FEATURE: u8 = 0x03;
pub const REQ_GET_DESCRIPTOR: u8 = 0x06;
pub const REQ_SET_CONFIGURATION: u8 = 0x09;

/* ---- HID class requests ---- */

pub const REQ_SET_PROTOCOL: u8 = 0x0B;
pub const REQ_SET_IDLE: u8 = 0x0A;
pub const HID_PROTOCOL_BOOT: u16 = 0;

/* ---- descriptor types (the high byte of wValue in GET_DESCRIPTOR) ---- */

pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIGURATION: u8 = 2;
pub const DESC_INTERFACE: u8 = 4;
pub const DESC_ENDPOINT: u8 = 5;
pub const DESC_HUB: u8 = 0x29;
pub const DESC_HUB_SUPER_SPEED: u8 = 0x2A;

/* ---- class, subclass and protocol codes this driver cares about ---- */

pub const CLASS_HID: u8 = 0x03;
pub const CLASS_HUB: u8 = 0x09;
pub const SUBCLASS_BOOT: u8 = 0x01;
pub const PROTOCOL_KEYBOARD: u8 = 0x01;

/* ---- the device descriptor, 18 bytes ---- */

pub const DEVICE_DESC_LEN: usize = 18;

pub mod device {
    /// bMaxPacketSize0
    pub fn max_packet0(d: &[u8]) -> u8 {
        d[7]
    }
    pub fn vendor_id(d: &[u8]) -> u16 {
        u16::from_le_bytes([d[8], d[9]])
    }
    pub fn product_id(d: &[u8]) -> u16 {
        u16::from_le_bytes([d[10], d[11]])
    }
    pub fn device_class(d: &[u8]) -> u8 {
        d[4]
    }
}

/* ---- the configuration descriptor, 9 bytes, and what follows it ---- */

pub const CONFIG_DESC_LEN: usize = 9;

pub mod config {
    pub fn total_length(d: &[u8]) -> u16 {
        u16::from_le_bytes([d[2], d[3]])
    }
    pub fn configuration_value(d: &[u8]) -> u8 {
        d[5]
    }
}

/// The header every descriptor in a configuration starts with: its length
/// and its type.
pub fn desc_len(d: &[u8]) -> u8 {
    d[0]
}

pub fn desc_type(d: &[u8]) -> u8 {
    d[1]
}

pub mod interface {
    pub fn number(d: &[u8]) -> u8 {
        d[2]
    }
    pub fn class(d: &[u8]) -> u8 {
        d[5]
    }
    pub fn subclass(d: &[u8]) -> u8 {
        d[6]
    }
    pub fn protocol(d: &[u8]) -> u8 {
        d[7]
    }
}

pub mod endpoint {
    pub const DIR_IN: u8 = 0x80;
    pub const NUM_MASK: u8 = 0x0F;
    pub const XFER_MASK: u8 = 0x03;
    pub const XFER_INTERRUPT: u8 = 0x03;

    pub fn address(d: &[u8]) -> u8 {
        d[2]
    }
    pub fn attributes(d: &[u8]) -> u8 {
        d[3]
    }
    pub fn max_packet(d: &[u8]) -> u16 {
        u16::from_le_bytes([d[4], d[5]])
    }
    pub fn interval(d: &[u8]) -> u8 {
        d[6]
    }
}

/* ---- the hub class: port features and status (USB 2.0 spec 11.24) ---- */

pub const HUB_FEATURE_PORT_RESET: u16 = 4;
pub const HUB_FEATURE_PORT_POWER: u16 = 8;
pub const HUB_FEATURE_C_PORT_CONNECTION: u16 = 16;
pub const HUB_FEATURE_C_PORT_ENABLE: u16 = 17;
pub const HUB_FEATURE_C_PORT_RESET: u16 = 20;
pub const HUB_FEATURE_BH_PORT_RESET: u16 = 28;
pub const HUB_FEATURE_C_BH_PORT_RESET: u16 = 29;

pub const HUB_PORT_STATUS_CONNECTION: u16 = 1 << 0;
pub const HUB_PORT_STATUS_ENABLE: u16 = 1 << 1;
pub const HUB_PORT_STATUS_RESET: u16 = 1 << 4;
pub const HUB_PORT_STATUS_LOW_SPEED: u16 = 1 << 9;
pub const HUB_PORT_STATUS_HIGH_SPEED: u16 = 1 << 10;

pub const HUB_PORT_CHANGE_CONNECTION: u16 = 1 << 0;
pub const HUB_PORT_CHANGE_RESET: u16 = 1 << 4;

/// Offsets inside the hub class descriptor, the same for 0x29 and 0x2A up to
/// bPwrOn2PwrGood.
pub const HUB_DESC_NUM_PORTS: usize = 2;
pub const HUB_DESC_CHARACTERISTICS: usize = 3;
pub const HUB_DESC_POWER_ON_DELAY: usize = 5;
pub const HUB_DESC_MIN_LENGTH: usize = 7;

/// The control endpoint's packet size before the device descriptor can be
/// read. Low and full speed must start at 8 -- the real value comes from the
/// first eight bytes of the descriptor -- high speed is fixed at 64, and
/// SuperSpeed encodes 512.
pub fn default_max_packet0(speed: u8) -> u16 {
    match speed {
        SPEED_SUPER | SPEED_SUPER_PLUS => 512,
        SPEED_HIGH => 64,
        _ => 8,
    }
}
