//! The firmware command interface: the Host Command Register in BAR 0, in
//! polling mode, one command at a time (spec section 2).

use core::time::Duration;

use crate::time as boot;

use crate::dma::DmaBuf;
use crate::pci::PciIo;
use crate::fw::Fail;

/// The device configuration space BAR (spec 1.2).
pub const BAR_DCS: u8 = 0;
/// HCR offset in BAR 0 (spec 1.3).
const HCR: u64 = 0x80680;
/// Status/go dword bits (spec 2.2).
const GO: u32 = 1 << 23;
const T_SHIFT: u32 = 21;
/// Polling token, bits 31:16 of dword 0x14 (spec 2.3).
const TOKEN: u32 = 0xffff_0000;
/// Command completion timeout, every class (spec 2.7).
const TIMEOUT_US: u64 = 60_000_000;

/// A command: opcode and a name for the log (spec 2.9).
#[derive(Clone, Copy)]
pub struct Op(pub u16, pub &'static str);

pub const QUERY_FW: Op = Op(0x004, "QUERY_FW");
pub const QUERY_DEV_CAP: Op = Op(0x003, "QUERY_DEV_CAP");
pub const QUERY_ADAPTER: Op = Op(0x006, "QUERY_ADAPTER");
pub const INIT_HCA: Op = Op(0x007, "INIT_HCA");
pub const CLOSE_HCA: Op = Op(0x008, "CLOSE_HCA");
pub const MOD_STAT_CFG: Op = Op(0x034, "MOD_STAT_CFG");
pub const QUERY_PORT: Op = Op(0x043, "QUERY_PORT");
pub const QUERY_FUNC: Op = Op(0x056, "QUERY_FUNC");
pub const RUN_FW: Op = Op(0xff6, "RUN_FW");
pub const UNMAP_ICM: Op = Op(0xff9, "UNMAP_ICM");
pub const MAP_ICM: Op = Op(0xffa, "MAP_ICM");
pub const UNMAP_ICM_AUX: Op = Op(0xffb, "UNMAP_ICM_AUX");
pub const MAP_ICM_AUX: Op = Op(0xffc, "MAP_ICM_AUX");
pub const SET_ICM_SIZE: Op = Op(0xffd, "SET_ICM_SIZE");
pub const UNMAP_FA: Op = Op(0xffe, "UNMAP_FA");
pub const MAP_FA: Op = Op(0xfff, "MAP_FA");
// The data path (spec 2.9, sections 4 and 5).
pub const SET_PORT: Op = Op(0x00c, "SET_PORT");
pub const INIT_PORT: Op = Op(0x009, "INIT_PORT");
pub const CLOSE_PORT: Op = Op(0x00a, "CLOSE_PORT");
pub const SW2HW_MPT: Op = Op(0x00d, "SW2HW_MPT");
pub const HW2SW_MPT: Op = Op(0x00f, "HW2SW_MPT");
pub const MAP_EQ: Op = Op(0x012, "MAP_EQ");
pub const SW2HW_EQ: Op = Op(0x013, "SW2HW_EQ");
pub const HW2SW_EQ: Op = Op(0x014, "HW2SW_EQ");
pub const SW2HW_CQ: Op = Op(0x016, "SW2HW_CQ");
pub const HW2SW_CQ: Op = Op(0x017, "HW2SW_CQ");
pub const RST2INIT_QP: Op = Op(0x019, "RST2INIT_QP");
pub const INIT2RTR_QP: Op = Op(0x01a, "INIT2RTR_QP");
pub const RTR2RTS_QP: Op = Op(0x01b, "RTR2RTS_QP");
pub const QUERY_QP: Op = Op(0x022, "QUERY_QP");
pub const TO_RST_QP: Op = Op(0x021, "2RST_QP");
pub const CONF_SPECIAL_QP: Op = Op(0x023, "CONF_SPECIAL_QP");
pub const READ_MCG: Op = Op(0x025, "READ_MCG");
pub const WRITE_MCG: Op = Op(0x026, "WRITE_MCG");
pub const MGID_HASH: Op = Op(0x027, "MGID_HASH");
pub const SET_MCAST_FLTR: Op = Op(0x048, "SET_MCAST_FLTR");
pub const SENSE_PORT: Op = Op(0x04d, "SENSE_PORT");
// Link diagnostics (#17; spec 5.11, 5.12).
pub const MAD_IFC: Op = Op(0x024, "MAD_IFC");
pub const ACCESS_REG: Op = Op(0x03b, "ACCESS_REG");

/// Command status byte (spec 2.6).
pub const STATUS_MULTI_FUNC: u8 = 0x50;

fn status_name(s: u8) -> &'static str {
    match s {
        0x01 => "internal error",
        0x02 => "bad opcode",
        0x03 => "bad parameter",
        0x04 => "bad system state",
        0x05 => "bad resource",
        0x06 => "resource busy",
        0x08 => "exceeds device limits",
        0x09 => "bad resource state",
        0x0a => "index out of range",
        0x0b => "firmware image corrupted",
        0x0c => "ICM error",
        0x10 => "bad QP state",
        0x20 => "bad segment parameters",
        0x21 => "memory region has windows bound",
        0x22 => "local memory not present",
        0x30 => "bad management packet",
        0x40 => "CQ resize: too many outstanding CQEs",
        0x50 => "multi-function support required",
        _ => "unknown status",
    }
}

/// Why a command did not complete with status 0.
#[derive(Clone, Copy)]
pub enum CmdError {
    /// Completed with this non-zero status.
    Status(u8),
    /// GO still set after the timeout: the device needs a reset (spec 2.7).
    Timeout,
    /// The HCR was still pending before posting.
    Busy,
    /// A BAR access through PCI I/O failed.
    Pci,
}

impl core::fmt::Display for CmdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            CmdError::Status(s) => write!(f, "status {s:#04x}, {}", status_name(s)),
            CmdError::Timeout => f.write_str("timed out after 60 s"),
            CmdError::Busy => f.write_str("HCR still pending, not posted"),
            CmdError::Pci => f.write_str("BAR 0 access failed"),
        }
    }
}

impl CmdError {
    /// True when the device can no longer be trusted to finish commands and
    /// must be reset rather than torn down with more commands.
    pub fn needs_reset(self) -> bool {
        !matches!(self, CmdError::Status(_))
    }
}

pub struct Hcr {
    /// The T value the next command is posted with (spec 2.4).
    toggle: u32,
    pub inbox: DmaBuf,
    pub outbox: DmaBuf,
    /// Catastrophic error buffer (BAR, offset), once QUERY_FW has said (spec 2.8).
    pub catas: Option<(u8, u64)>,
    /// Log only failures: for commands repeated in a loop (link polling).
    pub quiet: bool,
    /// Log nothing: the ExitBootServices teardown (6.4).
    pub silent: bool,
    /// A diagnostic whose failure is not an error (#17): a status is traced,
    /// not replayed as a failure; the caller reports it.
    pub optional: bool,
}

impl Hcr {
    /// Set up the interface after a reset: one input and one output mailbox
    /// (4 KiB, 4 KiB aligned; spec 2.5), and the toggle from the T bit
    /// (spec 2.4, [HW-CHECK] 1).
    pub fn new(pci: &mut PciIo) -> Result<Self, Fail> {
        let inbox = DmaBuf::new(pci, 4096).map_err(|e| Fail::log("mailbox", e.status()))?;
        let outbox = match DmaBuf::new(pci, 4096) {
            Ok(b) => b,
            Err(e) => {
                // SAFETY: no command has been posted, so the device never saw it.
                unsafe { inbox.free(pci) };
                return Err(Fail::log("mailbox", e.status()));
            }
        };
        let mut hcr = Hcr { toggle: 1, inbox, outbox, catas: None, quiet: false, silent: false, optional: false };
        match hcr.read(pci, 0x18) {
            Ok(s) => {
                let t = (s >> T_SHIFT) & 1;
                if s & GO == 0 {
                    hcr.toggle = t ^ 1;
                }
                trace!(
                    "  HCR: status dword {s:08x} after reset (GO {}, T {t}); first toggle {}",
                    (s & GO != 0) as u8,
                    hcr.toggle
                );
            }
            Err(_) => say!("  HCR: cannot read the status dword"),
        }
        Ok(hcr)
    }

    /// Free both mailboxes.
    ///
    /// # Safety
    /// No command may be in flight (see `DmaBuf::free`).
    pub unsafe fn free(self, pci: &mut PciIo) {
        unsafe {
            self.inbox.free(pci);
            self.outbox.free(pci);
        }
    }

    fn read(&self, pci: &mut PciIo, off: u64) -> Result<u32, CmdError> {
        pci.mem_read32(BAR_DCS, HCR + off).map(u32::from_be).map_err(|_| CmdError::Pci)
    }

    fn write(&self, pci: &mut PciIo, off: u64, v: u32) -> Result<(), CmdError> {
        pci.mem_write32(BAR_DCS, HCR + off, v.to_be()).map_err(|_| CmdError::Pci)
    }

    fn pending(&self, pci: &mut PciIo) -> Result<bool, CmdError> {
        let s = self.read(pci, 0x18)?;
        Ok(s & GO != 0 || (s >> T_SHIFT) & 1 == self.toggle)
    }

    /// Post one command and poll it to completion (spec 2.3). Returns the
    /// immediate output parameter.
    fn post(
        &mut self,
        pci: &mut PciIo,
        op: Op,
        op_mod: u8,
        in_mod: u32,
        in_param: u64,
        out_param: u64,
    ) -> Result<u64, CmdError> {
        if self.pending(pci)? {
            return Err(CmdError::Busy);
        }
        self.write(pci, 0x00, (in_param >> 32) as u32)?;
        self.write(pci, 0x04, in_param as u32)?;
        self.write(pci, 0x08, in_mod)?;
        self.write(pci, 0x0c, (out_param >> 32) as u32)?;
        self.write(pci, 0x10, out_param as u32)?;
        self.write(pci, 0x14, TOKEN)?;
        // The mailbox and the six dwords above reach the device before GO.
        kcore::barrier::dma_wmb();
        let go = GO | (self.toggle << T_SHIFT) | (u32::from(op_mod) << 12) | u32::from(op.0 & 0xfff);
        self.write(pci, 0x18, go)?;
        self.toggle ^= 1;

        let mut waited = 0u64;
        let mut spins = 0u32;
        while self.pending(pci)? {
            if waited >= TIMEOUT_US {
                return Err(CmdError::Timeout);
            }
            // Most commands finish in microseconds; INIT_HCA and the MAP_*
            // family can take much longer.
            let step = if spins < 1000 { 1 } else { 100 };
            boot::stall(Duration::from_micros(step));
            waited += step;
            spins += 1;
        }
        kcore::barrier::dma_rmb();
        let status = (self.read(pci, 0x18)? >> 24) as u8;
        if status != 0 {
            return Err(CmdError::Status(status));
        }
        Ok((u64::from(self.read(pci, 0x0c)?) << 32) | u64::from(self.read(pci, 0x10)?))
    }

    /// Post, and log the outcome: one console line per command, which is
    /// what the hardware checklist (spec 7) asks for.
    fn run(
        &mut self,
        pci: &mut PciIo,
        op: Op,
        op_mod: u8,
        in_mod: u32,
        in_param: u64,
        out_param: u64,
    ) -> Result<u64, CmdError> {
        let r = self.post(pci, op, op_mod, in_mod, in_param, out_param);
        if self.silent {
            return r;
        }
        match r {
            Ok(_) if self.quiet => {}
            Ok(_) => trace!("  {} ({in_mod:#x}): ok", op.1),
            // An optional command's caller reports its own failure (#16).
            Err(e) if self.optional && !e.needs_reset() => trace!("  {} ({in_mod:#x}): {e}", op.1),
            Err(e @ CmdError::Timeout) => {
                alarm!("  {} ({in_mod:#x}): {e}", op.1);
                self.log_catas(pci);
            }
            Err(e) => alarm!("  {} ({in_mod:#x}): {e}", op.1),
        }
        r
    }

    /// No mailbox; immediate in and out.
    pub fn imm(&mut self, pci: &mut PciIo, op: Op, op_mod: u8, in_mod: u32, in_param: u64) -> Result<u64, CmdError> {
        self.run(pci, op, op_mod, in_mod, in_param, 0)
    }

    /// Input mailbox (`self.inbox`, filled by the caller).
    pub fn with_in(&mut self, pci: &mut PciIo, op: Op, op_mod: u8, in_mod: u32) -> Result<(), CmdError> {
        let a = self.inbox.dev;
        self.run(pci, op, op_mod, in_mod, a, 0).map(|_| ())
    }

    /// Input mailbox and an immediate output (MGID_HASH).
    pub fn in_imm(&mut self, pci: &mut PciIo, op: Op, op_mod: u8, in_mod: u32) -> Result<u64, CmdError> {
        let a = self.inbox.dev;
        self.run(pci, op, op_mod, in_mod, a, 0)
    }

    /// Output mailbox (`self.outbox`, zeroed first; read it afterwards).
    pub fn with_out(&mut self, pci: &mut PciIo, op: Op, op_mod: u8, in_mod: u32) -> Result<(), CmdError> {
        self.outbox.zero();
        let a = self.outbox.dev;
        self.run(pci, op, op_mod, in_mod, 0, a).map(|_| ())
    }

    /// Input and output mailboxes (MAD_IFC, ACCESS_REG; spec 2.5): the
    /// caller fills `self.inbox`; `self.outbox` is zeroed first.
    pub fn in_out(&mut self, pci: &mut PciIo, op: Op, op_mod: u8, in_mod: u32) -> Result<(), CmdError> {
        self.outbox.zero();
        let (i, o) = (self.inbox.dev, self.outbox.dev);
        self.run(pci, op, op_mod, in_mod, i, o).map(|_| ())
    }

    /// The first dword of the catastrophic error buffer: non-zero means the
    /// firmware hit a fatal error (spec 2.8).
    pub fn log_catas(&self, pci: &mut PciIo) {
        if let Some((bar, off)) = self.catas {
            match pci.mem_read32(bar, off) {
                Ok(v) => say!("  catastrophic error buffer: {:08x}", u32::from_be(v)),
                Err(e) => say!("  catastrophic error buffer: unreadable ({:?})", e.status()),
            }
        }
    }
}
