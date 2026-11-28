//! The Ethernet data path: the objects one port needs, and polling send and
//! receive for the SNP (docs/spec/connectx3.md sections 4–6; section numbers
//! in comments are that document's).
//!
//! `open` creates the shared objects in Linux's order (EQ and MAP_EQ,
//! CONF_SPECIAL_QP, the physical memory region), then brings up every
//! Ethernet port: CQs, QPs, SET_PORT, INIT_PORT and steering (6.1 steps
//! 15–27). Every object created pushes the command that takes it back onto
//! the `Hca` undo stack, so `fw::close` and `fw::quiesce` run 6.4's order.
//! Object numbers are per port (4.1), so both ports can be up at once.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::time::Duration;

use crate::time as boot;

use crate::diag;
use crate::dma::{Mem, PAGE};
use crate::fw::{self, fail, Fail, Hca, PortInfo, Setup};
use crate::hcr;
use crate::mcg::{self, Amgm, Entry};
use crate::pci::PciIo;
use crate::sent::Sent;

/// Queue sizes: EQ entries (4.6), receive descriptors (5.6), send TXBBs (5.7).
const EQ_ENTRIES: u32 = 256;
const RX_ENTRIES: u32 = 256;
const TX_TXBBS: u32 = 128;
/// Entry sizes: EQE and CQE (32 bytes, 4.6/4.7), TXBB (5.7), receive stride (5.6).
const EQE: usize = 32;
const CQE: usize = 32;
const TXBB: usize = 64;
const RX_STRIDE: usize = 16;
/// One receive buffer per descriptor, one bounce buffer per TXBB (5.6).
const BUF: usize = 2048;
/// TXBBs the device may prefetch past the producer (5.7).
const HEADROOM: u32 = 33;
/// SET_PORT frame size: MTU 1500 + 14 + 8 + 4 (5.9).
const PORT_MTU: u16 = 1526;
/// Shortest frame sent; the device would pad, padding here is harmless (5.7).
const MIN_FRAME: usize = 60;
/// Events mapped to the EQ: CQ error, WQ catastrophic, local catastrophic,
/// port change, WQ invalid request, WQ access error; not command completion (4.6).
const EVENT_MASK: u64 = 1 << 0x04 | 1 << 0x05 | 1 << 0x08 | 1 << 0x09 | 1 << 0x10 | 1 << 0x11;
/// CQE opcode of an error completion, and the send opcode (4.7, 5.7).
const CQE_ERROR: u8 = 0x1e;
const OP_SEND: u32 = 0x0a;
/// Largest frame the SNP hands over: MTU 1500 + the 14-byte header (5.9).
pub const MAX_FRAME: usize = 1514;
/// Sent buffers waiting for `GetStatus` to hand them back, plus frames in
/// flight; at this many `send` refuses (`Full`) until the caller collects
/// them. None is ever forgotten (`sent.rs`, finding 6aeccbfbe450, #37).
const DONE_MAX: usize = 1024;

fn ms(n: u32) -> Duration {
    Duration::from_millis(u64::from(n))
}

fn wmb() {
    kcore::barrier::dma_wmb();
}

/// A run of MTT entries in a context: byte offset bits 39:32 and 31:0 (4.2).
fn set_mtt(m: &Mem, hi: usize, lo: usize, offset: u64) {
    m.set_u8(hi, (offset >> 32) as u8);
    m.set_be32(lo, offset as u32);
}

pub struct Mac(pub [u8; 6]);

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = &self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

/// MTT entries, written straight into the ICM that backs the MTT table (4.2),
/// allocated upward from the first free entry, each run aligned to its
/// rounded-up size as Linux's buddy allocator would.
struct Mtt {
    mem: Mem,
    entry: u64,
    next: u64,
    end: u64,
}

impl Mtt {
    /// Describe `buf` (whole 4 KiB pages) and return the run's byte offset.
    fn map(&mut self, buf: Mem) -> Result<u64, Fail> {
        let n = (buf.len() / PAGE) as u64;
        let first = self.next.next_multiple_of(fw::pow2(n));
        if first + n > self.end {
            return Err(fail!("MTT table full: {n} entries from {first}, table ends at {}", self.end));
        }
        for i in 0..n {
            self.mem.set_be64(((first + i) * self.entry) as usize, (buf.dev + i * PAGE as u64) | 1);
        }
        // Coherent memory: a store fence before the create command suffices.
        wmb();
        self.next = first + n;
        Ok(first * self.entry)
    }
}

/// The event queue (4.6), polled.
struct Eq {
    eqn: u32,
    /// PCI I/O's `BarIndex` for the UAR BAR (`Hca::uar_bar`).
    bar: u8,
    mem: Mem,
    ci: u32,
    /// MAP_EQ succeeded: port changes arrive as events (5.8).
    mapped: bool,
}

impl Eq {
    /// Consume and log every event, writing the consumer index at least
    /// every 0x80 entries (4.6). Port changes update the port's link state.
    fn poll(&mut self, pci: &mut PciIo, ports: &mut [Port]) {
        let mut n = 0;
        loop {
            let e = self.mem.slice((self.ci % EQ_ENTRIES) as usize * EQE, EQE);
            if u32::from(e.u8(0x1f) >> 7) != (self.ci / EQ_ENTRIES) & 1 {
                break;
            }
            kcore::barrier::dma_rmb();
            let (kind, sub) = (e.u8(0x01), e.u8(0x03));
            let obj = e.be32(0x04) & 0xff_ffff;
            match kind {
                0x09 => {
                    let num = (e.be32(0x0c) >> 28) as u8;
                    let up = match sub {
                        1 => Some(false),
                        4 => Some(true),
                        _ => None,
                    };
                    match (up, ports.iter_mut().find(|p| p.num == num)) {
                        (Some(up), Some(p)) => p.set_link(up),
                        _ => trace!("  event: port {num} change, subtype {sub:#04x}"),
                    }
                }
                0x04 => say!(
                    "  event: CQ {obj:#x} error, {}",
                    if e.u8(0x0f) == 1 { "overrun" } else { "access violation" }
                ),
                0x05 | 0x10 | 0x11 => say!("  event: QP {obj:#x} error, type {kind:#04x}"),
                0x08 => say!("  event: local catastrophic error"),
                _ => trace!("  event: type {kind:#04x} subtype {sub:#04x}, data {obj:#x}"),
            }
            self.ci = self.ci.wrapping_add(1);
            n += 1;
            if n % 0x80 == 0 {
                self.doorbell(pci);
            }
        }
        if n % 0x80 != 0 {
            self.doorbell(pci);
        }
    }

    /// Consumer index, not re-armed (4.6), in UAR page eqn / 4 (1.4).
    fn doorbell(&self, pci: &mut PciIo) {
        let off = u64::from(self.eqn / 4) * PAGE as u64 + 0x800 + 8 * u64::from(self.eqn % 4);
        let _ = pci.mem_write32(self.bar, off, (self.ci & 0xff_ffff).to_be());
    }
}

/// A completion queue (4.7) and its doorbell record.
struct Cq {
    n: u32,
    mem: Mem,
    db: Mem,
    ci: u32,
}

impl Cq {
    /// The next completion, if software owns it.
    fn peek(&self) -> Option<Mem> {
        let e = self.mem.slice((self.ci % self.n) as usize * CQE, CQE);
        if u32::from(e.u8(0x1f) >> 7) != (self.ci / self.n) & 1 {
            return None;
        }
        kcore::barrier::dma_rmb();
        Some(e)
    }

    /// Done with it: tell the device through the doorbell record.
    fn pop(&mut self) {
        self.ci = self.ci.wrapping_add(1);
        self.db.set_be32(0, self.ci & 0xff_ffff);
    }
}

/// Objects shared by the ports (4.1, 4.3, 4.6).
struct Shared {
    pd: u32,
    uar: u32,
    lkey: u32,
    eq: Eq,
    mtt: Mtt,
    /// The MCG table's overflow (AMGM) entries, fresh and freed (5.4.1).
    amgm: Amgm,
    b0: bool,
    /// VEP_MC_STEER: MGID_HASH op_mod and the RQP_CALC multicast mode.
    mc_steer: bool,
    /// INIT_HCA enabled counters (capability bit 48).
    counters: bool,
    lb_src_chk: bool,
    port_remap: bool,
    sw_cq_init: bool,
}

/// One port's queues, as the SNP uses them.
pub struct Port {
    num: u8,
    mac: [u8; 6],
    link_up: bool,
    /// The rate QUERY_PORT reported when it last saw link (3.5,
    /// `module::rate`), or "", for the one-line summary.
    speed: &'static str,
    /// The module, from its EEPROM when last read (5.11, `module::media`).
    media: String,
    uar: u32,
    /// PCI I/O's `BarIndex` for the UAR BAR (`Hca::uar_bar`).
    uar_bar: u8,
    lkey: u32,
    rx_qpn: u32,
    tx_qpn: u32,
    rx_cq: Cq,
    tx_cq: Cq,
    /// The RX doorbell record and the frame buffers.
    rx_db: Mem,
    rx_bufs: Mem,
    rx_prod: u32,
    sq: Mem,
    tx_bufs: Mem,
    tx_prod: u32,
    tx_done: u32,
    /// The caller's buffer for each send slot, and the ones sent (6.2 step 4).
    tokens: Vec<usize>,
    done: Sent,
    /// An error completion put the TX QP in the error state (5.7).
    tx_broken: bool,
    /// Multicast MACs attached to the RX QP (B0): the SNP's multicast list,
    /// plus any whose detach failed (`mcg::sync`, finding 627bcdb9b490, #36).
    joined: Vec<[u8; 6]>,
    /// The link changed; its speed and module are not printed yet (`report_news`).
    news: bool,
    /// The bring-up link wait is over (`wait_link`). Until then a link change is
    /// trace only: the port's one line after Start reports the link (#16).
    settled: bool,
}

/// Every Ethernet port, up, and what they share.
pub struct Eth {
    sh: Shared,
    pub ports: Vec<Port>,
    /// QUERY_DEV_CAP ETH_PROT_CTRL and ETH_BACKPL_AN_REP (5.12, 5.13).
    ptys_offered: bool,
    an_rep: bool,
    /// The firmware answered a PTYS query; `Some(false)` stops asking.
    ptys_answers: Option<bool>,
}

/// Create the shared objects and bring up every Ethernet port (6.1 steps
/// 15–27). No port at all is `Ok` with an empty list.
pub fn open(hca: &mut Hca, pci: &mut PciIo, s: &Setup) -> Result<Eth, Fail> {
    if !s.rev3 {
        return Err(fail!("data path: needs command interface revision 3 (5.5)"));
    }
    let sh = shared(hca, pci, s)?;
    let mut eth = Eth {
        sh,
        ports: Vec::new(),
        ptys_offered: s.cap.eth_prot_ctrl,
        an_rep: s.cap.eth_backpl_an_rep,
        ptys_answers: None,
    };
    for p in &s.ports {
        if is_ethernet(hca, pci, s, p)? {
            let port = port_up(hca, pci, &mut eth.sh, s, p)?;
            trace!("  port {}: up, link {}", p.num, if port.link_up { "up" } else { "down" });
            eth.ports.push(port);
        }
    }
    Ok(eth)
}

impl Eth {
    /// Poll the EQ: port changes and errors (6.3).
    pub fn poll(&mut self, pci: &mut PciIo) {
        self.sh.eq.poll(pci, &mut self.ports);
    }

    /// Port changes arrive as events (MAP_EQ worked); otherwise the caller
    /// polls QUERY_PORT with `refresh_link`.
    pub fn events(&self) -> bool {
        self.sh.eq.mapped
    }

    /// QUERY_PORT, quietly, for port `i`'s link state (5.8).
    pub fn refresh_link(&mut self, hca: &mut Hca, pci: &mut PciIo, i: usize) -> Result<(), Fail> {
        hca.hcr.quiet = true;
        let r = hca.query_port(pci, self.ports[i].num);
        hca.hcr.quiet = false;
        self.ports[i].set_link(r?.link_up);
        Ok(())
    }

    /// Print the speed and module of every port whose link changed since
    /// (the event handler has no HCA to ask QUERY_PORT with), and at
    /// link-down the module EEPROM and PTYS too (#17).
    pub fn report_news(&mut self, hca: &mut Hca, pci: &mut PciIo) {
        for i in 0..self.ports.len() {
            if core::mem::take(&mut self.ports[i].news) {
                self.report_link(hca, pci, i);
                if !self.ports[i].link_up {
                    self.diagnose(hca, pci, i, true);
                }
            }
        }
    }

    /// The DAC diagnostics of #17 for port `i`: the module EEPROM (5.11)
    /// and the PTYS link modes (5.12), read-only. A firmware that refused
    /// PTYS once is not asked again. `loud`: on the console even when quiet
    /// (a port without link, #16).
    fn diagnose(&mut self, hca: &mut Hca, pci: &mut PciIo, i: usize, loud: bool) {
        if hca.broken() {
            return;
        }
        let num = self.ports[i].num;
        self.ports[i].media = diag::module(hca, pci, num, loud);
        if self.ptys_answers != Some(false) && !hca.broken() {
            self.ptys_answers = Some(diag::ptys(hca, pci, num, self.ptys_offered, loud));
        }
    }

    /// Print port `i`'s speed and module, as QUERY_PORT reports them now.
    fn report_link(&mut self, hca: &mut Hca, pci: &mut PciIo, i: usize) {
        hca.hcr.quiet = true;
        let r = hca.query_port(pci, self.ports[i].num);
        hca.hcr.quiet = false;
        match r {
            Ok(info) => {
                info.print_link(!info.link_up);
                if info.link_up {
                    self.ports[i].speed = crate::module::rate(info.speed);
                }
            }
            Err(_) => say!("  port {}: QUERY_PORT failed; no link details", self.ports[i].num),
        }
    }

    /// Wait up to `max_ms` for every port to have link, so the first
    /// consumer of the SNP does not start on a port still training.
    pub fn wait_link(&mut self, hca: &mut Hca, pci: &mut PciIo, max_ms: u32) {
        self.wait_link_inner(hca, pci, max_ms);
        for p in &mut self.ports {
            p.settled = true;
        }
    }

    fn wait_link_inner(&mut self, hca: &mut Hca, pci: &mut PciIo, max_ms: u32) {
        let mut waited = 0;
        loop {
            self.poll(pci);
            for i in 0..self.ports.len() {
                if !self.ports[i].link_up && self.refresh_link(hca, pci, i).is_err() {
                    return;
                }
            }
            if self.ports.iter().all(|p| p.link_up) {
                trace!("  link up on every Ethernet port after {waited} ms");
                for i in 0..self.ports.len() {
                    self.ports[i].news = false;
                    self.report_link(hca, pci, i);
                    self.diagnose(hca, pci, i, false);
                }
                diag::speed_control(self.ptys_offered, self.an_rep, false);
                return;
            }
            if waited >= max_ms {
                for i in 0..self.ports.len() {
                    self.ports[i].news = false;
                    if !self.ports[i].link_up {
                        let num = self.ports[i].num;
                        say!("stormnic-mlx4: port {num}: no link after {} s; reported as no media", max_ms / 1000);
                    }
                    self.report_link(hca, pci, i);
                    let loud = !self.ports[i].link_up;
                    self.diagnose(hca, pci, i, loud);
                }
                diag::speed_control(self.ptys_offered, self.an_rep, true);
                return;
            }
            boot::stall(ms(100));
            waited += 100;
        }
    }

    /// Receive frames sent to the multicast MACs `want` on port `i`, and no
    /// longer to the ones attached before (5.4.1, `mcg::sync`): the SNP's
    /// multicast list, applied as one step. In A0 mode multicast already
    /// reaches the RX QP (5.4.2), so there is nothing to do; the SNP filters
    /// in software either way. Broadcast stays attached from bring-up.
    pub fn set_multicast(&mut self, hca: &mut Hca, pci: &mut PciIo, i: usize, want: &[[u8; 6]]) -> Result<(), Fail> {
        if !self.sh.b0 {
            return Ok(());
        }
        let want: Vec<[u8; 6]> = want.iter().copied().filter(|m| *m != [0xff; 6]).collect();
        let p = &mut self.ports[i];
        let mut joined = core::mem::take(&mut p.joined);
        let t = McgTable { hca, pci, mc_steer: self.sh.mc_steer };
        let mut m = Steer { t, amgm: &mut self.sh.amgm, port: p.num, qpn: p.rx_qpn };
        let r = mcg::sync(&mut m, &mut joined, &want);
        let in_use = self.sh.amgm.in_use();
        let p = &mut self.ports[i];
        p.joined = joined;
        match r {
            Ok(0) => {}
            Ok(n) => say!("  port {}: steering: {n} multicast detach(es) failed, kept for the next filter change", p.num),
            Err(e) => return Err(e),
        }
        trace!("  port {}: steering: {} multicast MAC(s) attached, {in_use} overflow MCG entries in use", p.num, p.joined.len());
        Ok(())
    }
}

/// The 5.4.1 commands as an `mcg::Table`.
struct McgTable<'a> {
    hca: &'a mut Hca,
    pci: &'a mut PciIo,
    /// MGID_HASH op_mod (capability bit 42).
    mc_steer: bool,
}

const MGM_ENTRY: usize = 1 << fw::LOG_MGM_ENTRY;

impl mcg::Table for McgTable<'_> {
    type Error = Fail;
    fn hash(&mut self, gid: &[u8; 16]) -> Result<u32, Fail> {
        self.hca.hcr.inbox.zero();
        self.hca.hcr.inbox.write(0, gid);
        let r = self.hca.hcr.in_imm(self.pci, hcr::MGID_HASH, self.mc_steer as u8, 0);
        Ok((self.hca.cmd(r)? & 0xffff) as u32)
    }
    fn read(&mut self, at: u32) -> Result<Entry, Fail> {
        let r = self.hca.hcr.with_out(self.pci, hcr::READ_MCG, 0, at);
        self.hca.cmd(r)?;
        let mut b = [0u8; MGM_ENTRY];
        self.hca.hcr.outbox.read(0, &mut b);
        Ok(Entry::decode(&b))
    }
    fn write(&mut self, at: u32, e: &Entry) -> Result<(), Fail> {
        let mut b = [0u8; MGM_ENTRY];
        e.encode(&mut b);
        self.hca.hcr.inbox.write(0, &b);
        let r = self.hca.hcr.with_in(self.pci, hcr::WRITE_MCG, 0, at);
        self.hca.cmd(r)
    }
}

/// One port's RX QP joining and leaving multicast MACs, for `mcg::sync`.
struct Steer<'a, 'b> {
    t: McgTable<'b>,
    amgm: &'a mut Amgm,
    port: u8,
    qpn: u32,
}

impl mcg::Members for Steer<'_, '_> {
    type Error = Fail;
    fn join(&mut self, mac: [u8; 6]) -> Result<(), Fail> {
        attach(&mut self.t, self.amgm, self.port, mac, false, self.qpn)
    }
    fn leave(&mut self, mac: [u8; 6]) -> Result<(), Fail> {
        let gid = mcg::gid(self.port, mac, false);
        match mcg::detach(&mut self.t, self.amgm, &gid, self.qpn) {
            Ok(was) => {
                trace!("  steering: multicast {} -/-> QP {:#x}{}", Mac(mac), self.qpn, if was { "" } else { " (was not attached)" });
                Ok(())
            }
            Err(e) => Err(mcg_fail(e, "detach", mac)),
        }
    }
}

fn mcg_fail(e: mcg::Error<Fail>, what: &str, mac: [u8; 6]) -> Fail {
    match e {
        mcg::Error::Cmd(f) => f,
        mcg::Error::TableFull => fail!("steering {what} {}: MCG table full", Mac(mac)),
        mcg::Error::EntryFull(at) => fail!("steering {what} {}: MCG entry {at:#x} is full", Mac(mac)),
        mcg::Error::Loop(h) => fail!("steering {what} {}: MCG chain from {h:#x} loops", Mac(mac)),
    }
}

/// 5.1: Ethernet-only ports and VPI ports. A VPI port is always driven as
/// Ethernet (Linux's "unless configured otherwise", owner on #15): there is
/// no command that sets the type; the port is Ethernet when driven with the
/// Ethernet SET_PORT forms and Ethernet QPs. The firmware's suggestion and
/// SENSE_PORT's answer are only logged.
fn is_ethernet(hca: &mut Hca, pci: &mut PciIo, s: &Setup, p: &PortInfo) -> Result<bool, Fail> {
    let suggests = if p.b0 & 0x08 != 0 { "Ethernet" } else { "IB" };
    match p.b0 & 3 {
        2 => {
            trace!("  port {}: type Ethernet (Ethernet-only port; firmware suggestion {suggests} ignored)", p.num);
            Ok(true)
        }
        3 => {
            if s.cap.flag(12) && s.cap.flag(55) && p.b0 & 0x10 != 0 {
                match hca.hcr.imm(pci, hcr::SENSE_PORT, 0, u32::from(p.num), 0) {
                    Ok(v) => {
                        let what = match v {
                            1 => "IB",
                            2 => "Ethernet",
                            _ => "nothing",
                        };
                        trace!("  port {}: SENSE_PORT says {what}", p.num);
                    }
                    Err(e) if e.needs_reset() => return hca.cmd(Err(e)),
                    Err(_) => trace!("  port {}: SENSE_PORT failed", p.num),
                }
            }
            trace!("  port {}: type Ethernet (VPI port, forced; firmware suggests {suggests})", p.num);
            Ok(true)
        }
        _ => {
            say!("  port {}: not Ethernet-capable (QUERY_PORT byte 0 {:#04x}); skipped", p.num, p.b0);
            Ok(false)
        }
    }
}

/// 6.1 steps 15–18: object numbers, EQ, special QPs, memory region.
fn shared(hca: &mut Hca, pci: &mut PciIo, s: &Setup) -> Result<Shared, Fail> {
    let (cap, t) = (&s.cap, &s.prof.t);
    if cap.entry[fw::MTT] != 8 {
        return Err(fail!("MTT entry size {} is not 8 bytes (4.2); stopping", cap.entry[fw::MTT]));
    }
    // 4.1
    let pd = cap.rsvd_pds as u32;
    let uar = cap.rsvd_uars.max(128) as u32;
    let eqn = cap.rsvd_eqs;
    if eqn >= t[fw::EQC].count || eqn >= 256 {
        return Err(fail!("EQ number {eqn} does not fit the {}-entry EQ table (4.1); stopping", t[fw::EQC].count));
    }
    let eqn = eqn as u32;
    if u64::from(uar) >= cap.uar_bytes / PAGE as u64 {
        return Err(fail!("UAR page {uar} is outside the UAR area; stopping"));
    }
    if u32::from(cap.log_max_cqes) < fw::log2(u64::from(RX_ENTRIES.max(TX_TXBBS))) + 1
        || u32::from(cap.log_max_qp_wqes) < fw::log2(u64::from(RX_ENTRIES.max(TX_TXBBS)))
    {
        return Err(fail!("queues of {RX_ENTRIES} entries exceed the device's limits; stopping"));
    }
    let mut mtt = Mtt { mem: hca.mtt_mem(), entry: 8, next: s.prof.first_free_mtt, end: t[fw::MTT].count };
    trace!("  objects: PD {pd:#x}, UAR page {uar:#x}, EQ {eqn:#x}, first MTT {}", mtt.next);

    // 4.6: the EQ, every entry hardware-owned for the first pass.
    let mem = hca.lend(pci, "EQ", EQ_ENTRIES as usize * EQE)?;
    for i in 0..EQ_ENTRIES as usize {
        mem.set_u8(i * EQE + 0x1f, 0x80);
    }
    let off = mtt.map(mem)?;
    let m = &hca.hcr.inbox;
    m.zero();
    m.set_be32(0x00, 0x0000_0900);
    m.set_u8(0x0c, fw::log2(u64::from(EQ_ENTRIES)) as u8);
    set_mtt(m, 0x1b, 0x1c, off);
    let r = hca.hcr.with_in(pci, hcr::SW2HW_EQ, 0, eqn);
    hca.cmd(r)?;
    hca.push_undo(hcr::HW2SW_EQ, 1, eqn, 0);
    let mapped = match hca.hcr.imm(pci, hcr::MAP_EQ, 0, eqn, EVENT_MASK) {
        Ok(_) => {
            hca.push_undo(hcr::MAP_EQ, 0, eqn | 1 << 31, EVENT_MASK);
            true
        }
        Err(e) if e.needs_reset() => return hca.cmd(Err(e)),
        // Linux only warns (4.6); link state is then polled with QUERY_PORT.
        Err(_) => {
            say!("  MAP_EQ failed: no port events, polling QUERY_PORT only");
            false
        }
    };
    let eq = Eq { eqn, bar: hca.uar_bar, mem, ci: 0, mapped };

    // 3.12
    let sqpn = s.prof.base_sqpn as u32;
    let r = hca.hcr.imm(pci, hcr::CONF_SPECIAL_QP, 0, sqpn, 0);
    hca.cmd(r)?;
    hca.push_undo(hcr::CONF_SPECIAL_QP, 0, 0, 0);

    // 4.3: one physical region over all memory. MPT index 1 would give the
    // invalid L_Key 0x100, so skip it.
    let mut mpt = cap.rsvd_mpts as u32;
    if mpt == 1 {
        mpt = 2;
    }
    let mpt = mpt & (t[fw::DMPT].count as u32 - 1);
    let m = &hca.hcr.inbox;
    m.zero();
    m.set_be32(0x00, 0xf002_0f00);
    m.set_be32(0x08, mpt);
    m.set_be32(0x0c, pd | 0x0300_0000);
    m.set_be64(0x18, u64::MAX);
    m.set_be32(0x38, 12);
    let r = hca.hcr.with_in(pci, hcr::SW2HW_MPT, 0, mpt);
    hca.cmd(r)?;
    hca.push_undo(hcr::HW2SW_MPT, 1, mpt, 0);
    let lkey = mpt.rotate_left(8);
    trace!("  memory region: MPT {mpt:#x}, L_Key {lkey:#010x}");

    let counters = cap.flag(48);
    Ok(Shared {
        pd,
        uar,
        lkey,
        eq,
        mtt,
        amgm: Amgm::new(t[fw::MCG].count as u32),
        b0: cap.b0(),
        mc_steer: cap.flag(42),
        counters,
        lb_src_chk: counters && cap.flags2 & (1 << 19) != 0,
        port_remap: cap.bmme & (1 << 24) != 0,
        sw_cq_init: cap.flags2 & (1 << 23) != 0,
    })
}

/// 6.1 steps 19–27 for one port.
fn port_up(hca: &mut Hca, pci: &mut PciIo, sh: &mut Shared, s: &Setup, p: &PortInfo) -> Result<Port, Fail> {
    let (t, idx) = (&s.prof.t, u32::from(p.num - 1));
    // 4.1: the RX QP 128-aligned with the 127 numbers above it unused (A0),
    // TX QPs after port 1's RX block; two CQs per port.
    let sqpn = s.prof.base_sqpn as u32;
    let rx_qpn = sqpn + 128 * (2 * idx + 1);
    let tx_qpn = sqpn + 256 + idx;
    let rx_cqn = s.cap.rsvd_cqs as u32 + 2 * idx;
    let tx_cqn = rx_cqn + 1;
    if u64::from(rx_qpn.max(tx_qpn)) >= t[fw::QPC].count || u64::from(tx_cqn) >= t[fw::CQC].count {
        return Err(fail!("port {}: QP/CQ numbers outside the profile; stopping", p.num));
    }
    trace!(
        "  port {}: MAC {}, MTU cap {}, RX QP {rx_qpn:#x} CQ {rx_cqn:#x}, TX QP {tx_qpn:#x} CQ {tx_cqn:#x}, {} steering",
        p.num,
        Mac(p.mac),
        p.mtu_cap,
        if sh.b0 { "B0" } else { "A0" }
    );

    // 4.4: the doorbell records: RX CQ, TX CQ, RX QP, TX QP (unused).
    let db = hca.lend(pci, "doorbell records", PAGE)?;
    let rx_cq = cq(hca, pci, sh, rx_cqn, RX_ENTRIES, db.slice(0, 8))?;
    let tx_cq = cq(hca, pci, sh, tx_cqn, TX_TXBBS, db.slice(8, 8))?;

    // 4.8.2: RX QP buffer: one dummy TXBB, owner bit set, then the RQ.
    let rxq = hca.lend(pci, "RX QP", TXBB + RX_ENTRIES as usize * RX_STRIDE)?;
    rxq.set_be32(0, 0x8000_0000);
    let rq = rxq.slice(TXBB, RX_ENTRIES as usize * RX_STRIDE);
    let rx_bufs = hca.lend(pci, "receive buffers", RX_ENTRIES as usize * BUF)?;
    // 5.6: one 2 KiB buffer per descriptor, the whole ring filled.
    for i in 0..RX_ENTRIES as usize {
        let d = i * RX_STRIDE;
        rq.set_be32(d, BUF as u32);
        rq.set_be32(d + 4, sh.lkey);
        rq.set_be64(d + 8, rx_bufs.dev + (i * BUF) as u64);
    }
    let rx_mtt = sh.mtt.map(rxq)?;
    let rx = Qp {
        qpn: rx_qpn,
        rq_size_stride: (fw::log2(u64::from(RX_ENTRIES)) << 3) as u8,
        sq_size_stride: 0x02,
        cqn_send: rx_cqn,
        cqn_recv: rx_cqn,
        db: db.dev + 16,
        mtt: rx_mtt,
        rx: true,
    };
    qp(hca, pci, sh, p.num, &rx)?;
    let rx_db = db.slice(16, 4);
    wmb();
    rx_db.set_be32(0, RX_ENTRIES & 0xffff);

    // 4.8.2, 5.7: TX QP: the SQ, every TXBB stamped not-yet-valid, plus a
    // spare page for the phantom RQ ([HW-CHECK] 10 says it may not be needed).
    let sq_bytes = TX_TXBBS as usize * TXBB;
    let txq = hca.lend(pci, "TX QP", sq_bytes + PAGE)?;
    for i in 0..TX_TXBBS as usize {
        txq.set_be32(i * TXBB, 0xffff_ffff);
    }
    let tx_bufs = hca.lend(pci, "send buffers", TX_TXBBS as usize * BUF)?;
    let tx_mtt = sh.mtt.map(txq)?;
    let tx = Qp {
        qpn: tx_qpn,
        rq_size_stride: 0,
        sq_size_stride: ((fw::log2(u64::from(TX_TXBBS)) << 3) | 2) as u8,
        cqn_send: tx_cqn,
        cqn_recv: tx_cqn,
        db: db.dev + 24,
        mtt: tx_mtt,
        rx: false,
    };
    qp(hca, pci, sh, p.num, &tx)?;

    set_port(hca, pci, sh, p, rx_qpn)?;

    // 5.5
    let r = hca.hcr.imm(pci, hcr::INIT_PORT, 0, u32::from(p.num), 0);
    hca.cmd(r)?;
    hca.push_undo(hcr::CLOSE_PORT, 0, u32::from(p.num), 0);

    // 5.4.1: B0 steering entries after INIT_PORT: our MAC and broadcast.
    if sh.b0 {
        let mut t = McgTable { hca: &mut *hca, pci: &mut *pci, mc_steer: sh.mc_steer };
        attach(&mut t, &mut sh.amgm, p.num, p.mac, true, rx_qpn)?;
        attach(&mut t, &mut sh.amgm, p.num, [0xff; 6], false, rx_qpn)?;
    }
    // 5.4.3: no port multicast filter; steering decides.
    let r = hca.hcr.imm(pci, hcr::SET_MCAST_FLTR, 1, u32::from(p.num), 0);
    hca.cmd(r)?;

    Ok(Port {
        num: p.num,
        mac: p.mac,
        link_up: p.link_up,
        speed: "",
        media: String::from("module not read"),
        uar: sh.uar,
        uar_bar: hca.uar_bar,
        lkey: sh.lkey,
        rx_qpn,
        tx_qpn,
        rx_cq,
        tx_cq,
        rx_db,
        rx_bufs,
        rx_prod: RX_ENTRIES,
        sq: txq.slice(0, sq_bytes),
        tx_bufs,
        tx_prod: 0,
        tx_done: 0,
        tokens: vec![0; TX_TXBBS as usize],
        done: Sent::new(DONE_MAX),
        tx_broken: false,
        joined: Vec::new(),
        news: false,
        settled: false,
    })
}

/// 4.7: a CQ of `n` entries, filled with 0xCC and handed to the device.
fn cq(hca: &mut Hca, pci: &mut PciIo, sh: &mut Shared, cqn: u32, n: u32, db: Mem) -> Result<Cq, Fail> {
    let mem = hca.lend(pci, "CQ", n as usize * CQE)?;
    mem.fill(0xcc);
    let off = sh.mtt.map(mem)?;
    let m = &hca.hcr.inbox;
    m.zero();
    m.set_be32(0x0c, (fw::log2(u64::from(n)) << 24) | sh.uar);
    m.set_u8(0x17, sh.eq.eqn as u8);
    set_mtt(m, 0x1b, 0x1c, off);
    m.set_be64(0x38, db.dev);
    let r = hca.hcr.with_in(pci, hcr::SW2HW_CQ, sh.sw_cq_init as u8, cqn);
    hca.cmd(r)?;
    hca.push_undo(hcr::HW2SW_CQ, 1, cqn, 0);
    // Without SW_CQ_INIT the firmware initialises the entries; stamp them
    // hardware-owned again, as mlx4_en does (4.7). No QP uses the CQ yet.
    for i in 0..n as usize {
        mem.set_u8(i * CQE + 0x1f, mem.u8(i * CQE + 0x1f) | 0x80);
    }
    Ok(Cq { n, mem, db, ci: 0 })
}

/// The context values that differ between the two QPs (4.8.4).
struct Qp {
    qpn: u32,
    rq_size_stride: u8,
    sq_size_stride: u8,
    cqn_send: u32,
    cqn_recv: u32,
    db: u64,
    mtt: u64,
    rx: bool,
}

pub fn dump_qp_mailbox(m: &crate::dma::DmaBuf) {
    for line in (0..0x100).step_by(0x10) {
        let mut s = String::new();
        for i in line..line + 0x10 {
            let _ = write!(s, " {:02x}", m.u8(i));
        }
        trace!("  QP {line:02x}:{s}");
    }
}

/// RESET → INIT → RTR → RTS with the same context each time (4.8.3).
fn qp(hca: &mut Hca, pci: &mut PciIo, sh: &Shared, port: u8, q: &Qp) -> Result<(), Fail> {
    let steps = [(hcr::RST2INIT_QP, 1u32), (hcr::INIT2RTR_QP, 2), (hcr::RTR2RTS_QP, 3)];
    for (i, (op, state)) in steps.into_iter().enumerate() {
        let m = &hca.hcr.inbox;
        m.zero();
        let c = m.slice(0x08, 0xf8);
        c.set_be32(0x00, (state << 28) | (0x7 << 16));
        c.set_be32(0x04, sh.pd);
        c.set_u8(0x08, 0xff);
        c.set_u8(0x09, q.rq_size_stride);
        c.set_u8(0x0a, q.sq_size_stride);
        c.set_be32(0x0c, sh.uar);
        c.set_be32(0x10, q.qpn);
        // Primary path. 5.10: the RX QP drops its own multicast when the
        // loopback source check is available.
        if q.rx && sh.lb_src_chk {
            c.set_u8(0x18, 0x02);
            c.set_u8(0x19, 0x80);
        }
        c.set_u8(0x1c, if sh.counters { port - 1 } else { 0xff });
        c.set_u8(0x20, 0x01);
        c.set_u8(0x38, 0x83 | ((port - 1) << 6));
        c.set_be32(0x7c, q.cqn_send);
        if i == 1 && !q.rx && sh.port_remap {
            c.set_be32(0x90, 0x08);
        }
        c.set_be32(0x9c, q.cqn_recv);
        c.set_be64(0xa0, q.db);
        c.set_be32(0xc0, 0x4000_0000);
        set_mtt(&c, 0xcb, 0xcc, q.mtt);
        let mut r = hca.hcr.with_in(pci, op, 0, q.qpn);
        if i == 1 && !q.rx && sh.port_remap && matches!(r, Err(hcr::CmdError::Status(3))) {
            say!("  QP {:#x}: firmware refused FPP; retrying INIT2RTR without port remapping", q.qpn);
            c.set_be32(0x90, 0);
            r = hca.hcr.with_in(pci, op, 0, q.qpn);
        }
        if let Err(e) = hca.cmd(r) {
            say!("  QP {:#x}: {} failed; submitted context:", q.qpn, op.1);
            dump_qp_mailbox(&hca.hcr.inbox);
            match hca.hcr.with_out(pci, hcr::QUERY_QP, 0, q.qpn) {
                Ok(()) => {
                    say!("  QP {:#x}: firmware context:", q.qpn);
                    dump_qp_mailbox(&hca.hcr.outbox);
                }
                Err(e) => say!("  QP {:#x}: QUERY_QP: {e}", q.qpn),
            }
            return Err(e);
        }
        if i == 0 {
            hca.push_undo(hcr::TO_RST_QP, 2, q.qpn, 0);
        }
    }
    Ok(())
}

/// 5.3 (and 5.4.2 for A0): MAC table, general settings, RQP_CALC.
fn set_port(hca: &mut Hca, pci: &mut PciIo, sh: &Shared, p: &PortInfo, rx_qpn: u32) -> Result<(), Fail> {
    let port = u32::from(p.num);
    let mac = p.mac.iter().fold(0u64, |a, &b| a << 8 | u64::from(b));
    let m = &hca.hcr.inbox;
    m.zero();
    m.set_be64(0, 1 << 63 | mac);
    let r = hca.hcr.with_in(pci, hcr::SET_PORT, 1, 2 << 8 | port);
    hca.cmd(r)?;

    let m = &hca.hcr.inbox;
    m.zero();
    m.set_u8(0x03, 0x07);
    m.set_be16(0x06, PORT_MTU);
    m.set_u8(0x08, 0x80);
    m.set_u8(0x0c, 0x80);
    let r = hca.hcr.with_in(pci, hcr::SET_PORT, 1, port);
    hca.cmd(r)?;

    if !sh.b0 {
        let m = &hca.hcr.inbox;
        m.zero();
        m.set_be32(0x00, rx_qpn);
        m.set_u8(0x05, p.log_macs.min(7));
        m.set_u8(0x0f, 1);
        m.set_be32(0x14, rx_qpn);
        m.set_be32(0x18, (if sh.mc_steer { 1 } else { 2 }) << 30 | rx_qpn);
        let r = hca.hcr.with_in(pci, hcr::SET_PORT, 1, 1 << 8 | port);
        hca.cmd(r)?;
    }
    Ok(())
}

/// 5.4.1: add `qpn` to the MCG entry for this MAC, creating it if needed.
fn attach(t: &mut McgTable, amgm: &mut Amgm, port: u8, mac: [u8; 6], unicast: bool, qpn: u32) -> Result<(), Fail> {
    let gid = mcg::gid(port, mac, unicast);
    let a = mcg::attach(t, amgm, &gid, qpn, mcg::capacity(MGM_ENTRY)).map_err(|e| mcg_fail(e, "attach", mac))?;
    trace!(
        "  steering: {} {} -> QP {qpn:#x} (MCG entry {:#x}, hash {:#x}{})",
        if unicast { "unicast" } else { "multicast" },
        Mac(mac),
        a.at,
        a.hash,
        if a.added { "" } else { ", already a member" }
    );
    Ok(())
}

impl Port {
    pub fn num(&self) -> u8 {
        self.num
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn qpns(&self) -> (u32, u32) {
        (self.rx_qpn, self.tx_qpn)
    }

    pub fn link_up(&self) -> bool {
        self.link_up
    }

    /// The rate last seen with link, or "" (`report_link`).
    pub fn speed(&self) -> &'static str {
        self.speed
    }

    /// The module's media, as last read (`diagnose`).
    pub fn media(&self) -> &str {
        &self.media
    }

    fn set_link(&mut self, up: bool) {
        if up != self.link_up {
            if self.settled {
                say!("stormnic-mlx4: port {}: link {}", self.num, if up { "up" } else { "down" });
            } else {
                trace!("  port {}: link {}", self.num, if up { "up" } else { "down" });
            }
            self.link_up = up;
            self.news = true;
        }
    }

    /// The next good received frame, left in place: its buffer and length.
    /// Error and bad-FCS completions on the way are dropped (5.6).
    pub fn peek_rx(&mut self) -> Option<(Mem, usize)> {
        loop {
            let e = self.rx_cq.peek()?;
            if e.u8(0x1f) & 0x1f == CQE_ERROR {
                say!(
                    "  port {} rx: error completion, syndrome {:#04x} (vendor {:#04x})",
                    self.num,
                    e.u8(0x1b),
                    e.u8(0x1a)
                );
            } else if e.u8(0x13) & 0x10 == 0 {
                let slot = (u32::from(e.be16(0x18)) % RX_ENTRIES) as usize;
                let len = (e.be32(0x14) as usize).min(BUF);
                return Some((self.rx_bufs.slice(slot * BUF, BUF), len));
            }
            self.pop_rx();
        }
    }

    /// Is a completion waiting? Only looks, for `WaitForPacket`.
    pub fn rx_ready(&self) -> bool {
        self.rx_cq.peek().is_some()
    }

    /// Done with the frame `peek_rx` returned: consume its completion and
    /// hand the buffer back to the device.
    pub fn pop_rx(&mut self) {
        self.rx_cq.pop();
        // The slot's descriptor still names its buffer: reposting it is
        // just advancing the producer counter.
        self.rx_prod = self.rx_prod.wrapping_add(1);
        wmb();
        self.rx_db.set_be32(0, self.rx_prod & 0xffff);
    }

    /// Reclaim completed sends and stamp their TXBBs (5.7). Every WQE is one
    /// TXBB and asks for a completion, so completions come in ring order.
    fn reap_tx(&mut self) {
        while let Some(e) = self.tx_cq.peek() {
            if e.u8(0x1f) & 0x1f == CQE_ERROR {
                self.tx_broken = true;
                say!(
                    "  port {} tx: error completion, syndrome {:#04x} (vendor {:#04x}); the TX QP is now in error",
                    self.num,
                    e.u8(0x1b),
                    e.u8(0x1a)
                );
            }
            let slot = (self.tx_done % TX_TXBBS) as usize;
            let owner = (self.tx_done / TX_TXBBS) & 1;
            self.sq.set_be32(slot * TXBB, 0x7fff_ffff | owner << 31);
            self.done.push(self.tokens[slot]);
            self.tx_done = self.tx_done.wrapping_add(1);
            self.tx_cq.pop();
        }
    }

    /// A buffer whose frame has gone out (6.2 step 4).
    pub fn take_sent(&mut self) -> Option<usize> {
        self.reap_tx();
        self.done.take()
    }

    pub fn sent_pending(&mut self) -> bool {
        self.reap_tx();
        !self.done.is_empty()
    }

    /// Queue one frame (5.7): copy it into the slot's buffer, build a
    /// control + data segment WQE, hand it over, ring the doorbell. `token`
    /// comes back from `take_sent` once the device has sent it.
    pub fn send(&mut self, pci: &mut PciIo, frame: &[u8], token: usize) -> Result<(), Send> {
        self.reap_tx();
        if self.tx_broken {
            return Err(Send::Broken);
        }
        if frame.len() > BUF {
            return Err(Send::TooLong);
        }
        let in_flight = self.tx_prod.wrapping_sub(self.tx_done);
        // Room on the ring, and room in the list for its token once it is
        // sent: a caller that does not call GetStatus gets NOT_READY rather
        // than losing a buffer (6.2 step 4, finding 6aeccbfbe450, #37).
        if in_flight >= TX_TXBBS - HEADROOM - 1 || !self.done.admits(in_flight as usize) {
            return Err(Send::Full);
        }
        let slot = (self.tx_prod % TX_TXBBS) as usize;
        let len = frame.len().max(MIN_FRAME);
        let b = self.tx_bufs.slice(slot * BUF, BUF);
        b.write(0, frame);
        for i in frame.len()..len {
            b.set_u8(i, 0);
        }
        self.tokens[slot] = token;
        let w = self.sq.slice(slot * TXBB, TXBB);
        // Data segment: address and key, then the byte count.
        w.set_be32(0x14, self.lkey);
        w.set_be64(0x18, b.dev);
        wmb();
        w.set_be32(0x10, len as u32);
        // Control segment: no VLAN, 2 × 16 bytes, CQ update + solicited.
        w.set_be16(0x04, 0);
        w.set_u8(0x06, 0);
        w.set_u8(0x07, 2);
        w.set_be32(0x08, 0x0000_000e);
        w.set_be32(0x0c, 0);
        wmb();
        let owner = if self.tx_prod & TX_TXBBS != 0 { 1 << 31 } else { 0 };
        w.set_be32(0x00, owner | OP_SEND);
        self.tx_prod = self.tx_prod.wrapping_add(1);
        wmb();
        // 4.8.6: QPN << 8, big-endian, at UAR + 0x14.
        let off = u64::from(self.uar) * PAGE as u64 + 0x14;
        pci.mem_write32(self.uar_bar, off, (self.tx_qpn << 8).to_be()).map_err(|e| {
            say!("  port {} tx: doorbell write failed ({:?})", self.num, e.status());
            Send::Broken
        })
    }
}

impl Port {
    pub fn into_halves(self, pci: PciIo) -> (RxPort, TxPort) {
        let rx = RxPort {
            num: self.num,
            rx_cq: self.rx_cq,
            rx_db: self.rx_db,
            rx_bufs: self.rx_bufs,
            rx_prod: self.rx_prod,
        };
        let tx = TxPort {
            num: self.num,
            uar: self.uar,
            uar_bar: self.uar_bar,
            lkey: self.lkey,
            tx_qpn: self.tx_qpn,
            tx_cq: self.tx_cq,
            sq: self.sq,
            tx_bufs: self.tx_bufs,
            tx_prod: self.tx_prod,
            tx_done: self.tx_done,
            tx_broken: self.tx_broken,
            gap_reported: false,
            pci,
        };
        (rx, tx)
    }
}

pub struct RxPort {
    num: u8,
    rx_cq: Cq,
    rx_db: Mem,
    rx_bufs: Mem,
    rx_prod: u32,
}

impl RxPort {
    pub fn peek_rx(&mut self) -> Option<(Mem, usize)> {
        loop {
            let e = self.rx_cq.peek()?;
            if e.u8(0x1f) & 0x1f == CQE_ERROR {
                say!(
                    "mlx4: port {} rx error completion, syndrome {:#04x} (vendor {:#04x})",
                    self.num,
                    e.u8(0x1b),
                    e.u8(0x1a)
                );
            } else if e.u8(0x13) & 0x10 == 0 {
                let slot = (u32::from(e.be16(0x18)) % RX_ENTRIES) as usize;
                let len = (e.be32(0x14) as usize).min(BUF);
                return Some((self.rx_bufs.slice(slot * BUF, BUF), len));
            }
            self.pop_rx();
        }
    }

    pub fn pop_rx(&mut self) {
        self.rx_cq.pop();
        self.rx_prod = self.rx_prod.wrapping_add(1);
        kcore::barrier::dma_wmb();
        self.rx_db.set_be32(0, self.rx_prod & 0xffff);
    }
}

pub struct TxPort {
    num: u8,
    uar: u32,
    uar_bar: u8,
    lkey: u32,
    tx_qpn: u32,
    tx_cq: Cq,
    sq: Mem,
    tx_bufs: Mem,
    tx_prod: u32,
    tx_done: u32,
    tx_broken: bool,
    gap_reported: bool,
    pci: PciIo,
}

impl TxPort {
    fn reap_tx(&mut self) {
        while let Some(e) = self.tx_cq.peek() {
            let wqe_index = e.be16(0x18);
            let steps = wqe_index.wrapping_sub(self.tx_done as u16);
            if steps != 0 && !self.gap_reported {
                say!(
                    "mlx4: port {} tx completion gap: wqe {} prod {} done {}, skipping {} WQEs",
                    self.num,
                    wqe_index,
                    self.tx_prod,
                    self.tx_done,
                    steps
                );
                self.gap_reported = true;
            }
            if e.u8(0x1f) & 0x1f == CQE_ERROR && !self.tx_broken {
                let error_slot = usize::from(wqe_index) % TX_TXBBS as usize;
                let w = self.sq.slice(error_slot * TXBB, TXBB);
                say!(
                    "mlx4: port {} tx error: qpn {:#x} wqe {} prod {} done {} cq_ci {}, syndrome {:#04x} (vendor {:#04x})",
                    self.num,
                    e.be32(0x00),
                    wqe_index,
                    self.tx_prod,
                    self.tx_done,
                    self.tx_cq.ci,
                    e.u8(0x1b),
                    e.u8(0x1a)
                );
                say!(
                    "mlx4: port {} failing WQE: ctrl {:#010x} len {} lkey {:#010x} addr {:#018x}",
                    self.num,
                    w.be32(0x00),
                    w.be32(0x10),
                    w.be32(0x14),
                    w.be64(0x18)
                );
                self.tx_broken = true;
            }
            if steps >= TX_TXBBS as u16 {
                say!(
                    "mlx4: port {} tx completion outside the ring: wqe {} done {}, {} steps",
                    self.num,
                    wqe_index,
                    self.tx_done,
                    steps
                );
                self.tx_broken = true;
                self.tx_cq.pop();
                continue;
            }
            for _ in 0..=steps {
                let slot = (self.tx_done % TX_TXBBS) as usize;
                let owner = (self.tx_done / TX_TXBBS) & 1;
                self.sq.set_be32(slot * TXBB, 0x7fff_ffff | owner << 31);
                self.tx_done = self.tx_done.wrapping_add(1);
            }
            self.tx_cq.pop();
        }
    }

    pub fn can_send(&mut self) -> bool {
        self.reap_tx();
        !self.tx_broken && self.tx_prod.wrapping_sub(self.tx_done) < TX_TXBBS - HEADROOM - 1
    }

    pub fn send(&mut self, frame: &[u8]) -> Result<(), Send> {
        self.reap_tx();
        if self.tx_broken {
            return Err(Send::Broken);
        }
        if frame.len() > BUF {
            return Err(Send::TooLong);
        }
        let in_flight = self.tx_prod.wrapping_sub(self.tx_done);
        if in_flight >= TX_TXBBS - HEADROOM - 1 {
            return Err(Send::Full);
        }
        let slot = (self.tx_prod % TX_TXBBS) as usize;
        let len = frame.len().max(MIN_FRAME);
        let b = self.tx_bufs.slice(slot * BUF, BUF);
        b.write(0, frame);
        for i in frame.len()..len {
            b.set_u8(i, 0);
        }
        let w = self.sq.slice(slot * TXBB, TXBB);
        w.set_be32(0x14, self.lkey);
        w.set_be64(0x18, b.dev);
        kcore::barrier::dma_wmb();
        w.set_be32(0x10, len as u32);
        w.set_be16(0x04, 0);
        w.set_u8(0x06, 0);
        w.set_u8(0x07, 2);
        w.set_be32(0x08, 0x0000_000e);
        w.set_be32(0x0c, 0);
        kcore::barrier::dma_wmb();
        let owner = if self.tx_prod & TX_TXBBS != 0 { 1 << 31 } else { 0 };
        w.set_be32(0x00, owner | OP_SEND);
        self.tx_prod = self.tx_prod.wrapping_add(1);
        Ok(())
    }

    pub fn doorbell(&mut self) -> Result<(), Send> {
        kcore::barrier::dma_wmb();
        let off = u64::from(self.uar) * PAGE as u64 + 0x14;
        self.pci.mem_write32(self.uar_bar, off, (self.tx_qpn << 8).to_be()).map_err(|e| {
            say!("mlx4: port {} tx doorbell failed ({:?})", self.num, e.status());
            Send::Broken
        })
    }
}

/// Why `send` did not queue a frame.
pub enum Send {
    /// No room in the ring until completions are reaped.
    Full,
    TooLong,
    /// The TX QP is in the error state, or the doorbell write failed.
    Broken,
}
