//! A port's registers, and what is done with them as a whole: stopped,
//! started, reset, brought back from a failed command. Nothing here keeps
//! state: the disk's lock (`disk::Queue`) is what orders who touches them
//! when.

use kcore::io::MmioRegion;
use kcore::time::Deadline;

use crate::regs::*;

/// Between two polls: the CPU to whatever else can run here once the boot
/// has a scheduler to give it to.
pub(crate) fn relax() {
    if block::interrupts_started() {
        kcore::task::yield_to_runnable();
    } else {
        core::hint::spin_loop();
    }
}

pub(crate) fn wait(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let mut deadline = Deadline::after_ms(ms);
    loop {
        if done() {
            return true;
        }
        if deadline.passed() {
            return done();
        }
        relax();
    }
}

pub(crate) struct PortRegs {
    regs: MmioRegion,
    /// The HBA's CAP: 64-bit addressing, CLO.
    pub cap: u32,
}

impl PortRegs {
    pub fn new(regs: MmioRegion, cap: u32) -> PortRegs {
        PortRegs { regs, cap }
    }

    #[inline]
    pub fn read(&self, reg: usize) -> u32 {
        self.regs.read32(reg)
    }

    #[inline]
    pub fn write(&self, reg: usize, value: u32) {
        self.regs.write32(reg, value);
    }

    /// Commands and received FISes off: the HBA does nothing with this
    /// port's memory from here on.
    pub fn stop(&self) -> bool {
        let command = self.read(PX_CMD);
        if command & CMD_ST != 0 {
            self.write(PX_CMD, command & !CMD_ST);
        }
        if !wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0) {
            return false;
        }
        let command = self.read(PX_CMD);
        if command & CMD_FRE != 0 {
            self.write(PX_CMD, command & !CMD_FRE);
        }
        wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_FR == 0)
    }

    pub fn clear_errors(&self) {
        self.write(PX_SERR, u32::MAX);
        self.write(PX_IS, u32::MAX);
    }

    pub fn linked(&self) -> bool {
        self.read(PX_SSTS) & SSTS_DET_MASK == SSTS_DET_PRESENT
    }

    pub fn comreset(&self) -> bool {
        let control = self.read(PX_SCTL);
        self.write(PX_SCTL, (control & !SCTL_DET_MASK) | SCTL_DET_COMRESET);
        kcore::time::delay_us(COMRESET_HOLD_US);
        self.write(PX_SCTL, control & !SCTL_DET_MASK);
        let linked = wait(LINK_MS, || self.linked());
        self.write(PX_SERR, u32::MAX);
        linked
    }

    pub fn ready(&self) -> bool {
        self.read(PX_TFD) & (TFD_BSY | TFD_DRQ) == 0
    }

    /// Commands on, once the device is not busy: FRE is on already.
    pub fn start(&self) -> bool {
        if !wait(DEVICE_READY_MS, || self.ready()) {
            return false;
        }
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command | CMD_FRE | CMD_ST);
        true
    }

    /// Back from a failed command (AHCI 1.3.1, 6.2.2 and 10.4.2): stopped --
    /// which takes PxCI and PxSACT back to nothing -- errors cleared, and a
    /// COMRESET, with ST off, where the command engine would not stop, the
    /// link is gone, the device stays busy and CLO cannot clear that, or
    /// `reset` asks for one; started again. False where none of it brings
    /// the port back.
    pub fn recover(&self, reset: bool) -> bool {
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command & !CMD_ST);
        let stopped = wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0);
        self.clear_errors();
        let reset = reset || !stopped || !self.linked() || !(self.ready() || self.override_busy());
        if reset && (!self.comreset() || !wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0)) {
            return false;
        }
        self.clear_errors();
        self.start()
    }

    /// CLO, where the HBA has it: the device's busy and DRQ taken out of
    /// PxTFD, so that a command can be issued to it again. Whether the
    /// device then reads as ready.
    fn override_busy(&self) -> bool {
        if self.cap & CAP_SCLO == 0 {
            return false;
        }
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command | CMD_CLO);
        wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CLO == 0) && self.ready()
    }
}
