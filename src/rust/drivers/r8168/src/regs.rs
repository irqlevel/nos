/* RTL8111/8168 MMIO register offsets (all relative to MMIO BAR base).
 * Source: Realtek RTL8168 datasheet rev 1.5, and Linux r8169 driver. */

/* --- MAC address (read at init, write to restore after reset) --- */
pub const IDR0: usize = 0x00; /* MAC byte 0 (u8) */
pub const IDR1: usize = 0x01;
pub const IDR2: usize = 0x02;
pub const IDR3: usize = 0x03;
pub const IDR4: usize = 0x04;
pub const IDR5: usize = 0x05; /* MAC byte 5 (u8) */

/* --- Multicast filter --- */
pub const MAR0: usize = 0x08; /* 8 bytes, set to 0xFF..FF to accept all multicast */

/* --- TX descriptor ring base address --- */
pub const TNPDS:  usize = 0x20; /* Normal-Priority TX descriptor start (64-bit) */
pub const TNPDS_LO: usize = 0x20;
pub const TNPDS_HI: usize = 0x24;

/* --- TX/RX control --- */
pub const CMD_REG:  usize = 0x37; /* u8: bit4=RST, bit3=TE, bit2=RE */
pub const TX_POLL:  usize = 0x38; /* u8: bit6=NPQ (Normal Priority Queue start) */
pub const CFG9346:  usize = 0x50; /* u8: 0xC0=unlock, 0x00=lock */

/* --- Interrupt registers --- */
pub const INTR_MASK:   usize = 0x3C; /* u16: interrupt enable mask */
pub const INTR_STATUS: usize = 0x3E; /* u16: interrupt status (W1C) */

/* --- TX configuration --- */
pub const TX_CONFIG: usize = 0x40; /* u32 */

/* --- RX configuration --- */
pub const RX_CONFIG: usize = 0x44; /* u32 */

/* --- PHY access --- */
pub const PHYAR: usize = 0x60; /* u32: PHY register access */

/* --- Timer --- */
pub const TIMER_INT: usize = 0x54; /* u32: timer interrupt interval */

/* --- RX descriptor ring base address --- */
pub const RDSAR_LO: usize = 0xE4; /* u32: RX descriptor start address low */
pub const RDSAR_HI: usize = 0xE8; /* u32: RX descriptor start address high */

/* --- Misc --- */
pub const MAX_TX_PKT_SIZE: usize = 0xEC; /* u8: max TX packet size / 128 */
pub const CPCR: usize = 0xE0; /* u16: C+ command register */
pub const RX_MAX_SIZE: usize = 0xDA; /* u16: max RX packet size */
pub const ETH_ERR_CNT: usize = 0xC0; /* u8 */

/* ================================================================== */
/* CMD_REG bits */
pub const CMD_RESET:    u8 = 1 << 4; /* soft reset -- self-clearing */
pub const CMD_TX_EN:    u8 = 1 << 3; /* enable TX DMA */
pub const CMD_RX_EN:    u8 = 1 << 2; /* enable RX DMA */

/* TX_POLL bits */
pub const TX_POLL_NPQ: u8 = 1 << 6; /* kick normal-priority TX queue */

/* ================================================================== */
/* Interrupt status / mask bits (u16) */
pub const ISR_ROK:        u16 = 1 << 0;  /* RX OK */
pub const ISR_RER:        u16 = 1 << 1;  /* RX error */
pub const ISR_TOK:        u16 = 1 << 2;  /* TX OK */
pub const ISR_TER:        u16 = 1 << 3;  /* TX error */
pub const ISR_RX_OVFLOW:  u16 = 1 << 4;  /* RX overflow */
pub const ISR_LINK_CHG:   u16 = 1 << 5;  /* Link status change */
pub const ISR_RX_FIFO_OV: u16 = 1 << 6;  /* RX FIFO overflow */
pub const ISR_TDU:        u16 = 1 << 7;  /* TX descriptor unavailable */
pub const ISR_SW:         u16 = 1 << 8;  /* software interrupt */
pub const ISR_TIMEOUT:    u16 = 1 << 14; /* timer interrupt */
pub const ISR_SYS_ERR:    u16 = 1 << 15; /* fatal PCI bus error */

/* Enabled interrupt sources */
pub const INTR_MASK_BITS: u16 = ISR_ROK | ISR_RER | ISR_TOK | ISR_TER
    | ISR_RX_OVFLOW | ISR_LINK_CHG | ISR_RX_FIFO_OV | ISR_SYS_ERR;

/* ================================================================== */
/* TX descriptor opts1 bits */
pub const TX_OWN:    u32 = 1 << 31; /* owned by hardware */
pub const TX_EOR:    u32 = 1 << 30; /* end of ring -- wrap to start */
pub const TX_FS:     u32 = 1 << 29; /* first segment */
pub const TX_LS:     u32 = 1 << 28; /* last segment */
pub const TX_LEN_MASK: u32 = 0x0000_FFFF; /* frame length in bits 15:0 */

/* RX descriptor opts1 bits */
pub const RX_OWN:    u32 = 1 << 31; /* owned by hardware */
pub const RX_EOR:    u32 = 1 << 30; /* end of ring */
pub const RX_MAR:    u32 = 1 << 26; /* multicast frame */
pub const RX_PAM:    u32 = 1 << 25; /* physical address match */
pub const RX_BAR:    u32 = 1 << 24; /* broadcast */
pub const RX_LEN_MASK: u32 = 0x0000_3FFF; /* received frame length in bits 13:0 */

/* ================================================================== */
/* TxConfig register bits */
/* IFG (Inter-Frame Gap) = 11b = 96 bit-times (802.3 standard) */
pub const TX_CONFIG_IFG: u32 = 0x0300_0000;
/* DMA burst size = 1024 bytes (bits 10:8 = 111) */
pub const TX_CONFIG_DMA_BURST: u32 = 0x0000_0700;
pub const TX_CONFIG_VAL: u32 = TX_CONFIG_IFG | TX_CONFIG_DMA_BURST;

/* ================================================================== */
/* RxConfig register bits */
/* Accept broadcast, multicast, physical address match packets */
pub const RX_CFG_AAP:  u32 = 1 << 0; /* accept all physical unicast */
pub const RX_CFG_APM:  u32 = 1 << 1; /* accept physical address match */
pub const RX_CFG_AM:   u32 = 1 << 2; /* accept multicast */
pub const RX_CFG_AB:   u32 = 1 << 3; /* accept broadcast */
/* DMA burst: 1024 bytes (bits 10:8 = 111) */
pub const RX_CFG_DMA_BURST: u32 = 0x0000_0700;
/* RX FIFO threshold: none (bits 15:13 = 111) */
pub const RX_CFG_RXFTH: u32 = 0x0000_E000;
/* No RX buffer size wrapping (bit 7=0) */
pub const RX_CONFIG_VAL: u32 = RX_CFG_APM | RX_CFG_AM | RX_CFG_AB
    | RX_CFG_DMA_BURST | RX_CFG_RXFTH;

/* ================================================================== */
/* C+ command register (CPCR) bits */
pub const CPCR_RX_VLAN: u16 = 1 << 6; /* RX VLAN de-tagging */
pub const CPCR_RX_CHKSUM: u16 = 1 << 5; /* RX checksum offload */
pub const CPCR_DAC: u16 = 1 << 4; /* dual address cycle (64-bit DMA) */
pub const CPCR_MULRW: u16 = 1 << 3; /* PCI multiple read/write */
/* Enable 64-bit DMA addressing and multiple R/W */
pub const CPCR_VAL: u16 = CPCR_DAC | CPCR_MULRW;

/* ================================================================== */
/* Misc */
pub const CFG9346_UNLOCK: u8 = 0xC0;
pub const CFG9346_LOCK:   u8 = 0x00;

/* RTL8168 PCI IDs */
pub const PCI_VENDOR_REALTEK: u16 = 0x10EC;
pub const PCI_DEVICE_RTL8168: u16 = 0x8168;

/* Max TX packet size field value: (0x3B + 1) * 128 = 7680 bytes */
pub const MAX_TX_PKT_VAL: u8 = 0x3B;

/* RX frame buffer size per descriptor */
pub const RX_BUF_SIZE: usize = 2048;
