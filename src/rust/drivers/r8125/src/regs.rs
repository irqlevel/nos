/* RTL8125 (2.5GbE) MMIO register offsets, all relative to the BAR base.
 *
 * The RTL8125 keeps the RTL8168 descriptor rings and DMA engines, and moves
 * three things that silently break a driver written for the older chip:
 *   - interrupt mask/status are 32-bit at 0x38/0x3C, not 16-bit at 0x3C/0x3E
 *     (an 8168 driver would write its mask straight into the status register)
 *   - the TX doorbell is a 16-bit write of bit 0 to 0x90, not NPQ to 0x38
 *   - RSS and the multi-queue block exist and must be switched off, or frames
 *     land in queues this driver never looks at
 *
 * The register file is 64 KiB, not the classic 256 bytes: the coalescing,
 * queue, RSS and EEE blocks all live above 0x100, so the whole BAR is mapped.
 *
 * Offsets, bit values and the bring-up sequence in hw.rs are the hardware
 * programming facts documented by the Linux r8169 driver
 * (drivers/net/ethernet/realtek/r8169_main.c); the code here is our own. */

/* --- Station address ---
 * IDR0..5 are the receive-address registers the unicast filter matches
 * against.  They are not a trustworthy *source* for the address on this
 * chip: the copy loaded from the eFuse lives in a backup block high in the
 * register file, and the driver is expected to read it from there and
 * program the RAR itself (see read_mac/write_mac in lib.rs). */
pub const IDR0: usize = 0x00; /* also the 32-bit RAR low half */
pub const IDR4: usize = 0x04; /* also the 32-bit RAR high half */
pub const MAC0_BKP: usize = 0x19E0; /* 6 bytes, eFuse copy */

/* --- Multicast hash filter: 8 bytes, all-ones accepts every group --- */
pub const MAR0: usize = 0x08;

/* --- TX descriptor ring base (normal priority queue) --- */
pub const TNPDS_LO: usize = 0x20;
pub const TNPDS_HI: usize = 0x24;

/* --- Command register --- */
pub const CMD_REG: usize = 0x37; /* u8 */

/* --- Interrupts: 32-bit on this chip --- */
pub const INTR_MASK: usize = 0x38; /* u32 */
pub const INTR_STATUS: usize = 0x3C; /* u32, write-1-to-clear */

/* --- DMA engine configuration --- */
pub const TX_CONFIG: usize = 0x40; /* u32 */
pub const RX_CONFIG: usize = 0x44; /* u32 */

/* --- Config register lock and the config block itself --- */
pub const CFG9346: usize = 0x50; /* u8 */
pub const CONFIG1: usize = 0x52; /* u8 */
pub const CONFIG2: usize = 0x53; /* u8 */
pub const CONFIG3: usize = 0x54; /* u8 */
pub const CONFIG5: usize = 0x56; /* u8 */

/* --- PHY link state (read-only) --- */
pub const PHY_STATUS: usize = 0x6C; /* u8 */

/* --- TX doorbell (8125: 16-bit, bit 0) --- */
pub const TX_POLL: usize = 0x90; /* u16 */

/* --- MAC OCP window: the chip's internal register file --- */
pub const OCPDR: usize = 0xB0; /* u32, see hw.rs mac_ocp_* */

/* --- MCU state: OOB handoff and FIFO-drained flags --- */
pub const MCU: usize = 0xD3; /* u8 */

/* --- Frame size filter and RX ring base --- */
pub const RX_MAX_SIZE: usize = 0xDA; /* u16 */
pub const INTR_MITIGATE: usize = 0xE2; /* u16: on 8125 also a drain flag */
pub const RDSAR_LO: usize = 0xE4;
pub const RDSAR_HI: usize = 0xE8;
pub const MISC: usize = 0xF0; /* u32 */

/* --- Blocks that only exist on the 8125 --- */
/* Interrupt coalescing timers, zeroed as a block at bring-up. */
pub const COALESCE_BASE: usize = 0x0A00;
pub const COALESCE_END: usize = 0x0B00;
/* Undocumented, written verbatim by every vendor bring-up sequence. */
pub const MAGIC_0382: usize = 0x0382; /* u16 <- 0x221B */
pub const MAGIC_1880: usize = 0x1880; /* u16, bits 5:4 cleared */
pub const RSS_CTRL: usize = 0x4500; /* u8: 0 = RSS off */
pub const Q_NUM_CTRL: usize = 0x4800; /* u16: 0 = single queue pair */
pub const EEE_TXIDLE_TIMER: usize = 0x6048; /* u16 (8125B) */

/* Highest register byte this driver touches; the BAR mapping must cover it. */
pub const REG_SPACE_USED: usize = EEE_TXIDLE_TIMER + 2;

/* ================================================================== */
/* CMD_REG (0x37) bits */
pub const CMD_STOP_REQ: u8 = 0x80; /* ask the FIFOs to drain (8125B) */
pub const CMD_RESET: u8 = 0x10; /* soft reset -- self-clearing */
pub const CMD_RX_EN: u8 = 0x08;
pub const CMD_TX_EN: u8 = 0x04;

/* TX_POLL (0x90) bits */
pub const TX_POLL_KICK: u16 = 1 << 0;

/* MCU (0xD3) bits */
pub const MCU_NOW_IS_OOB: u8 = 1 << 7; /* firmware still owns the MAC */
pub const MCU_TX_EMPTY: u8 = 1 << 5;
pub const MCU_RX_EMPTY: u8 = 1 << 4;
pub const MCU_RXTX_EMPTY: u8 = MCU_TX_EMPTY | MCU_RX_EMPTY;
pub const MCU_LINK_LIST_RDY: u8 = 1 << 1;

/* MISC (0xF0) bits */
pub const MISC_RXDV_GATED_EN: u32 = 1 << 19; /* gate the RX datapath */

/* INTR_MITIGATE (0xE2): doubles as a "TX/RX drained" flag on the 8125B */
pub const MITIGATE_DRAINED: u16 = 0x0103;

/* CFG9346 (0x50) */
pub const CFG9346_UNLOCK: u8 = 0xC0;
pub const CFG9346_LOCK: u8 = 0x00;

/* Config register bits used during bring-up */
pub const CONFIG1_SPEED_DOWN: u8 = 1 << 4;
pub const CONFIG2_CLKREQ_EN: u8 = 1 << 7;
pub const CONFIG3_RDY_TO_L23: u8 = 1 << 1;
pub const CONFIG5_ASPM_EN: u8 = 1 << 0;

/* PHY_STATUS (0x6C) bits */
pub const PHY_LINK_UP: u8 = 1 << 1;
pub const PHY_FULL_DUPLEX: u8 = 1 << 0;

/* ================================================================== */
/* Interrupt status / mask bits.  Same meanings as the 8168's 16-bit
 * register, in a 32-bit register; the upper half is per-queue and unused
 * while the chip runs with a single queue pair. */
pub const ISR_ROK: u32 = 1 << 0; /* RX OK */
pub const ISR_RER: u32 = 1 << 1; /* RX error */
pub const ISR_TOK: u32 = 1 << 2; /* TX OK */
pub const ISR_TER: u32 = 1 << 3; /* TX error */
pub const ISR_RX_OVERFLOW: u32 = 1 << 4;
pub const ISR_LINK_CHG: u32 = 1 << 5;
pub const ISR_RX_FIFO_OVER: u32 = 1 << 6;
pub const ISR_TDU: u32 = 1 << 7; /* TX descriptor unavailable */
pub const ISR_SYS_ERR: u32 = 1 << 15; /* fatal bus error */

/* What the chip is asked to report.  SYSErr is deliberately not enabled:
 * on this generation it fires spuriously and the vendor driver leaves it
 * masked from the 8168 onwards. */
pub const INTR_MASK_BITS: u32 =
    ISR_ROK | ISR_RER | ISR_TOK | ISR_TER | ISR_LINK_CHG | ISR_RX_OVERFLOW;

/* ================================================================== */
/* TX descriptor opts1 bits (legacy 16-byte format; hw.rs clears the
 * chip's "new TX descriptor format" bit so this layout stays valid) */
pub const TX_OWN: u32 = 1 << 31;
pub const TX_EOR: u32 = 1 << 30;
pub const TX_FS: u32 = 1 << 29;
pub const TX_LS: u32 = 1 << 28;
pub const TX_LEN_MASK: u32 = 0x0000_FFFF;

/* RX descriptor opts1 bits */
pub const RX_OWN: u32 = 1 << 31;
pub const RX_EOR: u32 = 1 << 30;
pub const RX_FF: u32 = 1 << 29; /* first fragment */
pub const RX_LF: u32 = 1 << 28; /* last fragment */
pub const RX_RWT: u32 = 1 << 22; /* receive watchdog expired */
pub const RX_RES: u32 = 1 << 21; /* error summary */
pub const RX_RUNT: u32 = 1 << 20;
pub const RX_CRC: u32 = 1 << 19;
pub const RX_ERR_MASK: u32 = RX_RWT | RX_RES | RX_RUNT | RX_CRC;
pub const RX_LEN_MASK: u32 = 0x0000_3FFF;

/* ================================================================== */
/* TX_CONFIG (0x40): unlimited DMA burst, 802.3 inter-frame gap */
pub const TX_CFG_DMA_BURST: u32 = 7 << 8;
pub const TX_CFG_IFG: u32 = 3 << 24;
pub const TX_CONFIG_VAL: u32 = TX_CFG_IFG | TX_CFG_DMA_BURST;

/* The XID lives in the top of TX_CONFIG and identifies the chip revision. */
pub const TX_CONFIG_XID_SHIFT: u32 = 20;
pub const TX_CONFIG_XID_MASK: u32 = 0xFCF;
pub const XID_MATCH_MASK: u32 = 0x7CF;
pub const XID_RTL8125A: u32 = 0x609;
pub const XID_RTL8125B: u32 = 0x641;

/* ================================================================== */
/* RX_CONFIG (0x44) */
/* Bit 0 (accept all physical = promiscuous) is deliberately never set. */
pub const RX_CFG_ACCEPT_MY_PHYS: u32 = 1 << 1;
pub const RX_CFG_ACCEPT_MULTICAST: u32 = 1 << 2;
pub const RX_CFG_ACCEPT_BROADCAST: u32 = 1 << 3;
pub const RX_CFG_ACCEPT_MASK: u32 = 0x3F; /* incl. the two error-accept bits */
pub const RX_CFG_DMA_BURST: u32 = 7 << 8; /* unlimited */
pub const RX_CFG_PAUSE_SLOT_ON: u32 = 1 << 11; /* 8125B and later */
pub const RX_CFG_FETCH_DFLT: u32 = 8 << 27; /* descriptor prefetch depth */

/* What this driver accepts: unicast to us, broadcast and all multicast. */
pub const RX_ACCEPT_BITS: u32 =
    RX_CFG_ACCEPT_MY_PHYS | RX_CFG_ACCEPT_MULTICAST | RX_CFG_ACCEPT_BROADCAST;

/* ================================================================== */
/* MAC OCP access encoding (register OCPDR) */
pub const OCP_WRITE_FLAG: u32 = 0x8000_0000;
pub const OCP_REG_SHIFT: u32 = 15;

/* ================================================================== */
/* PCI identity */
pub const PCI_VENDOR_REALTEK: u16 = 0x10EC;
pub const PCI_DEVICE_RTL8125: u16 = 0x8125;

/* Per-descriptor RX buffer.  One frame per descriptor: RX_MAX_SIZE_VAL keeps
 * the chip from ever splitting a frame across two of them. */
pub const RX_BUF_SIZE: usize = 2048;
/* 1526 = 1518-byte Ethernet frame + VLAN tag + slack, as on the 8168. */
pub const RX_MAX_SIZE_VAL: u16 = 0x05F6;

/* EEE TX idle timer (8125B): MTU + Ethernet header + 0x20, per the vendor
 * driver's formula for a 1500-byte MTU. */
pub const EEE_TXIDLE_VAL: u16 = 1500 + 14 + 0x20;
