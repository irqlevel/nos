//! The xHCI register map (xHCI 1.2, chapter 5) and the TRB encoding
//! (chapter 6).

/* ---- capability registers, at BAR0 + 0 ---- */

pub const CAP_CAPLENGTH: usize = 0x00; /* u8 CAPLENGTH, u16 HCIVERSION at +2 */
pub const CAP_HCSPARAMS1: usize = 0x04;
pub const CAP_HCSPARAMS2: usize = 0x08;
pub const CAP_HCCPARAMS1: usize = 0x10;
pub const CAP_DBOFF: usize = 0x14;
pub const CAP_RTSOFF: usize = 0x18;

/* ---- operational registers, at BAR0 + CAPLENGTH ---- */

pub const OP_USBCMD: usize = 0x00;
pub const OP_USBSTS: usize = 0x04;
pub const OP_PAGESIZE: usize = 0x08;
pub const OP_DNCTRL: usize = 0x14;
pub const OP_CRCR: usize = 0x18;
pub const OP_DCBAAP: usize = 0x30;
pub const OP_CONFIG: usize = 0x38;
pub const OP_PORTSC_BASE: usize = 0x400;
pub const OP_PORT_REG_SIZE: usize = 0x10;

pub const USBCMD_RUN: u32 = 1 << 0;
pub const USBCMD_RESET: u32 = 1 << 1;
pub const USBCMD_INT_ENABLE: u32 = 1 << 2;
pub const USBCMD_HS_ERR_ENABLE: u32 = 1 << 3;

pub const USBSTS_HALTED: u32 = 1 << 0;
pub const USBSTS_HOST_SYSTEM_ERROR: u32 = 1 << 2;
pub const USBSTS_EVENT_INT: u32 = 1 << 3;
pub const USBSTS_PORT_CHANGE: u32 = 1 << 4;
pub const USBSTS_CONTROLLER_NOT_READY: u32 = 1 << 11;
pub const USBSTS_HOST_CONTROLLER_ERROR: u32 = 1 << 12;

/* ---- runtime registers, at BAR0 + RTSOFF; interrupter 0 at +0x20 ---- */

pub const RT_INTERRUPTER0: usize = 0x20;
pub const IR_IMAN: usize = 0x00;
pub const IR_IMOD: usize = 0x04;
pub const IR_ERSTSZ: usize = 0x08;
pub const IR_ERSTBA: usize = 0x10;
pub const IR_ERDP: usize = 0x18;

pub const IMAN_INTERRUPT_PENDING: u32 = 1 << 0;
pub const IMAN_INTERRUPT_ENABLE: u32 = 1 << 1;
pub const ERDP_EVENT_HANDLER_BUSY: u64 = 1 << 3;

/* ---- PORTSC ---- */

pub const PORTSC_CONNECTED: u32 = 1 << 0;
pub const PORTSC_ENABLED: u32 = 1 << 1;
pub const PORTSC_RESET: u32 = 1 << 4;
pub const PORTSC_POWER: u32 = 1 << 9;
pub const PORTSC_LINK_WRITE_STROBE: u32 = 1 << 16;
pub const PORTSC_RESET_CHANGE: u32 = 1 << 21;
pub const PORTSC_WARM_RESET: u32 = 1 << 31;
/// CSC, PEC, WRC, OCC, PRC, PLC, CEC -- all write-1-to-clear.
pub const PORTSC_CHANGE_MASK: u32 = 0x00FE_0000;

pub const PORTSC_SPEED_SHIFT: u32 = 10;
pub const PORTSC_SPEED_MASK: u32 = 0xF;

/* ---- CRCR ---- */

pub const CRCR_RING_CYCLE_STATE: u64 = 1 << 0;

/* ---- extended capabilities ---- */

pub const EXT_CAP_LEGACY_SUPPORT: u32 = 1;

pub const LEGACY_BIOS_OWNED: u32 = 1 << 16;
pub const LEGACY_OS_OWNED: u32 = 1 << 24;

/// USBLEGCTLSTS. Masking down to this -- `(0x7 << 1) | (0xff << 5) |
/// (0x7 << 17)`, the same set Linux keeps -- clears every SMI enable bit in
/// one write.
pub const LEGACY_CTL_KEEP_MASK: u32 = 0x000E_1FEE;
/// And this acknowledges the RW1C SMI status bits, so firmware stops
/// re-entering SMM on our register accesses.
pub const LEGACY_CTL_ACK_SMI_MASK: u32 = 0xE000_0000;

/* ---- endpoint types (xHCI 1.2 table 6-9) ---- */

pub const EP_TYPE_CONTROL: u32 = 4;
pub const EP_TYPE_INTERRUPT_IN: u32 = 7;

/* ---- Setup Stage transfer type ---- */

pub const TRT_NO_DATA: u32 = 0;
pub const TRT_OUT_DATA: u32 = 2;
pub const TRT_IN_DATA: u32 = 3;

/* ---- TRB types (xHCI 1.2 table 6-91) ---- */

pub const TRB_NORMAL: u32 = 1;
pub const TRB_SETUP_STAGE: u32 = 2;
pub const TRB_DATA_STAGE: u32 = 3;
pub const TRB_STATUS_STAGE: u32 = 4;
pub const TRB_LINK: u32 = 6;
pub const TRB_ENABLE_SLOT: u32 = 9;
pub const TRB_DISABLE_SLOT: u32 = 10;
pub const TRB_ADDRESS_DEVICE: u32 = 11;
pub const TRB_CONFIGURE_ENDPOINT: u32 = 12;
pub const TRB_EVALUATE_CONTEXT: u32 = 13;
pub const TRB_RESET_ENDPOINT: u32 = 14;
pub const TRB_STOP_ENDPOINT: u32 = 15;
pub const TRB_SET_TR_DEQUEUE: u32 = 16;
pub const TRB_NO_OP_COMMAND: u32 = 23;
pub const TRB_TRANSFER_EVENT: u32 = 32;
pub const TRB_COMMAND_COMPLETION: u32 = 33;
pub const TRB_PORT_STATUS_CHANGE: u32 = 34;

/* ---- completion codes (xHCI 1.2 table 6-90) ---- */

pub const COMP_INVALID: u32 = 0;
pub const COMP_SUCCESS: u32 = 1;
pub const COMP_SHORT_PACKET: u32 = 13;
pub const COMP_COMMAND_RING_STOPPED: u32 = 24;

/* ---- TRB control-word fields ---- */

pub const TRB_CYCLE: u32 = 1 << 0;
pub const TRB_ISP: u32 = 1 << 2;
pub const TRB_CHAIN: u32 = 1 << 4;
pub const TRB_IOC: u32 = 1 << 5;
/// Immediate Data, in a Setup Stage TRB.
pub const TRB_IDT: u32 = 1 << 6;
/// Link TRB only.
pub const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
/// Data and Status Stage only.
pub const TRB_DIR_IN: u32 = 1 << 16;

pub const fn trb_type_field(kind: u32) -> u32 {
    (kind & 0x3F) << 10
}

pub const fn trb_type_of(control: u32) -> u32 {
    (control >> 10) & 0x3F
}

pub const fn trb_slot_field(slot: u8) -> u32 {
    (slot as u32) << 24
}

pub const fn trb_slot_of(control: u32) -> u8 {
    ((control >> 24) & 0xFF) as u8
}

pub const fn trb_endpoint_of(control: u32) -> u8 {
    ((control >> 16) & 0x1F) as u8
}

pub const fn trb_completion_of(status: u32) -> u32 {
    (status >> 24) & 0xFF
}

pub const fn trb_residual_of(status: u32) -> u32 {
    status & 0xFF_FFFF
}
