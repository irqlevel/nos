//! Firmware bring-up and teardown: ownership, reset, QUERY_FW, MAP_FA/RUN_FW,
//! QUERY_DEV_CAP, the ICM profile and mapping, INIT_HCA, and back again
//! (docs/spec/connectx3.md sections 1, 3 and 6.4). Section numbers in comments are
//! that document's.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::time::Duration;

use crate::time as boot;
use crate::pci::Status;

use crate::bars;
use crate::dma::{self, DmaBuf, Mem, PAGE};
use crate::hcr::{self, CmdError, Hcr, Op, BAR_DCS};
use crate::pci::{self, PciIo};

/// A step failed; what and why has already been printed.
pub struct Fail;

impl Fail {
    pub fn log(what: &str, status: Status) -> Fail {
        alarm!("  {what}: {status:?}");
        Fail
    }
}

macro_rules! fail {
    ($($t:tt)*) => {{
        alarm!("  {}", format_args!($($t)*));
        $crate::fw::Fail
    }};
}
pub(crate) use fail;

/// BAR 0 registers (1.3).
const OWNER: u64 = 0x8069c;
const RESET: u64 = 0xf0010;
const RESET_SEMAPHORE: u64 = 0xf03fc;
/// The UAR BAR's config register (1.2: config 0x18, BAR register 2). Its
/// PCI I/O `BarIndex` depends on the firmware (#15): `Hca::uar_bar`.
const UAR_REG: usize = 2;
/// x86 cache line, for INIT_HCA and the MTT reservation (3.10, 4.1).
const CACHE_LINE: u64 = 64;
/// MGM entry size, log2 (3.7, Linux default).
pub const LOG_MGM_ENTRY: u32 = 10;
/// Entries per page-list command (3.2).
const PAGE_LIST_MAX: usize = PAGE / 16;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub fn pow2(x: u64) -> u64 {
    x.max(1).next_power_of_two()
}

pub fn log2(x: u64) -> u32 {
    x.trailing_zeros()
}

/// Bring the device up and keep it (6.1 steps 1–14): memory space and bus
/// master on, ownership taken, reset, firmware started, INIT_HCA done. On
/// failure everything is put back (reset state, ownership released, PCI
/// attributes restored) before returning.
pub fn open(pci: &mut PciIo) -> Result<(Hca, Setup), Fail> {
    let original = pci.get_attributes().map_err(|e| Fail::log("PCI attributes", e.status()))?;
    let supported = pci.supported_attributes().unwrap_or(0);
    // 1.7 and 0.3: memory space, bus master, 64-bit DMA where offered.
    let want = pci::ATTR_MEMORY | pci::ATTR_BUS_MASTER | (supported & pci::ATTR_DUAL_ADDRESS_CYCLE);
    pci.enable_attributes(want).map_err(|e| Fail::log("enable memory space and bus master", e.status()))?;
    let r = claim(pci).and_then(|()| {
        let r = start(pci, original);
        if r.is_err() {
            release(pci, true);
        }
        r
    });
    if r.is_err() {
        restore(pci, original);
    }
    r
}

/// Driver Binding `Stop` (6.4): take every object back, stop the firmware,
/// free its memory, release ownership, restore the PCI attributes. False
/// when the device had to be reset or its memory had to stay allocated.
pub fn close(hca: Hca, pci: &mut PciIo) -> bool {
    let original = hca.original;
    let ok = hca.teardown(pci);
    release(pci, true);
    restore(pci, original);
    ok
}

/// ExitBootServices (6.4): the same command teardown as `close`, but silent
/// and without freeing memory (the notify function may not call memory
/// services). Then release ownership without the wait and clear bus master.
/// If a command fails, clearing bus master alone stops the device's DMA.
pub fn quiesce(hca: &mut Hca, pci: &mut PciIo) {
    hca.hcr.silent = true;
    let _ = hca.stop_firmware(pci);
    hca.broken = true;
    let _ = pci.mem_write32(BAR_DCS, OWNER, 0);
    if let Ok(cmd) = pci.read_config16(0x04) {
        let _ = pci.write_config16(0x04, cmd & !(1 << 2));
    }
}

/// Take the ownership semaphore (1.5), with INTx off first (1.7).
fn claim(pci: &mut PciIo) -> Result<(), Fail> {
    // 1.7: a polling driver: INTx disabled, no MSI.
    if let Ok(cmd) = pci.read_config16(0x04) {
        let _ = pci.write_config16(0x04, cmd | (1 << 10));
    }
    // One read takes the semaphore; read it exactly once.
    let v = pci.mem_read32(BAR_DCS, OWNER).map_err(|e| Fail::log("ownership read", e.status()))?;
    if v != 0 {
        return Err(fail!("ownership semaphore reads {:08x}: another function or driver owns the device; leaving it", u32::from_be(v)));
    }
    trace!("  ownership semaphore read 0: claimed");
    Ok(())
}

/// Give the semaphore back (1.5); `wait` gives the firmware the second it
/// needs before the next owner reads it.
fn release(pci: &mut PciIo, wait: bool) {
    match pci.mem_write32(BAR_DCS, OWNER, 0) {
        Ok(()) => {
            if wait {
                boot::stall(ms(1000));
            }
            trace!("  ownership released");
        }
        Err(e) => say!("  ownership release: {:?}", e.status()),
    }
}

fn restore(pci: &mut PciIo, original: u64) {
    if let Err(e) = pci.set_attributes(original) {
        say!("  restoring PCI attributes {original:#x}: {:?}", e.status());
    }
}

fn start(pci: &mut PciIo, original: u64) -> Result<(Hca, Setup), Fail> {
    // 1.6: an option ROM or an earlier driver may have left firmware running.
    reset(pci)?;
    let uar = uar_bar(pci)?;
    let hcr = Hcr::new(pci)?;
    let mut hca = Hca {
        hcr,
        uar_bar: uar.index,
        uar_size: uar.size,
        fa: None,
        aux: None,
        icm: Vec::new(),
        mtt: None,
        open: false,
        broken: false,
        undo: Vec::new(),
        lent: Vec::new(),
        original,
    };
    match hca.bring_up(pci) {
        Ok(setup) => {
            trace!("  firmware bring-up complete");
            Ok((hca, setup))
        }
        Err(f) => {
            hca.teardown(pci);
            Err(f)
        }
    }
}

/// The UAR BAR's PCI I/O `BarIndex` (1.2): EDK2 numbers BAR registers (2),
/// AMI Aptio 4 numbers BARs (1); `bars.rs` asks PCI I/O about both.
fn uar_bar(pci: &mut PciIo) -> Result<bars::Pick, Fail> {
    let pick = pci.find_bar(UAR_REG).map_err(|e| Fail::log("UAR BAR: config read", e.status()))?;
    match pick.size {
        Some(size) => trace!("  UAR BAR (register {UAR_REG}): BarIndex {}, {size:#x} bytes; {}", pick.index, pick.how.describe()),
        None => trace!("  UAR BAR (register {UAR_REG}): BarIndex {}, size unknown; {}", pick.index, pick.how.describe()),
    }
    Ok(pick)
}

/// The PCI Express capability's offset, for the reset's restore (1.6).
fn pcie_cap(pci: &mut PciIo) -> Option<u32> {
    let mut at = pci.read_config32(0x34).ok()? & 0xfc;
    for _ in 0..48 {
        if at == 0 {
            return None;
        }
        let h = pci.read_config16(at).ok()?;
        if h & 0xff == 0x10 {
            return Some(at);
        }
        at = u32::from(h >> 8) & 0xfc;
    }
    None
}

/// Reset the device and restore its configuration space (1.6).
fn reset(pci: &mut PciIo) -> Result<(), Fail> {
    let mut saved = [0u32; 16];
    for (i, d) in saved.iter_mut().enumerate() {
        *d = pci.read_config32(i as u32 * 4).map_err(|e| Fail::log("reset: save config", e.status()))?;
    }
    let pcie = pcie_cap(pci);
    let ctl = pcie.and_then(|c| Some((pci.read_config16(c + 0x08).ok()?, pci.read_config16(c + 0x10).ok()?)));

    // The hardware semaphore keeps flash updates out while we reset.
    let mut waited = 0;
    loop {
        match pci.mem_read32(BAR_DCS, RESET_SEMAPHORE) {
            Ok(0) => break,
            Ok(v) if waited >= 10_000 => {
                return Err(fail!("reset: hardware semaphore still {:08x} after 10 s", u32::from_be(v)))
            }
            Ok(_) => {}
            Err(e) => return Err(Fail::log("reset: semaphore read", e.status())),
        }
        boot::stall(ms(1));
        waited += 1;
    }
    pci.mem_write32(BAR_DCS, RESET, 1u32.to_be()).map_err(|e| Fail::log("reset: write", e.status()))?;
    boot::stall(ms(1000));

    let mut back = false;
    for _ in 0..2000 {
        if pci.read_config16(0).is_ok_and(|v| v != 0xffff) {
            back = true;
            break;
        }
        boot::stall(ms(1));
    }
    if !back {
        return Err(fail!("reset: the device did not come back (vendor ID 0xffff after 2 s)"));
    }

    if let (Some(c), Some((devctl, lnkctl))) = (pcie, ctl) {
        let _ = pci.write_config16(c + 0x08, devctl);
        let _ = pci.write_config16(c + 0x10, lnkctl);
    }
    for (i, d) in saved.iter().enumerate() {
        if i != 1 {
            let _ = pci.write_config32(i as u32 * 4, *d);
        }
    }
    let _ = pci.write_config32(0x04, saved[1]);
    trace!("  reset: done (semaphore after {waited} ms), config space restored");
    Ok(())
}

/// QUERY_FW (3.1).
struct FwInfo {
    pages: u64,
    rev: u16,
}

/// The QUERY_DEV_CAP fields bring-up and the data path use (3.4), counts
/// already decoded. In SYS_EQS mode QUERY_FUNC replaces the EQ and UAR
/// reservations (3.11).
pub struct DevCap {
    pub rsvd_qps: u64,
    pub rsvd_srqs: u64,
    pub rsvd_cqs: u64,
    pub rsvd_eqs: u64,
    pub max_eqs: u64,
    pub rsvd_mtts: u64,
    pub rsvd_mpts: u64,
    /// Counts, not logs (0x48, 0x64).
    pub rsvd_uars: u64,
    pub rsvd_pds: u64,
    pub log_max_qp_wqes: u8,
    pub log_max_cqes: u8,
    pub num_sys_eqs: u64,
    pub ports: u8,
    pub flags: u64,
    /// Extended flags 2 (0x70).
    pub flags2: u32,
    pub bmme: u32,
    /// 0x7a bit 5: ETH_PROT_CTRL, the PTYS register may be used (3.4, 5.12).
    pub eth_prot_ctrl: bool,
    /// 0x9c bit 0: ETH_BACKPL_AN_REP (3.4, 5.13).
    pub eth_backpl_an_rep: bool,
    pub uar_bytes: u64,
    /// Entry sizes, indexed like `Profile::t`.
    pub entry: [u64; TABLES],
    pub max_icm: u64,
}

impl DevCap {
    pub fn flag(&self, bit: u32) -> bool {
        self.flags & (1 << bit) != 0
    }
    /// B0 steering: both VEP steering capabilities (5.4, 3.10).
    pub fn b0(&self) -> bool {
        self.flag(41) && self.flag(42)
    }
}

/// What QUERY_PORT said about one port (3.5), for the data path.
#[derive(Clone, Copy)]
pub struct PortInfo {
    pub num: u8,
    pub link_up: bool,
    /// Byte 0: supported types (bits 1:0), suggested type (bit 3), default sense (bit 4).
    pub b0: u8,
    pub mtu_cap: u16,
    pub log_macs: u8,
    pub mac: [u8; 6],
    /// Byte 1 bit 7: autonegotiation enabled (ANE).
    pub autoneg: bool,
    /// Byte 0 bit 6: autonegotiation complete (ANC; 3.5, 5.13).
    pub an_complete: bool,
    /// Byte 5 & 0x6f: link speed code (5.8).
    pub speed: u8,
    /// 0x18: transceiver type (31:24) and vendor OUI (23:0).
    pub xcvr: u32,
    /// 0x1c: wavelength.
    pub wavelength: u16,
    /// 0x20: transceiver code.
    pub xcvr_code: u64,
}

impl PortInfo {
    /// The speed code's name (3.5). It is meaningful only with link up.
    pub fn speed_name(&self) -> &'static str {
        match self.speed {
            0x00 => "10G XAUI",
            0x01 => "10G XFI",
            0x02 => "1G",
            0x04 => "100M",
            0x08 => "20G",
            0x20 => "56G",
            0x40 => "40G",
            _ => "other",
        }
    }

    /// Print what QUERY_PORT says about the link and the module (3.5, 5.8),
    /// so a port without link can be matched to the switch side. `loud`:
    /// on the console even when quiet (a port without link, #16).
    pub fn print_link(&self, loud: bool) {
        note!(loud,
            "  port {}: link {}, speed code {:#04x} ({}{}), autoneg {}, {}",
            self.num,
            if self.link_up { "up" } else { "down" },
            self.speed,
            self.speed_name(),
            if self.link_up { "" } else { "; no link, so not a negotiated speed" },
            if self.autoneg { "enabled" } else { "not enabled" },
            if self.an_complete { "complete" } else { "not complete" }
        );
        if self.xcvr == 0 && self.wavelength == 0 && self.xcvr_code == 0 {
            note!(loud, "  port {}: module: no transceiver information (none plugged, or not readable)", self.num);
        } else {
            let oui = self.xcvr & 0xff_ffff;
            note!(
                loud,
                "  port {}: module: transceiver type {:#04x}, vendor OUI {:02x}:{:02x}:{:02x}, wavelength {}, code {:#018x}",
                self.num,
                self.xcvr >> 24,
                oui >> 16,
                (oui >> 8) & 0xff,
                oui & 0xff,
                self.wavelength,
                self.xcvr_code
            );
        }
    }
}

/// Everything bring-up learnt that the data path needs.
pub struct Setup {
    pub cap: DevCap,
    pub prof: Profile,
    pub ports: Vec<PortInfo>,
    /// Command interface revision 3 ("new port commands", 3.1, 5.5).
    pub rev3: bool,
}

/// The ICM tables (3.7), by index.
pub const QPC: usize = 0;
const RDMARC: usize = 1;
const ALTC: usize = 2;
const AUXC: usize = 3;
const SRQC: usize = 4;
pub const CQC: usize = 5;
pub const EQC: usize = 6;
pub const DMPT: usize = 7;
const CMPT: usize = 8;
pub const MTT: usize = 9;
pub const MCG: usize = 10;
const TABLES: usize = 11;
const NAMES: [&str; TABLES] = ["QPC", "RDMARC", "ALTC", "AUXC", "SRQC", "CQC", "EQC", "dMPT", "cMPT", "MTT", "MCG"];
/// RDMA responder entries per QP (3.7, Linux default).
const RDMARC_PER_QP: u64 = 16;

#[derive(Clone, Copy, Default)]
pub struct Table {
    pub entry: u64,
    pub count: u64,
    pub bytes: u64,
    pub base: u64,
}

pub struct Profile {
    pub t: [Table; TABLES],
    total: u64,
    /// For the data path: the special QP block (3.12) and the first MTT
    /// entry the driver may write (4.1).
    pub base_sqpn: u64,
    pub first_free_mtt: u64,
}

/// A range of ICM virtual space and the host memory backing it (3.9).
struct Icm {
    virt: u64,
    buf: DmaBuf,
}

/// A command that takes back an object the data path created (3.13, 6.4).
pub struct Undo {
    pub op: Op,
    pub op_mod: u8,
    pub in_mod: u32,
    pub in_param: u64,
}

/// What has been handed to the firmware, so teardown can take it back.
pub struct Hca {
    pub hcr: Hcr,
    /// PCI I/O's `BarIndex` for the UAR BAR, and its length if PCI I/O gave
    /// one (1.2, #15).
    pub uar_bar: u8,
    uar_size: Option<u64>,
    /// The firmware area, once MAP_FA has been issued (3.2).
    fa: Option<DmaBuf>,
    /// The auxiliary ICM pages, once MAP_ICM_AUX has been issued (3.8).
    aux: Option<DmaBuf>,
    /// Mapped ICM ranges, in mapping order (3.9).
    icm: Vec<Icm>,
    /// Which of `icm` backs the MTT table, for direct MTT writes (4.2).
    mtt: Option<usize>,
    /// INIT_HCA succeeded (3.10).
    open: bool,
    /// A command timed out or the HCR stopped answering: reset, don't talk (2.7).
    broken: bool,
    /// Commands that take back the data path's objects, in creation order;
    /// run last to first (6.4 steps 1–6).
    undo: Vec<Undo>,
    /// Queue buffers, doorbell records and frame buffers: freed with the
    /// ICM, once the firmware is stopped or the device reset (6.4).
    lent: Vec<DmaBuf>,
    /// The PCI attributes before `open`, restored by `close`.
    original: u64,
}

/// Issue MAP_FA / MAP_ICM_AUX / MAP_ICM for one buffer, 256 entries per
/// command (3.2).
fn map_pages(hcr: &mut Hcr, pci: &mut PciIo, op: Op, virt: Option<u64>, buf: &DmaBuf) -> Result<(), CmdError> {
    let mut it = dma::blocks(virt, buf.dev, buf.len() as u64).peekable();
    while it.peek().is_some() {
        hcr.inbox.zero();
        let mut n = 0;
        for b in it.by_ref().take(PAGE_LIST_MAX) {
            hcr.inbox.set_be64(n * 16, b.virt);
            hcr.inbox.set_be64(n * 16 + 8, b.phys | u64::from(b.log - 12));
            n += 1;
        }
        hcr.with_in(pci, op, 0, n as u32)?;
    }
    Ok(())
}

impl Hca {
    pub fn cmd<T>(&mut self, r: Result<T, CmdError>) -> Result<T, Fail> {
        r.map_err(|e| {
            if e.needs_reset() {
                self.broken = true;
            }
            Fail
        })
    }

    /// A command timed out or the HCR stopped answering (2.7): issue no
    /// optional commands.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// Remember how to take an object back once it exists.
    pub fn push_undo(&mut self, op: Op, op_mod: u8, in_mod: u32, in_param: u64) {
        self.undo.push(Undo { op, op_mod, in_mod, in_param });
    }

    /// Where the undo stack stands, for `undo_to`.
    pub fn mark(&self) -> usize {
        self.undo.len()
    }

    /// Take back every object created since `mark`, newest first. A failure
    /// marks the device broken (teardown then resets it) and stops here.
    pub fn undo_to(&mut self, pci: &mut PciIo, mark: usize) -> Result<(), Fail> {
        while self.undo.len() > mark {
            if self.broken {
                return Err(Fail);
            }
            let u = self.undo.pop().unwrap();
            if self.hcr.imm(pci, u.op, u.op_mod, u.in_mod, u.in_param).is_err() {
                self.broken = true;
                return Err(Fail);
            }
        }
        Ok(())
    }

    /// Zeroed DMA memory for the device, kept until teardown (see `lent`).
    pub fn lend(&mut self, pci: &mut PciIo, what: &str, bytes: usize) -> Result<Mem, Fail> {
        let b = DmaBuf::new(pci, bytes).map_err(|e| Fail::log(what, e.status()))?;
        let m = b.mem();
        self.lent.push(b);
        Ok(m)
    }

    /// The host memory backing the MTT table (4.2).
    pub fn mtt_mem(&self) -> Mem {
        self.icm[self.mtt.expect("MTT table mapped")].buf.mem()
    }

    fn bring_up(&mut self, pci: &mut PciIo) -> Result<Setup, Fail> {
        let fw = self.query_fw(pci)?;

        // 3.2: lend the firmware its memory, then start it.
        let fa = DmaBuf::new(pci, fw.pages as usize * PAGE).map_err(|e| Fail::log("FW area", e.status()))?;
        trace!("  FW area: {} KiB at {:#x}", fa.len() / 1024, fa.dev);
        let fa = self.fa.insert(fa);
        let r = map_pages(&mut self.hcr, pci, hcr::MAP_FA, None, fa);
        self.cmd(r)?;
        let r = self.hcr.imm(pci, hcr::RUN_FW, 0, 0, 0);
        self.cmd(r)?;

        // 3.3: 4 KiB device pages. Linux only warns when this fails.
        self.hcr.inbox.zero();
        self.hcr.inbox.set_u8(0x02, 1);
        self.hcr.inbox.set_u8(0x03, 0);
        if let Err(e) = self.hcr.with_in(pci, hcr::MOD_STAT_CFG, 0, 0) {
            if e.needs_reset() {
                return self.cmd(Err(e));
            }
        }

        let mut cap = self.query_dev_cap(pci)?;
        let mut ports = Vec::new();
        if fw.rev >= 3 {
            for port in 1..=cap.ports {
                ports.push(self.query_port(pci, port)?);
            }
        } else {
            trace!("  command interface revision 2: QUERY_PORT not used");
        }
        trace!(
            "  steering: {} (VEP_UC_STEER {}, VEP_MC_STEER {})",
            if cap.b0() { "B0" } else { "A0" },
            cap.flag(41) as u8,
            cap.flag(42) as u8
        );

        let prof = profile(&cap)?;

        // 3.8: firmware's auxiliary memory for this ICM size.
        let r = self.hcr.imm(pci, hcr::SET_ICM_SIZE, 0, 0, prof.total);
        let aux_pages = self.cmd(r)?;
        trace!("  SET_ICM_SIZE {:#x}: firmware asks for {aux_pages} auxiliary pages", prof.total);
        let aux = DmaBuf::new(pci, aux_pages as usize * PAGE).map_err(|e| Fail::log("aux pages", e.status()))?;
        let aux = self.aux.insert(aux);
        let r = map_pages(&mut self.hcr, pci, hcr::MAP_ICM_AUX, None, aux);
        self.cmd(r)?;

        self.map_icm(pci, &prof)?;
        self.init_hca(pci, &cap, &prof)?;

        if cap.num_sys_eqs != 0 {
            // 3.11
            let r = self.hcr.with_out(pci, hcr::QUERY_FUNC, 0, 0);
            self.cmd(r)?;
            let o = &self.hcr.outbox;
            cap.rsvd_eqs = u64::from(o.be16(0x04));
            cap.max_eqs = u64::from(o.be16(0x06));
            cap.rsvd_uars = u64::from(o.u8(0x0b) & 0xf);
            trace!(
                "  QUERY_FUNC: reserved EQs {}, max EQs {}, reserved UARs {}",
                cap.rsvd_eqs, cap.max_eqs, cap.rsvd_uars
            );
        }
        self.query_adapter(pci);
        Ok(Setup { cap, prof, ports, rev3: fw.rev >= 3 })
    }

    fn query_fw(&mut self, pci: &mut PciIo) -> Result<FwInfo, Fail> {
        match self.hcr.with_out(pci, hcr::QUERY_FW, 0, 0) {
            Ok(()) => {}
            Err(CmdError::Status(hcr::STATUS_MULTI_FUNC)) => {
                return Err(fail!("not the primary physical function; leaving it"))
            }
            Err(e) => return self.cmd(Err(e)),
        }
        let o = &self.hcr.outbox;
        let pages = u64::from(o.be16(0x00));
        let (major, subminor, minor) = (o.be16(0x02), o.be16(0x04), o.be16(0x06));
        let rev = o.be16(0x0a);
        let catas_off = o.be64(0x30);
        let catas_size = o.be32(0x38);
        // 1.2: the field × 2 is a BAR register; register 2 is the UAR BAR,
        // whose BarIndex the firmware decides (#15).
        let catas_reg = (o.u8(0x3c) >> 6) * 2;
        let catas_bar = if usize::from(catas_reg) == UAR_REG { self.uar_bar } else { catas_reg };
        trace!(
            "  firmware {major}.{minor}.{subminor}, command interface revision {rev}, PPF {}, log max commands {}",
            o.u8(0x09),
            o.u8(0x0f)
        );
        trace!(
            "  FW area {pages} pages; catastrophic error buffer BAR {catas_bar} + {catas_off:#x}, {catas_size} dwords"
        );
        self.hcr.catas = Some((catas_bar, catas_off));
        if !(2..=3).contains(&rev) {
            return Err(fail!("command interface revision {rev} is not 2 or 3; stopping"));
        }
        Ok(FwInfo { pages, rev })
    }

    fn query_dev_cap(&mut self, pci: &mut PciIo) -> Result<DevCap, Fail> {
        let r = self.hcr.with_out(pci, hcr::QUERY_DEV_CAP, 0, 0);
        self.cmd(r)?;
        let o = &self.hcr.outbox;
        // Spec 7 asks for the raw bytes 0x10–0xa7.
        for line in (0x10..0xa8).step_by(0x20) {
            let mut s = String::new();
            for i in line..(line + 0x20).min(0xa8) {
                let _ = write!(s, " {:02x}", o.u8(i));
            }
            trace!("  DEV_CAP {line:02x}:{s}");
        }
        let lg = |off: usize, mask: u8| 1u64 << (o.u8(off) & mask);
        let num_sys_eqs = u64::from(o.be16(0x26) & 0xfff);
        let rsvd_uars = u64::from(o.u8(0x48) >> 4);
        let mut rsvd_eqs = lg(0x1e, 0xf);
        if num_sys_eqs == 0 {
            rsvd_eqs = rsvd_eqs.max(4 * rsvd_uars);
        }
        let mut entry = [0u64; TABLES];
        for (i, off) in [(QPC, 0x82), (RDMARC, 0x80), (ALTC, 0x86), (AUXC, 0x84), (SRQC, 0x8c), (CQC, 0x8a), (EQC, 0x88), (DMPT, 0x92), (CMPT, 0x8e), (MTT, 0x90)] {
            entry[i] = u64::from(o.be16(off));
        }
        entry[MCG] = 1 << LOG_MGM_ENTRY;
        let flags2 = o.be32(0x70);
        let cap = DevCap {
            rsvd_qps: lg(0x12, 0xf),
            rsvd_srqs: 1 << (o.u8(0x14) >> 4),
            rsvd_cqs: lg(0x1a, 0xf),
            rsvd_eqs,
            max_eqs: lg(0x1f, 0xf),
            rsvd_mtts: 1 << (o.u8(0x20) >> 4),
            rsvd_mpts: lg(0x22, 0xf),
            rsvd_uars,
            rsvd_pds: u64::from(o.u8(0x64) >> 4),
            log_max_qp_wqes: o.u8(0x11),
            log_max_cqes: o.u8(0x19),
            num_sys_eqs,
            ports: o.u8(0x37) & 0xf,
            flags: o.be64(0x40),
            flags2,
            bmme: o.be32(0x94),
            eth_prot_ctrl: o.u8(0x7a) & 0x20 != 0,
            eth_backpl_an_rep: o.be32(0x9c) & 1 != 0,
            uar_bytes: 1 << ((o.u8(0x49) & 0x3f) + 20),
            entry,
            max_icm: o.be64(0xa0),
        };
        let log_min_page = o.u8(0x4b);
        trace!(
            "  DEV_CAP: {} ports, flags {:016x}, flags2 {flags2:08x}, BMME {:08x}, max ICM {:#x}",
            cap.ports, cap.flags, cap.bmme, cap.max_icm
        );
        trace!(
            "  reserved: QPs {}, SRQs {}, CQs {}, EQs {} (max {}, sys {}), MTTs {}, MPTs {}, UARs {rsvd_uars}, PDs {}",
            cap.rsvd_qps, cap.rsvd_srqs, cap.rsvd_cqs, cap.rsvd_eqs, cap.max_eqs, num_sys_eqs, cap.rsvd_mtts, cap.rsvd_mpts,
            cap.rsvd_pds
        );
        trace!("  log max: WQEs per QP {}, CQEs per CQ {}", cap.log_max_qp_wqes, cap.log_max_cqes);
        trace!("  SW_CQ_INIT {}, LB_SRC_CHK {}", (flags2 >> 23) & 1, (flags2 >> 19) & 1);
        trace!(
            "  ETH_PROT_CTRL {} (0x7a {:#04x}), ETH_BACKPL_AN_REP {} (0x9c {:08x})",
            cap.eth_prot_ctrl as u8,
            o.u8(0x7a),
            cap.eth_backpl_an_rep as u8,
            o.be32(0x9c)
        );

        // 6.1 step 8.
        if log_min_page > 12 {
            return Err(fail!("minimum page 2^{log_min_page} is above 4 KiB; stopping"));
        }
        if !(1..=2).contains(&cap.ports) {
            return Err(fail!("{} ports reported; stopping", cap.ports));
        }
        let uar_pages = cap.uar_bytes / PAGE as u64;
        if uar_pages <= 128 {
            return Err(fail!("UAR area has {uar_pages} pages, 128 or fewer: firmware log2_uar_bar_megabytes too small; stopping"));
        }
        let bar = self.uar_bar;
        match self.uar_size {
            Some(size) if cap.uar_bytes > size => {
                return Err(fail!("UAR area {:#x} is larger than the UAR BAR ({size:#x}); stopping", cap.uar_bytes))
            }
            Some(size) => trace!("  UAR area {:#x} ({uar_pages} pages), UAR BAR (index {bar}) {size:#x}", cap.uar_bytes),
            None => trace!("  UAR area {:#x} ({uar_pages} pages); UAR BAR (index {bar}) size unknown", cap.uar_bytes),
        }
        Ok(cap)
    }

    pub fn query_port(&mut self, pci: &mut PciIo, port: u8) -> Result<PortInfo, Fail> {
        let r = self.hcr.with_out(pci, hcr::QUERY_PORT, 0, u32::from(port));
        self.cmd(r)?;
        let info = self.port_info(port);
        if self.hcr.quiet {
            return Ok(info);
        }
        let o = &self.hcr.outbox;
        let b0 = o.u8(0x00);
        let types = match b0 & 3 {
            1 => "IB",
            2 => "Ethernet",
            3 => "IB/Ethernet (VPI)",
            _ => "none",
        };
        let mac: [u8; 6] = core::array::from_fn(|i| o.u8(0x12 + i));
        trace!(
            "  port {port}: supports {types}, suggests {}, MTU cap {}, log max MACs {}",
            if b0 & 0x08 != 0 { "Ethernet" } else { "IB" },
            o.be16(0x02),
            o.u8(0x0a) & 0xf
        );
        trace!(
            "  port {port}: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        info.print_link(false);
        Ok(info)
    }

    /// Decode the QUERY_PORT output mailbox (3.5).
    fn port_info(&self, port: u8) -> PortInfo {
        let o = &self.hcr.outbox;
        PortInfo {
            num: port,
            link_up: o.u8(0x00) & 0x80 != 0,
            b0: o.u8(0x00),
            mtu_cap: o.be16(0x02),
            log_macs: o.u8(0x0a) & 0xf,
            mac: core::array::from_fn(|i| o.u8(0x12 + i)),
            autoneg: o.u8(0x01) & 0x80 != 0,
            an_complete: o.u8(0x00) & 0x40 != 0,
            speed: o.u8(0x05) & 0x6f,
            xcvr: o.be32(0x18),
            wavelength: o.be16(0x1c),
            xcvr_code: o.be64(0x20),
        }
    }

    /// MAP_ICM every table up front, in Linux's order (3.9).
    fn map_icm(&mut self, pci: &mut PciIo, p: &Profile) -> Result<(), Fail> {
        let c = &p.t[CMPT];
        let sub = c.entry << 24;
        let page = |n: u64| n.div_ceil(PAGE as u64) * PAGE as u64;
        let whole = |i: usize| (NAMES[i], p.t[i].base, p.t[i].bytes);
        let ranges = [
            ("cMPT QP", c.base, page(p.t[QPC].count * c.entry)),
            ("cMPT SRQ", c.base + sub, page(p.t[SRQC].count * c.entry)),
            ("cMPT CQ", c.base + 2 * sub, page(p.t[CQC].count * c.entry)),
            ("cMPT EQ", c.base + 3 * sub, page(p.t[EQC].count * c.entry)),
            whole(EQC),
            whole(MTT),
            whole(DMPT),
            whole(QPC),
            whole(AUXC),
            whole(ALTC),
            whole(RDMARC),
            whole(CQC),
            whole(SRQC),
            whole(MCG),
        ];
        let total: u64 = ranges.iter().map(|r| r.2).sum();
        trace!("  MAP_ICM: {} ranges, {} KiB of host memory", ranges.len(), total / 1024);
        for (name, virt, bytes) in ranges {
            let buf = DmaBuf::new(pci, bytes as usize).map_err(|e| Fail::log(name, e.status()))?;
            let r = map_pages(&mut self.hcr, pci, hcr::MAP_ICM, Some(virt), &buf);
            // Kept even when the map failed part-way, so teardown unmaps it.
            if name == NAMES[MTT] {
                self.mtt = Some(self.icm.len());
            }
            self.icm.push(Icm { virt, buf });
            if r.is_err() {
                alarm!("  MAP_ICM {name} at {virt:#x} failed");
            }
            self.cmd(r)?;
        }
        Ok(())
    }

    /// INIT_HCA (3.10).
    fn init_hca(&mut self, pci: &mut PciIo, cap: &DevCap, p: &Profile) -> Result<(), Fail> {
        let m = &self.hcr.inbox;
        m.zero();
        m.set_u8(0x000, 2);
        m.set_u8(0x00e, (((log2(CACHE_LINE) - 4) << 5) | 0x10) as u8);
        let mut flags = 1u32; // check port for UD address vectors
        if cap.flag(7) {
            flags |= 1 << 3;
        }
        if cap.flag(48) {
            flags |= 1 << 4;
        }
        if cap.flag(52) {
            flags |= 1 << 13;
        }
        m.set_be32(0x014, flags);
        let t = &p.t;
        // Each base is at least 4 KiB aligned, so its low byte is free for the log.
        let base_log = |base_off: usize, i: usize, log: u32| {
            m.set_be64(base_off, t[i].base);
            m.set_u8(base_off + 7, log as u8);
        };
        base_log(0x030, QPC, log2(t[QPC].count));
        base_log(0x048, SRQC, log2(t[SRQC].count));
        base_log(0x050, CQC, log2(t[CQC].count));
        let log_eqs = if cap.num_sys_eqs != 0 { 0x1f } else { log2(t[EQC].count) };
        base_log(0x080, EQC, log_eqs);
        base_log(0x090, RDMARC, log2(RDMARC_PER_QP));
        m.set_be64(0x060, t[ALTC].base);
        m.set_be64(0x070, t[AUXC].base);
        m.set_be16(0x08a, cap.num_sys_eqs as u16);
        let log_mcgs = log2(t[MCG].count);
        m.set_be64(0x0c0, t[MCG].base);
        m.set_u8(0x0d3, LOG_MGM_ENTRY as u8);
        m.set_u8(0x0d7, (log_mcgs - 1) as u8);
        m.set_u8(0x0d8, if cap.b0() { 1 << 3 } else { 0 });
        m.set_u8(0x0db, log_mcgs as u8);
        m.set_be64(0x0f0, t[DMPT].base);
        let windows = cap.flag(16) || cap.bmme & (1 << 9) != 0;
        m.set_u8(0x0f8, if windows { 0x80 } else { 0 });
        m.set_u8(0x0fb, log2(t[DMPT].count) as u8);
        m.set_be64(0x100, t[MTT].base);
        m.set_be64(0x108, t[CMPT].base);
        m.set_u8(0x12a, log2(cap.uar_bytes / PAGE as u64) as u8);
        m.set_u8(0x12b, 0);
        trace!("  INIT_HCA: flags {flags:#x}, log EQs {log_eqs:#x}, memory windows {}", windows as u8);
        let r = self.hcr.with_in(pci, hcr::INIT_HCA, 0, 0);
        self.cmd(r)?;
        self.open = true;
        Ok(())
    }

    /// QUERY_ADAPTER: the board ID, for the log only (3.6).
    fn query_adapter(&mut self, pci: &mut PciIo) {
        if self.hcr.with_out(pci, hcr::QUERY_ADAPTER, 0, 0).is_err() {
            return;
        }
        let o = &self.hcr.outbox;
        const VSD: usize = 0x20;
        let topspin = o.be16(VSD) == 0x05ad && o.be16(VSD + 0xde) == 0x05ad;
        let byte = |i: usize| if topspin { o.u8(VSD + 0x20 + i) } else { o.u8(VSD + 0xd0 + (i & !3) + (3 - (i & 3))) };
        let id: String = (0..16).map(byte).take_while(|&b| b != 0).map(|b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
        trace!("  board ID: {id}");
    }

    /// Take back the data path's objects, then CLOSE_HCA, UNMAP_ICM (reverse
    /// order), UNMAP_ICM_AUX, UNMAP_FA (6.4 steps 1–8, 3.13). Nothing is
    /// freed. False when a command failed or one had already timed out.
    fn stop_firmware(&mut self, pci: &mut PciIo) -> bool {
        let mut ok = !self.broken && self.undo_to(pci, 0).is_ok();
        if ok && self.open {
            ok = self.hcr.imm(pci, hcr::CLOSE_HCA, 0, 0, 0).is_ok();
        }
        if ok {
            for r in self.icm.iter().rev() {
                let pages = (r.buf.len() / PAGE) as u32;
                if self.hcr.imm(pci, hcr::UNMAP_ICM, 0, pages, r.virt).is_err() {
                    ok = false;
                    break;
                }
            }
        }
        if ok && self.aux.is_some() {
            ok = self.hcr.imm(pci, hcr::UNMAP_ICM_AUX, 0, 0, 0).is_ok();
        }
        if ok && self.fa.is_some() {
            ok = self.hcr.imm(pci, hcr::UNMAP_FA, 0, 0, 0).is_ok();
        }
        ok
    }

    /// `stop_firmware`, then free the memory. If a command failed, or one
    /// already timed out, the device is reset instead (2.8). Memory the
    /// firmware may still use is never freed. True when everything went
    /// back cleanly.
    fn teardown(mut self, pci: &mut PciIo) -> bool {
        let ok = self.stop_firmware(pci);
        if !ok {
            say!("  teardown by command failed; resetting the device instead");
            if reset(pci).is_err() {
                // The firmware may still own this memory: leave it allocated.
                say!("  device did not reset; its memory stays allocated");
                core::mem::forget(self);
                return false;
            }
        }
        // SAFETY: the firmware has given every buffer back (UNMAP_FA) or the
        // device was reset (1.6, 3.13), and no command is in flight.
        unsafe {
            for b in self.lent.drain(..) {
                b.free(pci);
            }
            for r in self.icm.drain(..) {
                r.buf.free(pci);
            }
            if let Some(b) = self.aux.take() {
                b.free(pci);
            }
            if let Some(b) = self.fa.take() {
                b.free(pci);
            }
            self.hcr.free(pci);
        }
        trace!("  firmware stopped, memory returned");
        ok
    }
}

/// The ICM profile (3.7), with the recommended minimal counts, laid out
/// the way Linux's `mlx4_make_profile` does: largest table first.
fn profile(cap: &DevCap) -> Result<Profile, Fail> {
    for (i, &e) in cap.entry.iter().enumerate() {
        if !e.is_power_of_two() {
            return Err(fail!("{} entry size {e} is not a power of two; stopping", NAMES[i]));
        }
    }
    let base_sqpn = pow2(cap.rsvd_qps + 257);
    let num_qps = 2 * base_sqpn;
    // 4.1: the reserved MTT span, rounded to the cache line, then to a power of two.
    let mtt = cap.entry[MTT];
    let aligned = (cap.rsvd_mtts * mtt).div_ceil(CACHE_LINE) * CACHE_LINE / mtt;
    let first_free_mtt = pow2(aligned);
    let num_eqs = if cap.num_sys_eqs != 0 { cap.num_sys_eqs } else { cap.max_eqs.min(128) };

    let mut count = [0u64; TABLES];
    count[QPC] = num_qps;
    count[RDMARC] = num_qps * RDMARC_PER_QP;
    count[ALTC] = num_qps;
    count[AUXC] = num_qps;
    count[SRQC] = cap.rsvd_srqs + 1;
    // Two CQs per port, two ports at most (4.1).
    count[CQC] = (cap.rsvd_cqs + 4).max(64);
    count[EQC] = num_eqs;
    count[DMPT] = (cap.rsvd_mpts + 1).max(64);
    count[CMPT] = 4 << 24;
    count[MTT] = first_free_mtt + 256;
    count[MCG] = 256;

    let mut t = [Table::default(); TABLES];
    for i in 0..TABLES {
        let c = pow2(count[i]);
        t[i] = Table { entry: cap.entry[i], count: c, bytes: (c * cap.entry[i]).max(PAGE as u64), base: 0 };
    }
    let mut order: [usize; TABLES] = core::array::from_fn(|i| i);
    order.sort_by(|&a, &b| t[b].bytes.cmp(&t[a].bytes));
    let mut total = 0u64;
    for &i in &order {
        t[i].base = total;
        total += t[i].bytes;
    }
    for &i in &order {
        trace!(
            "  ICM {:<6} {:#011x}: {} x {} bytes",
            NAMES[i], t[i].base, t[i].count, t[i].entry
        );
    }
    trace!("  ICM total {total:#x}; base special QPN {base_sqpn:#x}, first free MTT {first_free_mtt}");
    if cap.rsvd_eqs >= t[EQC].count {
        say!("  note: reserved EQs {} leave no EQ in a {}-entry table (#3)", cap.rsvd_eqs, t[EQC].count);
    }
    if total > cap.max_icm {
        return Err(fail!("ICM total {total:#x} exceeds the device's {:#x}; stopping", cap.max_icm));
    }
    Ok(Profile { t, total, base_sqpn, first_free_mtt })
}
