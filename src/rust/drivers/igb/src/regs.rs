/* Register map for the Intel igb family (82575/82576/I210/I211).
 *
 * Offsets are the ones Intel publishes, which is the whole reason this part
 * is pleasanter to drive than the Realtek next door: the datasheet says what
 * each bit does instead of leaving it to be inferred from a vendor driver.
 *
 * Only queue 0 is defined. The per-queue blocks are strided -- RX at
 * 0x0C000 + 0x40*n, TX at 0x0E000 + 0x40*n -- so adding queues later is a
 * matter of the stride, not of new names. */

/* ---- General ---- */
pub const CTRL: usize = 0x00000; /* Device Control */
pub const STATUS: usize = 0x00008; /* Device Status */
pub const CTRL_EXT: usize = 0x00018; /* Extended Device Control */

/* ---- Interrupts (the legacy block; the extended EICR block is only
 *      needed once MSI-X spreads queues across vectors) ---- */
pub const ICR: usize = 0x01500; /* Cause, read-to-clear */
pub const ICS: usize = 0x01504; /* Cause Set */
pub const IMS: usize = 0x01508; /* Mask Set -- write 1 to enable */
pub const IMC: usize = 0x0150C; /* Mask Clear -- write 1 to disable */
pub const IAM: usize = 0x01510; /* Auto Mask */

/* ---- Extended interrupts ----
 *
 * The legacy ICR/IMS block above is what the older parts in this family use.
 * From the 82576 on, causes are routed per queue to MSI-X vectors through
 * IVAR, and the mask lives in this block instead. QEMU's model implements
 * only this path, which is also the one real multi-queue hardware wants. */
pub const GPIE: usize = 0x01514; /* General Purpose Interrupt Enable */
pub const EICS: usize = 0x01520; /* Extended Cause Set */
pub const EIMS: usize = 0x01524; /* Extended Mask Set */
pub const EIMC: usize = 0x01528; /* Extended Mask Clear */
pub const EIAC: usize = 0x0152C; /* Extended Auto Clear */
pub const EIAM: usize = 0x01530; /* Extended Auto Mask */
pub const EICR: usize = 0x01580; /* Extended Cause */
pub const IVAR0: usize = 0x01700; /* queue-to-vector map, 2 queues per reg */
pub const IVAR_MISC: usize = 0x01740; /* non-queue causes */

/* ---- GPIE bits ---- */
pub const GPIE_NSICR: u32 = 1 << 0; /* clear the cause on read */
pub const GPIE_MSIX_MODE: u32 = 1 << 4;
pub const GPIE_EIAME: u32 = 1 << 30; /* auto-mask a vector when it fires */
pub const GPIE_PBA: u32 = 1 << 31;

/* An IVAR byte: the low bits pick the vector, the top bit says the entry
 * means anything at all. */
pub const IVAR_VALID: u32 = 0x80;

/* This driver puts every cause on one vector, so one bit of EICR is the whole
 * interrupt. */
pub const EICR_VECTOR0: u32 = 1 << 0;

/* ---- Receive ---- */
pub const RCTL: usize = 0x00100;
pub const RDBAL0: usize = 0x0C000;
pub const RDBAH0: usize = 0x0C004;
pub const RDLEN0: usize = 0x0C008;
pub const SRRCTL0: usize = 0x0C00C;
pub const RDH0: usize = 0x0C010;
pub const RDT0: usize = 0x0C018;
pub const RXDCTL0: usize = 0x0C028;

/* ---- Transmit ---- */
pub const TCTL: usize = 0x00400;
pub const TDBAL0: usize = 0x0E000;
pub const TDBAH0: usize = 0x0E004;
pub const TDLEN0: usize = 0x0E008;
pub const TDH0: usize = 0x0E010;
pub const TDT0: usize = 0x0E018;
pub const TXDCTL0: usize = 0x0E028;

/* ---- Receive address filter ---- */
pub const RAL0: usize = 0x05400;
pub const RAH0: usize = 0x05404;
pub const MTA: usize = 0x05200; /* 128 entries, 4 bytes apart */
pub const MTA_ENTRIES: usize = 128;

/* The highest offset this driver touches, which sets how much of the BAR
 * has to be mapped. */
/* TXDCTL0 is the highest of them all; the interrupt block sits far below it. */
pub const REG_SPACE_USED: usize = TXDCTL0 + 4;

/* ---- CTRL bits ---- */
pub const CTRL_FD: u32 = 1 << 0; /* full duplex */
pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_ASDE: u32 = 1 << 5; /* auto-speed detection */
pub const CTRL_SLU: u32 = 1 << 6; /* set link up */
pub const CTRL_RST: u32 = 1 << 26;

/* ---- STATUS bits ---- */
pub const STATUS_FD: u32 = 1 << 0;
pub const STATUS_LU: u32 = 1 << 1; /* link up */
pub const STATUS_SPEED_MASK: u32 = 3 << 6;
pub const STATUS_SPEED_10: u32 = 0 << 6;
pub const STATUS_SPEED_100: u32 = 1 << 6;
pub const STATUS_SPEED_1000: u32 = 2 << 6;
pub const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;

/* ---- Interrupt cause / mask bits ---- */
pub const ICR_TXDW: u32 = 1 << 0; /* transmit descriptor written back */
pub const ICR_LSC: u32 = 1 << 2; /* link status change */
pub const ICR_RXDMT0: u32 = 1 << 4; /* rx descriptor minimum threshold */
pub const ICR_RXO: u32 = 1 << 6; /* receiver overrun */
pub const ICR_RXT0: u32 = 1 << 7; /* rx timer / packet received */

/* Everything this driver knows how to answer. */
pub const INTR_MASK_BITS: u32 = ICR_TXDW | ICR_LSC | ICR_RXDMT0 | ICR_RXO | ICR_RXT0;

/* The receive sources, masked for the duration of a poll. */
pub const RX_INTR_BITS: u32 = ICR_RXT0 | ICR_RXDMT0 | ICR_RXO;

/* ---- RCTL bits ---- */
pub const RCTL_RST: u32 = 1 << 0;
pub const RCTL_EN: u32 = 1 << 1;
pub const RCTL_SBP: u32 = 1 << 2; /* store bad packets */
pub const RCTL_UPE: u32 = 1 << 3; /* unicast promiscuous */
pub const RCTL_MPE: u32 = 1 << 4; /* multicast promiscuous */
pub const RCTL_LPE: u32 = 1 << 5; /* long packet enable */
pub const RCTL_BAM: u32 = 1 << 15; /* broadcast accept */
pub const RCTL_SZ_2048: u32 = 0 << 16;
pub const RCTL_VFE: u32 = 1 << 18; /* vlan filter enable */
pub const RCTL_SECRC: u32 = 1 << 26; /* strip ethernet CRC */

/* ---- TCTL bits ---- */
pub const TCTL_EN: u32 = 1 << 1;
pub const TCTL_PSP: u32 = 1 << 3; /* pad short packets */
pub const TCTL_CT_SHIFT: u32 = 4; /* collision threshold */
pub const TCTL_COLD_SHIFT: u32 = 12; /* collision distance */
pub const TCTL_RTLC: u32 = 1 << 24;

/* Half-duplex legacy values the datasheet asks for even on a full-duplex
 * link; the chip ignores them there but the vendor driver still writes them. */
pub const TCTL_CT_DEFAULT: u32 = 0x0F << TCTL_CT_SHIFT;
pub const TCTL_COLD_FULL_DUPLEX: u32 = 0x3F << TCTL_COLD_SHIFT;

/* ---- SRRCTL bits ---- */
/* Packet buffer size in KiB, in the low 7 bits. */
pub const SRRCTL_BSIZEPKT_SHIFT: u32 = 10;
/* Advanced descriptors, one buffer per descriptor: the header-split modes
 * would hand back a frame in two pieces, which the net layer has no use for. */
pub const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 1 << 25;
pub const SRRCTL_DROP_EN: u32 = 1 << 31;

/* ---- RXDCTL / TXDCTL ---- */
pub const XDCTL_QUEUE_ENABLE: u32 = 1 << 25;

/* ---- Receive address high ---- */
pub const RAH_AV: u32 = 1 << 31; /* address valid */

/* ---- Advanced receive descriptor, write-back format ---- */
pub const RXD_STAT_DD: u32 = 1 << 0; /* descriptor done */
pub const RXD_STAT_EOP: u32 = 1 << 1; /* end of packet */
/* Errors live in the top bits of the same word. RXE covers the lot for our
 * purposes: a frame with any of them set is not one to hand upwards. */
pub const RXD_ERR_CE: u32 = 1 << 24; /* CRC error */
pub const RXD_ERR_SE: u32 = 1 << 25; /* symbol error */
pub const RXD_ERR_SEQ: u32 = 1 << 26; /* sequence error */
pub const RXD_ERR_CXE: u32 = 1 << 28; /* carrier extension */
pub const RXD_ERR_RXE: u32 = 1 << 31; /* rx data error */
pub const RXD_ERR_MASK: u32 =
    RXD_ERR_CE | RXD_ERR_SE | RXD_ERR_SEQ | RXD_ERR_CXE | RXD_ERR_RXE;

/* ---- Advanced transmit descriptor ---- */
pub const TXD_DTYP_DATA: u32 = 0x3 << 20;
pub const TXD_DCMD_EOP: u32 = 1 << 24;
pub const TXD_DCMD_IFCS: u32 = 1 << 25; /* insert FCS */
pub const TXD_DCMD_RS: u32 = 1 << 27; /* report status */
pub const TXD_DCMD_DEXT: u32 = 1 << 29; /* descriptor extension: advanced */
pub const TXD_PAYLEN_SHIFT: u32 = 14;
pub const TXD_STAT_DD: u32 = 1 << 0;

/* ---- PHY access through the MDI control register ----
 *
 * The link does not come up on its own. Both the silicon and the QEMU model
 * leave STATUS.LU clear until auto-negotiation completes, and negotiation
 * starts when the driver asks for it through the PHY -- there is no MAC-side
 * register that does it. */
pub const MDIC: usize = 0x00020;
pub const MDIC_DATA_MASK: u32 = 0xFFFF;
pub const MDIC_REG_SHIFT: u32 = 16;
pub const MDIC_PHY_SHIFT: u32 = 21;
pub const MDIC_OP_WRITE: u32 = 1 << 26;
pub const MDIC_OP_READ: u32 = 2 << 26;
pub const MDIC_READY: u32 = 1 << 28;
pub const MDIC_ERROR: u32 = 1 << 30;

/* The integrated copper PHY sits at MDI address 1 on every part here. */
pub const PHY_ADDR_INTERNAL: u32 = 1;

/* Standard MII registers, which is all this driver needs: no vendor pages. */
pub const PHY_BMCR: u32 = 0; /* basic mode control */
pub const PHY_BMSR: u32 = 1; /* basic mode status */

pub const BMCR_RESET: u16 = 1 << 15;
pub const BMCR_ANENABLE: u16 = 1 << 12;
pub const BMCR_PDOWN: u16 = 1 << 11;
pub const BMCR_ANRESTART: u16 = 1 << 9;
pub const BMCR_FULLDPLX: u16 = 1 << 8;
pub const BMCR_SPEED1000: u16 = 1 << 6;

pub const BMSR_LSTATUS: u16 = 1 << 2;
pub const BMSR_ANEGCOMPLETE: u16 = 1 << 5;

/* ---- PCI ---- */
pub const PCI_VENDOR_INTEL: u16 = 0x8086;
pub const PCI_COMMAND: u16 = 0x04;
pub const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

/* The parts this driver claims. 82576 is what QEMU emulates and is where
 * this was developed; I210 is the one on real hardware. They share the
 * register map this file describes. */
pub const PCI_DEVICE_82576: u16 = 0x10C9;
pub const PCI_DEVICE_I210: u16 = 0x1533;
pub const SUPPORTED_DEVICES: [u16; 2] = [PCI_DEVICE_82576, PCI_DEVICE_I210];
