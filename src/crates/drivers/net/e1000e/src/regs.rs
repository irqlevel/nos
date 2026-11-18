/* Register map for the Intel 82574L (the e1000e family's 82574).
 *
 * Offsets and bits are the 82574 datasheet's (Intel 316080) and, where the
 * datasheet says "must be set", the values Linux's e1000e writes for the
 * part: drivers/net/ethernet/intel/e1000e -- regs.h, defines.h, 82571.h
 * and the 82574 paths of 82571.c and netdev.c, which this driver follows.
 * Where a value is a workaround rather than a documented field, the comment
 * at its use names the function it comes from.
 *
 * The family differs from igb next door in three ways that matter here: the
 * interrupt block is the old ICR/IMS one, with the 82574's own MSI-X glue
 * (IVAR, EIAC, a throttle per vector) bolted onto it; the queue registers
 * sit at 0x2800/0x3800 rather than 0xC000/0xE000; and the descriptors are
 * the legacy ones, not the advanced layout. */

/* ---- General ---- */
pub const CTRL: usize = 0x00000; /* Device Control */
pub const STATUS: usize = 0x00008; /* Device Status */
pub const EECD: usize = 0x00010; /* EEPROM/Flash Control */
pub const CTRL_EXT: usize = 0x00018; /* Extended Device Control */
pub const MDIC: usize = 0x00020; /* MDI Control */

/* ---- Interrupts ---- */
pub const ICR: usize = 0x000C0; /* Cause, read-to-clear */
pub const ITR: usize = 0x000C4; /* Throttle, INTx and MSI */
pub const ICS: usize = 0x000C8; /* Cause Set */
pub const IMS: usize = 0x000D0; /* Mask Set: write 1 to enable */
pub const IMC: usize = 0x000D8; /* Mask Clear: write 1 to disable */
pub const EIAC: usize = 0x000DC; /* MSI-X: causes cleared as their message goes */
pub const IAM: usize = 0x000E0; /* Auto Mask: causes masked as their message goes */
pub const IVAR: usize = 0x000E4; /* MSI-X: which vector each cause raises */

/// The throttle of MSI-X vector `n`, the 82574's alone (`E1000_EITR_82574`).
pub const fn eitr(n: usize) -> usize {
    0x000E8 + 4 * n
}

/// What one unit of ITR or EITR is: the interval between interrupts is
/// counted in 256 ns.
pub const ITR_UNIT_NS: u32 = 256;

/* Microseconds between interrupts: 2 idle, widened with the receive rate to
   20 at 400k packets a second, as on igb (see apply_itr). */
pub const ITR_MIN_US: u32 = 2;
pub const ITR_MAX_US: u32 = 20;
pub const ITR_PPS_PER_US: u32 = 20_000;

/* ---- Interrupt causes, ICR/ICS/IMS/IMC/IAM/EIAC bits ---- */
pub const ICR_TXDW: u32 = 1 << 0; /* transmit descriptor written back */
pub const ICR_LSC: u32 = 1 << 2; /* link status change */
pub const ICR_RXDMT0: u32 = 1 << 4; /* receive descriptors below the threshold */
pub const ICR_RXO: u32 = 1 << 6; /* receiver overrun */
pub const ICR_RXT0: u32 = 1 << 7; /* receive timer: a frame is in */
/* The MSI-X causes, which take the place of RXT0 and TXDW once MSI-X is on:
   each queue's own, and Other for everything that is not a queue. */
pub const ICR_RXQ0: u32 = 1 << 20;
pub const ICR_TXQ0: u32 = 1 << 22;
pub const ICR_OTHER: u32 = 1 << 24;
/* Set whenever this device is the one asserting the line: what tells a
   shared INTx apart from a neighbour's. */
pub const ICR_INT_ASSERTED: u32 = 1 << 31;

/// The receive sources on INTx, masked for the duration of a poll.
pub const RX_INTR_BITS: u32 = ICR_RXT0 | ICR_RXDMT0 | ICR_RXO;
/// Everything the INTx handler answers.
pub const INTX_MASK_BITS: u32 = RX_INTR_BITS | ICR_TXDW | ICR_LSC;
/// The queue causes MSI-X vector 0 carries.
pub const QUEUE_CAUSES: u32 = ICR_RXQ0 | ICR_TXQ0;
/// What raises the Other vector: a link change, and an overrun to count.
/// Not the rest of Linux's IMS_OTHER_MASK: nothing here answers an MDIO
/// completion, a small-packet or an ACK detection, or a manageability event,
/// and each is an interrupt for nothing.
pub const OTHER_CAUSES: u32 = ICR_LSC | ICR_RXO;

/* ---- IVAR: four bits a cause -- the vector in the low three, the fourth
 *      saying the entry means anything ---- */
pub const IVAR_VALID: u32 = 0x8;
pub const IVAR_RXQ0_SHIFT: u32 = 0;
pub const IVAR_TXQ0_SHIFT: u32 = 8;
pub const IVAR_OTHER_SHIFT: u32 = 16;
/* A transmit interrupt on every descriptor written back, not only when the
   chip is done with the queue: what Linux sets, and what lets a frame a
   flush is waiting for come back without the queue running dry first. */
pub const IVAR_TX_EVERY_WB: u32 = 1 << 31;

/* ---- CTRL bits ---- */
pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_SLU: u32 = 1 << 6; /* set link up */
pub const CTRL_FRCSPD: u32 = 1 << 11;
pub const CTRL_FRCDPLX: u32 = 1 << 12;
pub const CTRL_RST: u32 = 1 << 26; /* global reset, self-clearing */
pub const CTRL_RFCE: u32 = 1 << 27; /* honour received pause frames */
pub const CTRL_TFCE: u32 = 1 << 28; /* send pause frames */
/* Reserved on the 82573/82574/82583, and cleared there by Linux's
   e1000_initialize_hw_bits_82571. */
pub const CTRL_RESERVED_29: u32 = 1 << 29;

/* ---- STATUS bits ---- */
pub const STATUS_FD: u32 = 1 << 0;
pub const STATUS_LU: u32 = 1 << 1;
pub const STATUS_SPEED_SHIFT: u32 = 6;
pub const STATUS_SPEED_MASK: u32 = 3 << STATUS_SPEED_SHIFT;
pub const STATUS_SPEED_10: u32 = 0 << STATUS_SPEED_SHIFT;
pub const STATUS_SPEED_100: u32 = 1 << STATUS_SPEED_SHIFT;
pub const STATUS_SPEED_1000: u32 = 2 << STATUS_SPEED_SHIFT;
pub const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;

/* ---- EECD ---- */
/* The NVM's auto-read after a reset is done: the station address is in
   RAL0/RAH0 from here on, and the PHY's configuration starts loading. */
pub const EECD_AUTO_RD: u32 = 1 << 9;

/* ---- CTRL_EXT bits ---- */
/* The two "required hardware-dependent bits" Linux's
   e1000_initialize_hw_bits_82571 sets on the 82574: 22 set, 23 clear. */
pub const CTRL_EXT_BIT_22: u32 = 1 << 22;
pub const CTRL_EXT_BIT_23: u32 = 1 << 23;
/* MSI-X: mask a cause in IMS (as IAM names) when its message goes. */
pub const CTRL_EXT_EIAME: u32 = 1 << 24;
/* A read of ICR masks what IAM names: the INTx scheme Linux uses, which
   this driver does not -- it masks the receive side itself. */
pub const CTRL_EXT_IAME: u32 = 1 << 27;
/* The driver has the device: what tells manageability firmware an operating
   system's driver is running it (e1000e_get_hw_control). */
pub const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
/* MSI-X: the pending bit array clears with the cause. */
pub const CTRL_EXT_PBA_CLR: u32 = 1 << 31;

/* ---- MDIC: one PHY register in or out ---- */
pub const MDIC_DATA_MASK: u32 = 0xFFFF;
pub const MDIC_REG_SHIFT: u32 = 16;
pub const MDIC_PHY_SHIFT: u32 = 21;
pub const MDIC_OP_WRITE: u32 = 1 << 26;
pub const MDIC_OP_READ: u32 = 2 << 26;
pub const MDIC_READY: u32 = 1 << 28;
pub const MDIC_ERROR: u32 = 1 << 30;

/* ---- MDIO ownership ----
 *
 * The PHY is shared with the manageability firmware, and the 82574 has a
 * semaphore for it: software sets its ownership bit and has the bus only if
 * the bit reads back set (e1000_get_hw_semaphore_82573, which the 82574's
 * own wraps). Ownership goes back to the firmware with every reset, which
 * is why the reset itself is done holding it. */
pub const EXTCNF_CTRL: usize = 0x00F00;
pub const EXTCNF_CTRL_MDIO_SW_OWNERSHIP: u32 = 1 << 5;
/* Linux's MDIO_OWNERSHIP_TIMEOUT tries, 2 to 4 ms apart. */
pub const MDIO_OWNERSHIP_TRIES: u32 = 10;
pub const MDIO_OWNERSHIP_WAIT_US: u64 = 3000;

/* ---- PHY OEM bits ---- */
/* Low Power Link Up in D0: with it the PHY links at the lowest speed both
   ends offer -- 10 Mbit/s on a gigabit port -- to save power. Cleared, as
   e1000_set_d0_lplu_state_82574(false) clears it. */
pub const POEMB: usize = 0x00F10;
pub const POEMB_D0A_LPLU: u32 = 1 << 1;

/* ---- Packet buffer ---- */
/* Kilobytes of the 40 KB packet buffer that receive gets, transmit having
   the rest: 32, as Linux's e1000_82574_info. Written before the reset,
   which is when the split takes effect. */
pub const PBA: usize = 0x01000;
pub const PBA_RX_KB: u32 = 32;

/* ---- Receive queue 0 ---- */
pub const RCTL: usize = 0x00100;
pub const RDBAL: usize = 0x02800;
pub const RDBAH: usize = 0x02804;
pub const RDLEN: usize = 0x02808;
pub const RDH: usize = 0x02810;
pub const RDT: usize = 0x02818;
pub const RDTR: usize = 0x02820; /* delay before a receive interrupt */
pub const RXDCTL: usize = 0x02828;
pub const RADV: usize = 0x0282C; /* absolute delay cap */

/* ---- Transmit queue 0 ---- */
pub const TCTL: usize = 0x00400;
pub const TDBAL: usize = 0x03800;
pub const TDBAH: usize = 0x03804;
pub const TDLEN: usize = 0x03808;
pub const TDH: usize = 0x03810;
pub const TDT: usize = 0x03818;
pub const TIDV: usize = 0x03820; /* transmit interrupt delay, 1.024 us units */
pub const TXDCTL0: usize = 0x03828;
pub const TADV: usize = 0x0382C;
pub const TARC0: usize = 0x03840;
pub const TXDCTL1: usize = 0x03928;

/* Linux's defaults for the transmit delays (param.c's TxIntDelay and
   TxAbsIntDelay): the timers only run for a descriptor with IDE, which this
   driver never sets, but a zero TIDV is not a value the part supports. */
pub const TIDV_DEFAULT: u32 = 8;
pub const TADV_DEFAULT: u32 = 32;

/* ---- RCTL bits ---- */
pub const RCTL_EN: u32 = 1 << 1;
pub const RCTL_BAM: u32 = 1 << 15; /* broadcast accept */
pub const RCTL_SZ_2048: u32 = 0 << 16; /* with BSEX clear */
pub const RCTL_SECRC: u32 = 1 << 26; /* strip the CRC */

/* ---- TCTL bits ---- */
pub const TCTL_EN: u32 = 1 << 1;
pub const TCTL_PSP: u32 = 1 << 3; /* pad short packets */
pub const TCTL_CT_SHIFT: u32 = 4;
pub const TCTL_COLD_SHIFT: u32 = 12;
pub const TCTL_RTLC: u32 = 1 << 24;
/* The collision threshold and distance Linux programs: E1000_COLLISION_
   THRESHOLD and E1000_COLLISION_DISTANCE. Meaningless on a full-duplex
   link and required all the same. */
pub const TCTL_CT_DEFAULT: u32 = 15 << TCTL_CT_SHIFT;
pub const TCTL_COLD_DEFAULT: u32 = 63 << TCTL_COLD_SHIFT;
pub const TCTL_CT_MASK: u32 = 0xFF << TCTL_CT_SHIFT;
pub const TCTL_COLD_MASK: u32 = 0x3FF << TCTL_COLD_SHIFT;

/* ---- TXDCTL ---- */
/* Bit 22 is both what e1000_initialize_hw_bits_82571 sets on both queues and
   E1000_TXDCTL_COUNT_DESC; GRAN=1 with WTHRESH=1 is a write-back for every
   descriptor that asks for one (E1000_TXDCTL_FULL_TX_DESC_WB). */
pub const TXDCTL_WTHRESH_MASK: u32 = 0x3F << 16;
pub const TXDCTL_FULL_TX_DESC_WB: u32 = 0x0101_0000;
pub const TXDCTL_COUNT_DESC: u32 = 1 << 22;

/* ---- TARC0 ---- */
/* Bits 30:27 cleared and 26 set on the 82574, per
   e1000_initialize_hw_bits_82571. */
pub const TARC0_CLEAR: u32 = 0xF << 27;
pub const TARC0_BIT_26: u32 = 1 << 26;

/* ---- Receive filtering ---- */
pub const RXCSUM: usize = 0x05000;
pub const RFCTL: usize = 0x05008;
/* ACK Accelerate off. With it on, the 82574 raises an interrupt for every
   TCP ACK it receives -- in MSI-X mode one on the Other vector, which is an
   interrupt for nothing per ACK (Linux's "spurious interrupts on 82574 in
   MSI-X mode" workaround in e1000_configure_msix). */
pub const RFCTL_ACK_DIS: u32 = 1 << 12;
/* Extended receive descriptors: kept off, so that the legacy layout desc.rs
   decodes is what the chip writes. */
pub const RFCTL_EXTEN: u32 = 1 << 15;
pub const MTA: usize = 0x05200; /* multicast hash, 128 words */
pub const MTA_ENTRIES: usize = 128;
pub const RAL0: usize = 0x05400;
pub const RAH0: usize = 0x05404;
pub const RAH_AV: u32 = 1 << 31; /* address valid */
/* Receive address registers, 8 bytes apart; RAR0 is the station address. */
pub const RAR_STRIDE: usize = 8;
pub const RAR_ENTRIES: usize = 15;
pub const VFTA: usize = 0x05600; /* VLAN filter table, 128 words */
pub const VFTA_ENTRIES: usize = 128;

/* ---- Manageability ---- */
/* Read, never written: what the firmware that shares this port with a BMC
   is doing is its business. One bit of it is this driver's too: with
   BLK_PHY_RST_ON_IDE the firmware has the PHY in use -- a serial-over-LAN
   or IDE-redirection session -- and a PHY reset would cut it, so the link
   is left as it is (e1000e_check_reset_block_generic). */
pub const MANC: usize = 0x05820;
pub const MANC_ASF_EN: u32 = 1 << 1;
pub const MANC_ARP_EN: u32 = 1 << 13;
pub const MANC_RCV_TCO_EN: u32 = 1 << 17;
pub const MANC_BLK_PHY_RST_ON_IDE: u32 = 1 << 18;
pub const MANC_EN_MNG2HOST: u32 = 1 << 21;

/* ---- PCI Express control ---- */
pub const GCR: usize = 0x05B00;
/* Required on the 82574 (e1000_initialize_hw_bits_82571). */
pub const GCR_BIT_22: u32 = 1 << 22;
/* L1 without L0s on receive (e1000_init_hw_82571, for the 82574). */
pub const GCR_L1_ACT_WITHOUT_L0S_RX: u32 = 1 << 27;
pub const GCR2: usize = 0x05B64;
/* The errata workaround for unreliable PCIe completions that otherwise show
   up as transmit timeouts, ASPM or not (e1000_initialize_hw_bits_82571). */
pub const GCR2_BIT_0: u32 = 1 << 0;

/* ---- Statistics: all read-clear ---- */
pub const CRCERRS: usize = 0x04000;
pub const RXERRC: usize = 0x0400C;
pub const MPC: usize = 0x04010; /* missed: no room in the receive FIFO */
pub const GPRC: usize = 0x04074; /* good packets received */
pub const GPTC: usize = 0x04080; /* good packets sent */
pub const RNBC: usize = 0x040A0; /* no descriptor was there for a frame */
pub const TPR: usize = 0x040D0; /* everything taken off the wire */
pub const TPT: usize = 0x040D4; /* everything sent */

/// How much of BAR 0 the driver reaches: the last register it touches is
/// GCR2. The BAR itself is 128 KiB.
pub const REG_SPACE_USED: usize = GCR2 + 4;

/* ---- PHY registers ---- */
/* The PHY answers at MDIO address 1 and nowhere else (phy->addr = 1 in
   e1000_init_phy_params_82571); QEMU's model refuses any other. */
pub const PHY_ADDR: u32 = 1;

pub const PHY_BMCR: u32 = 0; /* basic mode control */
pub const PHY_BMSR: u32 = 1; /* basic mode status */
pub const PHY_ID1: u32 = 2;
pub const PHY_ID2: u32 = 3;
pub const PHY_ANAR: u32 = 4; /* advertisement */
pub const PHY_ANLPAR: u32 = 5; /* what the partner offered */
pub const PHY_GCTL: u32 = 9; /* 1000BASE-T control */
pub const PHY_GSTAT: u32 = 10; /* 1000BASE-T status */
/* Registers above 15 live on pages; the page is chosen by register 22
   (BM_PHY_PAGE_SELECT) and Linux selects it before every such access
   (e1000e_read_phy_reg_bm2). Everything here is on page 0. */
pub const PHY_MULTI_PAGE_MIN: u32 = 16;
pub const PHY_PAGE_SELECT: u32 = 22;
pub const PHY_PSCR: u32 = 16; /* PHY specific control (M88E1000_PHY_SPEC_CTRL) */
/* What the BM PHY of an 82574 revision 2 is set to by Linux's
   e1000e_copper_link_setup_m88: page 0 register 29 to 3, register 30 to 0.
   Undocumented; a workaround Intel's own driver applies. */
pub const PHY_BM_R2_REG_29: u32 = 29;
pub const PHY_BM_R2_REG_29_VALUE: u16 = 0x0003;
pub const PHY_BM_R2_REG_30: u32 = 30;
pub const PHY_BM_R2_REG_30_VALUE: u16 = 0x0000;

/* The PHY's identity, revision bits masked: the BME1000 every 82574 has.
   Revision 2 (BME1000_E_PHY_ID_R2) is the one Linux accepts on the 82574. */
pub const PHY_ID_MASK: u32 = 0xFFFF_FFF0;
pub const PHY_ID_BME1000: u32 = 0x0141_0CB0;
pub const PHY_ID_BME1000_R2: u32 = 0x0141_0CB1;

pub const BMCR_RESET: u16 = 1 << 15;
pub const BMCR_ANENABLE: u16 = 1 << 12;
pub const BMCR_PDOWN: u16 = 1 << 11;
pub const BMCR_ANRESTART: u16 = 1 << 9;

pub const BMSR_LSTATUS: u16 = 1 << 2;
pub const BMSR_ANEGCOMPLETE: u16 = 1 << 5;

/* PSCR: automatic MDI/MDI-X crossover at every speed, polarity correction
   left on, and downshift -- gigabit falls back to 100 over a cable with two
   pairs -- on, as Linux sets them for the BM PHY. */
pub const PSCR_AUTO_X_MODE: u16 = 0x0060;
pub const PSCR_POLARITY_REVERSAL: u16 = 0x0002;
pub const PSCR_ENABLE_DOWNSHIFT: u16 = 0x0800;

/* What to offer: every speed and duplex but 1000 half, which Linux does not
   advertise either. No pause: this driver runs without flow control, so it
   offers none rather than offer what it would then ignore. */
pub const ANAR_SELECTOR_802_3: u16 = 0x0001;
pub const ANAR_10_HALF: u16 = 1 << 5;
pub const ANAR_10_FULL: u16 = 1 << 6;
pub const ANAR_100_HALF: u16 = 1 << 7;
pub const ANAR_100_FULL: u16 = 1 << 8;
pub const ANAR_PAUSE_CAP: u16 = 1 << 10;
pub const ANAR_PAUSE_ASYM: u16 = 1 << 11;
pub const ANAR_SPEEDS: u16 = ANAR_10_HALF | ANAR_10_FULL | ANAR_100_HALF | ANAR_100_FULL;
pub const GCTL_1000_HALF: u16 = 1 << 8;
pub const GCTL_1000_FULL: u16 = 1 << 9;

/* ---- PCI ---- */
pub const PCI_VENDOR_INTEL: u16 = 0x8086;
/* The 82574L. The 82574LA (10F6) and the 82583V (150C) share its paths in
   Linux, but nothing here has run on either: the table names what has. */
pub const PCI_DEVICE_82574L: u16 = 0x10D3;

pub const PCI_COMMAND: u16 = 0x04;
pub const PCI_COMMAND_MASTER: u16 = 1 << 2;
pub const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

/* PCI Express capability, and its Link Control register's ASPM field. */
pub const PCI_CAP_ID_EXP: u8 = 0x10;
pub const PCI_EXP_LNKCTL: u16 = 0x10;
pub const PCI_EXP_LNKCTL_ASPM_L0S: u16 = 1 << 0;
pub const PCI_EXP_LNKCTL_ASPM_L1: u16 = 1 << 1;
pub const PCI_EXP_LNKCTL_ASPMC: u16 = PCI_EXP_LNKCTL_ASPM_L0S | PCI_EXP_LNKCTL_ASPM_L1;

/* A PCI-to-PCI bridge, and where its secondary bus number is. */
pub const PCI_CLASS_BRIDGE: u8 = 0x06;
pub const PCI_SUBCLASS_PCI_BRIDGE: u8 = 0x04;
pub const PCI_SECONDARY_BUS: u16 = 0x19;
