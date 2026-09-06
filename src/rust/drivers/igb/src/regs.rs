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
/* Interrupt throttling, one per MSI-X vector. A guaranteed minimum delay
   between interrupts, in microseconds, and -- the part that matters -- it is
   documented as *not* reset by a device reset. Whatever firmware left in it
   stands until the driver says otherwise, and left alone it caps the receive
   rate no matter how idle the CPU is. */
pub const EITR0: usize = 0x01680;
pub const EITR_INTERVAL_SHIFT: u32 = 2;
pub const EITR_INTERVAL_MASK: u32 = 0x1FFF << EITR_INTERVAL_SHIFT;

/* Microseconds between interrupts. Two is the low end of the range the
   datasheet suggests, and with the receive sources masked for the duration of
   a poll the real rate is set by how fast the ring drains, not by this. Zero
   is not a legal setting. */
pub const EITR_INTERVAL_US: u32 = 2;

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

/* ---- Statistics ----
 *
 * All read-clear: reading one zeroes it, so they have to be accumulated in
 * software rather than sampled. They are the only way to tell a NIC that is
 * not being offered packets from one that is throwing them away -- the
 * descriptor ring's own counters cannot see a frame the MAC dropped before it
 * ever reached for a descriptor. */
pub const RXERRC: usize = 0x0400C; /* receive errors */
pub const MPC: usize = 0x04010; /* missed: no room in the receive FIFO */
pub const GPRC: usize = 0x04074; /* good packets received */
pub const RNBC: usize = 0x040A0; /* no descriptor was available */
pub const TPR: usize = 0x040D0; /* everything the MAC took off the wire */

/* Per-queue, and not read-clear like the five above: these are read as they
   stand. RQDPC is the one that says a queue was offered a packet and had no
   descriptor to put it in -- the gap between what the MAC accepted and what
   the driver ever saw, which nothing else counts. */
pub const RQDPC0: usize = 0x0C030;
pub const PQGPRC0: usize = 0x10010;

/* ---- Receive address filter ---- */
pub const RAL0: usize = 0x05400;
pub const RAH0: usize = 0x05404;
pub const MTA: usize = 0x05200; /* 128 entries, 4 bytes apart */
pub const MTA_ENTRIES: usize = 128;

/* The highest offset this driver touches, which sets how much of the BAR
 * has to be mapped. */
/* EEC is the highest of them all -- the queue and interrupt blocks sit well
 * below it. The card's BAR is far larger again (512 KiB on the I210), but
 * nothing this driver touches lives above here. */
pub const REG_SPACE_USED: usize = EEC + 4;

/* ---- CTRL bits ---- */
pub const CTRL_FD: u32 = 1 << 0; /* full duplex */
pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_ASDE: u32 = 1 << 5; /* auto-speed detection */
pub const CTRL_SLU: u32 = 1 << 6; /* set link up */
pub const CTRL_RST: u32 = 1 << 26;

/* Speed the MAC runs at. There are three ways to set it, and the datasheet
   (3.7.4.4.2.2) is clear about which one an internal PHY wants: with ASDE
   and FRCSPD both clear, the MAC takes speed and duplex straight from the
   PHY at every link-up. ASDE is the trap this driver fell into -- it samples
   once, at the first LINK the PHY asserts, and never revisits, so a PHY
   that asserts link early and negotiates up leaves the MAC behind. FRCSPD
   and FRCDPLX override both with the CTRL fields; kept as the fallback. */
pub const CTRL_SPEED_SHIFT: u32 = 8;
pub const CTRL_SPEED_MASK: u32 = 3 << CTRL_SPEED_SHIFT;
pub const CTRL_FRCSPD: u32 = 1 << 11; /* take the speed from CTRL, not the PHY */
pub const CTRL_FRCDPLX: u32 = 1 << 12; /* take the duplex from CTRL.FD */

/* ---- STATUS bits ---- */
pub const STATUS_FD: u32 = 1 << 0;
pub const STATUS_LU: u32 = 1 << 1; /* link up */
pub const STATUS_SPEED_SHIFT: u32 = 6;
pub const STATUS_SPEED_MASK: u32 = 3 << STATUS_SPEED_SHIFT;
pub const STATUS_SPEED_10: u32 = 0 << 6;
pub const STATUS_SPEED_100: u32 = 1 << 6;
pub const STATUS_SPEED_1000: u32 = 2 << 6;
pub const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;

/* What the MAC's own auto-detection last sensed off the PHY receive clock.
   The datasheet calls it diagnostic, but it is the one place that says what
   the link really settled on when the latched SPEED above disagrees. Same
   encoding as CTRL.SPEED. */
pub const STATUS_ASDV_SHIFT: u32 = 8;
pub const STATUS_ASDV_MASK: u32 = 3 << STATUS_ASDV_SHIFT;

/* Initiates a fresh detection; self-clearing, result lands in STATUS.ASDV. */
pub const CTRL_EXT_ASDCHK: u32 = 1 << 12;

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
/* Drop a frame the moment the on-chip descriptor cache is empty, instead of
 * holding it in the packet buffer until the next fetch from the ring. Meant
 * for keeping one of several queues from stalling the others; on a single
 * queue it turns every fetch latency into loss, so it is not set. Kept for
 * the day there is a second queue. */
pub const SRRCTL_DROP_EN: u32 = 1 << 31;

/* ---- RXDCTL / TXDCTL ---- */
pub const XDCTL_QUEUE_ENABLE: u32 = 1 << 25;

/* Descriptor prefetch thresholds, and the reason the enable bit cannot be
   written on its own: these share the register, and zeroing them stops the
   chip prefetching descriptors at all. PTHRESH is how few it may hold on the
   die before it considers fetching more -- at zero the count would have to
   fall below zero, so it never fetches, and packets are dropped with a full
   ring in host memory. The values are the ones the part resets to, which is
   what it was designed around. */
pub const XDCTL_PTHRESH_SHIFT: u32 = 0;
pub const XDCTL_HTHRESH_SHIFT: u32 = 8;
pub const XDCTL_WTHRESH_SHIFT: u32 = 16;
pub const XDCTL_THRESH_MASK: u32 =
    (0x1F << XDCTL_PTHRESH_SHIFT) | (0x1F << XDCTL_HTHRESH_SHIFT) | (0x1F << XDCTL_WTHRESH_SHIFT);

/* The values Linux's igb programs, which is a stronger recommendation than
   the reset defaults: it drives these parts at line rate and this does not.
   WTHRESH is the one that differs -- it is how many finished descriptors the
   chip gathers before writing them back, and at 1 every received packet costs
   its own PCIe write. Linux uses 1 only for the 82576 with MSI-X, where it is
   a documented workaround, and 4 everywhere else. */
pub const XDCTL_PTHRESH: u32 = 8;
pub const XDCTL_HTHRESH: u32 = 8;
pub const XDCTL_WTHRESH_82576: u32 = 1;
pub const XDCTL_WTHRESH_I210: u32 = 4;

pub const fn xdctl_thresh(wthresh: u32) -> u32 {
    (XDCTL_PTHRESH << XDCTL_PTHRESH_SHIFT)
        | (XDCTL_HTHRESH << XDCTL_HTHRESH_SHIFT)
        | (wthresh << XDCTL_WTHRESH_SHIFT)
}

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

/* ---- NVM auto-read ----
 *
 * After a reset the chip reloads a block of registers from the NVM, the
 * station address among them. Reading RAL0 before this bit is set can hand
 * back whatever was there before the load. */
pub const EEC: usize = 0x12010;
pub const EEC_AUTO_RD: u32 = 1 << 9;

/* ---- MDI configuration ----
 *
 * Where the PHY sits on the MDIO bus. The 82576 has it hard-wired at address
 * 1; from the I210 on it is a field here, written from the NVM, and reading
 * the wrong address gets a bus with nothing on it rather than an error. */
pub const MDICNFG: usize = 0x00E04;
pub const MDICNFG_PHY_MASK: u32 = 0x1F << 21;
/* Clear means every MDIC access goes to the integrated PHY. */
pub const MDICNFG_DESTINATION: u32 = 1 << 30;
pub const MDICNFG_PHY_SHIFT: u32 = 21;

/* ---- Software/firmware semaphore (I210) ----
 *
 * The 82576 hands the PHY to whoever asks. On the I210 the manageability
 * firmware shares the same MDIO bus, and both sides are expected to take a
 * semaphore before touching it. Two levels: a hardware mutex in SWSM that
 * arbitrates access to SW_FW_SYNC, and a bit in SW_FW_SYNC itself claiming
 * the resource -- software in the low half of the word, firmware in the
 * high. */
pub const SWSM: usize = 0x05B50;
pub const SW_FW_SYNC: usize = 0x05B5C;

pub const SWSM_SMBI: u32 = 1 << 0; /* the hardware mutex itself */
pub const SWSM_SWESMBI: u32 = 1 << 1; /* software's claim on it */

pub const SWFW_PHY0_SM: u32 = 1 << 1;
pub const SWFW_FW_SHIFT: u32 = 16;

/* ---- CTRL_EXT bits ---- */
/* Which interface the MAC talks to. Zero is the integrated copper PHY, which
 * is what both parts here have; the others select SGMII and SERDES. */
pub const CTRL_EXT_LINK_MODE_MASK: u32 = 3 << 22;
pub const CTRL_EXT_LINK_MODE_INTERNAL: u32 = 0 << 22;

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

/* The address written into MDIC. It does not actually select anything on a
   copper part: with MDICNFG.destination clear every access goes to the
   integrated PHY and, in the datasheet's words, "the PHY address is ignored".
   MDICNFG.PHYADD is the address of an *external* PHY on an SGMII or SerDes
   board. One is what the older parts hard-wire, so it is what goes in the
   field. (333016 rev 3.7, section 3.7.2.2.) */
pub const PHY_ADDR_INTERNAL: u32 = 1;

/* Standard MII registers, which is all this driver needs: no vendor pages. */
pub const PHY_BMCR: u32 = 0; /* basic mode control */
pub const PHY_BMSR: u32 = 1; /* basic mode status */
pub const PHY_ANAR: u32 = 4; /* auto-negotiation advertisement */
pub const PHY_ANLPAR: u32 = 5; /* what the link partner offered */
pub const PHY_GCTL: u32 = 9; /* 1000BASE-T control */
pub const PHY_GSTAT: u32 = 10; /* 1000BASE-T status */

/* What to offer the link partner. Without these the PHY negotiates on
   whatever its advertisement registers happen to hold, which on this part
   settled at 10 Mbit -- a gigabit port and a gigabit PHY agreeing on 10BASE-T
   because nobody said otherwise. */
pub const ANAR_SELECTOR_802_3: u16 = 0x0001;
pub const ANAR_10_HALF: u16 = 1 << 5;
pub const ANAR_10_FULL: u16 = 1 << 6;
pub const ANAR_100_HALF: u16 = 1 << 7;
pub const ANAR_100_FULL: u16 = 1 << 8;
pub const ANAR_ADVERTISE_ALL: u16 = ANAR_SELECTOR_802_3
    | ANAR_10_HALF
    | ANAR_10_FULL
    | ANAR_100_HALF
    | ANAR_100_FULL;

pub const GCTL_1000_HALF: u16 = 1 << 8;
pub const GCTL_1000_FULL: u16 = 1 << 9;

/* 1000BASE-T status: what the partner claims it can do. */
pub const GSTAT_PARTNER_1000_HALF: u16 = 1 << 10;
pub const GSTAT_PARTNER_1000_FULL: u16 = 1 << 11;

/* The CTRL.SPEED encoding, which STATUS.SPEED and STATUS.ASDV share. */
pub const CTRL_SPEED_10: u32 = 0;
pub const CTRL_SPEED_100: u32 = 1;
pub const CTRL_SPEED_1000: u32 = 2;

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

/* The parts this driver claims.
 *
 * 82576 is what QEMU emulates and where this was developed. The I210 family
 * is what is on real hardware -- 0x1533 is the copper part on the AMD box.
 * They share the register map this file describes; where they diverge, the
 * driver splits on Generation rather than on the individual id. */
pub const PCI_DEVICE_82576: u16 = 0x10C9;
pub const PCI_DEVICE_82576_QUAD: u16 = 0x10E8;
pub const PCI_DEVICE_82576_NS: u16 = 0x150A;

pub const PCI_DEVICE_I210_COPPER: u16 = 0x1533;
pub const PCI_DEVICE_I210_FIBER: u16 = 0x1536;
pub const PCI_DEVICE_I210_SERDES: u16 = 0x1537;
pub const PCI_DEVICE_I210_SGMII: u16 = 0x1538;
pub const PCI_DEVICE_I210_COPPER_FLASHLESS: u16 = 0x157B;
pub const PCI_DEVICE_I210_SERDES_FLASHLESS: u16 = 0x157C;
pub const PCI_DEVICE_I211_COPPER: u16 = 0x1539;
