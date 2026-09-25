//! The machine's virtualization extension, and which CPUs it is on for.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hvarch::{Caps, CpuPage, Error, Ext, Result};
use kcore::consts::MAX_CPUS;
use kcore::cpu;
use kcore::sync::Mutex;

/// What one CPU is told to do, and what it says back.
///
/// It crosses to another CPU's interrupt handler, so it is values and an
/// atomic and nothing else: no allocation over there, nothing to free, and
/// the handler cannot reach anything that might go away underneath it. The
/// structure itself is the caller's stack -- sound because
/// [`cpu::run_on_with`] does not return until the handler has run.
struct Request {
    ext: Ext,
    /// The page the CPU is to use.
    page_phys: u64,
    /// [`NOT_RUN`] until the far CPU has answered, then [`DONE`] or an
    /// [`Error::code`].
    outcome: AtomicU32,
}

/* Neither is any error's code, so an answer is never mistaken for one:
 * the codes are small and start at 1. */
const NOT_RUN: u32 = 0;
const DONE: u32 = u32::MAX;

impl Request {
    fn new(ext: Ext, page_phys: u64) -> Self {
        Self { ext, page_phys, outcome: AtomicU32::new(NOT_RUN) }
    }

    /// What the far CPU said. A request whose handler never ran -- the CPU
    /// stopped between the check and the IPI -- reads as `NoSuchCpu` rather
    /// than as success.
    fn outcome(&self) -> Result<()> {
        match self.outcome.load(Ordering::Acquire) {
            DONE => Ok(()),
            NOT_RUN => Err(Error::NoSuchCpu),
            code => Err(Error::from_code(code)),
        }
    }

    fn finish(&self, r: Result<()>) {
        let code = match r {
            Ok(()) => DONE,
            Err(e) => e.code(),
        };
        self.outcome.store(code, Ordering::Release);
    }
}

/// Turn the extension on for the CPU this runs on. An IPI handler: interrupt
/// context, no sleeping, no allocating -- which is why the page it is given
/// was allocated by the caller, before the IPI.
fn enable_here(req: &Request) {
    /* On the CPU it is for -- an IPI handler runs nowhere else -- and the
     * page outlives the extension being on: the table below keeps it until
     * a `disable_here` has returned. */
    req.finish(unsafe { req.ext.enable(req.page_phys) });
}

/// Turn it off again, on the CPU this runs on, and say whether there was
/// anything to turn off -- or that there still is: a guest's VMCS current
/// on this CPU, which VT-x will not go off under ([`Ext::disable`]), and the
/// guest that owns it is then left to run. The same context as
/// [`enable_here`], and the moment the CPU lets go of its page: its slot in
/// the table is cleared here, on the CPU, with interrupts off, so that no
/// entry -- which reads the slot with interrupts off too -- can find the
/// page of a CPU the extension has gone off for, and none is turned away
/// from a CPU it stays on for.
///
/// The check belongs here rather than in the caller -- one IPI instead of
/// two, and no window in between for the answer to go stale. It has to
/// happen somewhere: `vmxoff` on a CPU that is not in root operation is an
/// undefined-opcode fault, and this kernel's handler panics.
fn disable_here(d: &Disable) {
    let answer = if !d.ext.enabled() {
        OFF
    } else if unsafe { d.ext.disable() } {
        ON
    } else {
        BUSY
    };
    if answer != BUSY {
        d.host_area.store(0, Ordering::Release);
    }
    d.answer.store(answer, Ordering::Release);
}

/// Asking a CPU whether the extension is on for it. Values and an atomic,
/// for the same reason [`Request`] is.
struct Query {
    ext: Ext,
    /// [`NOT_RUN`], or [`OFF`] or [`ON`].
    answer: AtomicU32,
}

/// Telling a CPU to turn it off: the same, and the CPU's slot in the host
/// area table for it to clear as it does.
struct Disable<'a> {
    ext: Ext,
    host_area: &'a AtomicU64,
    /// [`NOT_RUN`], or [`OFF`], [`ON`] or [`BUSY`].
    answer: AtomicU32,
}

const OFF: u32 = 1;
const ON: u32 = 2;
/// Still on: a guest's VMCS is current on the CPU.
const BUSY: u32 = 3;

impl Query {
    fn new(ext: Ext) -> Self {
        Self { ext, answer: AtomicU32::new(NOT_RUN) }
    }

    /// A CPU that never answered counts as off: it is not running, so
    /// nothing is running a guest there either.
    fn on(&self) -> bool {
        self.answer.load(Ordering::Acquire) == ON
    }
}

impl<'a> Disable<'a> {
    fn new(ext: Ext, host_area: &'a AtomicU64) -> Self {
        Self { ext, host_area, answer: AtomicU32::new(NOT_RUN) }
    }

    fn answer(&self) -> u32 {
        self.answer.load(Ordering::Acquire)
    }
}

/// What [`Machine::disable`] did: the CPUs it turned the extension off for,
/// and the ones it could not, a guest's VMCS being current there.
#[derive(Clone, Copy, Default)]
pub struct Disabled {
    pub off: u64,
    pub busy: u64,
}

/// Read the CPU's own state, on the CPU it is about.
fn ask_here(q: &Query) {
    let on = q.ext.enabled();
    q.answer.store(if on { ON } else { OFF }, Ordering::Release);
}

/// A CPU that would not take the extension, and why -- or, with no CPU,
/// why the machine has no extension to give. Which CPU matters: the host's
/// control registers are per CPU, and a machine whose BSP takes VMX and
/// whose APs do not (they come out of INIT with CR0.NE clear) is a machine
/// where "it would not turn on" is not enough to go on.
#[derive(Clone, Copy, Debug)]
pub struct Refused {
    pub cpu: Option<u32>,
    pub error: Error,
}

/// The pages handed out, indexed by CPU. A `Some` is a CPU the extension is
/// on for: the two are set and cleared together, under the lock, so there is
/// no state in which a CPU is running on a page that has been freed.
struct Cpus {
    page: Vec<Option<CpuPage>>,
}

/// The machine's virtualization extension.
///
/// One of these exists while the hypervisor module is loaded. It answers
/// what the CPU can do, turns the extension on for the CPUs that are to run
/// guests, and -- when it is dropped -- turns it off again everywhere and
/// only then gives the pages back.
pub struct Machine {
    caps: Caps,
    /// The extension a guest would run under, or why there is none. Decided
    /// once: the CPUs of a machine are alike, and a hypervisor whose answer
    /// depended on which CPU asked would have to say so to every caller.
    ext: Result<Ext>,
    cpus: Mutex<Cpus>,
    /// The physical address of each CPU's page, or 0: the table's pages as
    /// entering a guest has to check them -- with interrupts off, where the
    /// mutex cannot be taken. Set once the CPU has taken its page and before
    /// the table holds it; cleared by the CPU itself as it lets go of it
    /// (`disable_here`), and so before the page is freed. So a CPU's entry
    /// here is never the address of a page that has gone, nor of one for a
    /// CPU the extension is off for.
    host_areas: [AtomicU64; MAX_CPUS],
}

impl Machine {
    /// Ask the CPU what it has. Fails only for want of the memory the lock
    /// and the table take -- a machine with no virtualization at all is a
    /// `Machine` that answers questions and runs nothing, because
    /// `hv info` has to work there too.
    pub fn new() -> Result<Self> {
        let caps = Caps::probe();
        let ext = caps.ext();

        let mut page = Vec::new();
        page.try_reserve_exact(MAX_CPUS).map_err(|_| Error::NoMemory)?;
        for _ in 0..MAX_CPUS {
            page.push(None);
        }

        Ok(Self {
            caps,
            ext,
            cpus: Mutex::new(Cpus { page }).ok_or(Error::NoMemory)?,
            host_areas: [const { AtomicU64::new(0) }; MAX_CPUS],
        })
    }

    /// Each CPU's host save area, by CPU, 0 where there is none: what a
    /// guest's entry compares with what the CPU itself says before `vmrun`.
    /// Not for outside this crate: a cell anyone can read is a cell anyone
    /// can store to, and a stored address is what that check trusts.
    pub(crate) fn host_areas(&self) -> &[AtomicU64] {
        &self.host_areas
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// The extension a guest would run under, or why none can.
    pub fn ext(&self) -> Result<Ext> {
        self.ext
    }

    /// Which CPUs the extension is on for.
    pub fn enabled_mask(&self) -> u64 {
        let cpus = self.cpus.lock();
        let mut mask = 0u64;
        for (i, page) in cpus.page.iter().enumerate() {
            if page.is_some() {
                mask |= 1u64 << i;
            }
        }
        mask
    }

    /// Which CPUs have the extension on, asked of the CPUs themselves.
    ///
    /// [`Machine::enabled_mask`] says what this module turned on;  this says
    /// what the hardware has. They must agree, and the interesting case is
    /// when they do not: an extension left on by a module that has gone is
    /// precisely the failure a hypervisor that can be unloaded must not
    /// have, and it is invisible to anything that only reads its own
    /// bookkeeping. One IPI a CPU, so a diagnostic and not a datapath.
    pub fn hardware_mask(&self) -> u64 {
        let ext = match self.ext {
            Ok(ext) => ext,
            Err(_) => return 0,
        };
        let online = cpu::online_mask();
        let mut mask = 0u64;
        for i in 0..MAX_CPUS {
            let bit = 1u64 << i;
            if online & bit == 0 {
                continue;
            }
            let q = Query::new(ext);
            cpu::run_on_with(i as u32, &q, ask_here);
            if q.on() {
                mask |= bit;
            }
        }
        mask
    }

    /// Turn the extension on for every running CPU in `mask` it is not
    /// already on for, and say which ones that ended up being.
    ///
    /// Task context: it allocates a page per CPU -- here, where allocating
    /// is allowed -- and then sends each CPU an IPI that does nothing but
    /// write MSRs. The first CPU that refuses stops the run and is named in
    /// the answer, and what was turned on stays on: it is the caller's to
    /// decide whether a machine with half its CPUs ready is worth keeping,
    /// and `hv off` undoes it.
    pub fn enable(&self, mask: u64) -> core::result::Result<u64, Refused> {
        let ext = self.ext.map_err(|error| Refused { cpu: None, error })?;
        let online = cpu::online_mask();
        let mut cpus = self.cpus.lock();
        let mut done = 0u64;

        for i in 0..MAX_CPUS {
            let bit = 1u64 << i;
            if mask & bit == 0 || online & bit == 0 || cpus.page[i].is_some() {
                continue;
            }

            let refused = |error| Refused { cpu: Some(i as u32), error };

            /* Before the IPI, not inside it: a page allocation shoots down
             * every other CPU's TLB and waits for them to answer, and a CPU
             * in an interrupt handler cannot. */
            let page = self.caps.cpu_page().map_err(refused)?;
            let req = Request::new(ext, page.phys());
            cpu::run_on_with(i as u32, &req, enable_here);
            req.outcome().map_err(refused)?;

            /* Only now: the page is the CPU's from the moment it took it,
             * and the table is what keeps it alive. */
            self.host_areas[i].store(page.phys(), Ordering::Release);
            cpus.page[i] = Some(page);
            done |= bit;
        }
        Ok(done)
    }

    /// Turn it off for every CPU in `mask` it is on for, and say which ones
    /// those were -- and which it stays on for, because a guest's VMCS is
    /// current there ([`Disabled::busy`]); those keep their page, and the
    /// guest keeps running. Otherwise it does not fail: a CPU that does not
    /// answer is no longer running, and there is nothing left there to turn
    /// off.
    pub fn disable(&self, mask: u64) -> Disabled {
        let ext = match self.ext {
            Ok(ext) => ext,
            Err(_) => return Disabled::default(),
        };
        let online = cpu::online_mask();
        let mut cpus = self.cpus.lock();
        let mut done = Disabled::default();

        for i in 0..MAX_CPUS {
            let bit = 1u64 << i;
            if mask & bit == 0 {
                continue;
            }

            /* Every CPU asked for, not only the ones this module has a page
             * for: a load that found the extension already on has no page
             * for that CPU and turning it off is still the right thing --
             * the only thing, since the module that did turn it on is gone
             * and nothing else ever will. The CPU clears its own slot as it
             * goes off (`disable_here`): an entry that looked before has
             * interrupts off until its guest exits, and the IPI waits for
             * that; one that looks after finds nothing. */
            if online & bit != 0 {
                let d = Disable::new(ext, &self.host_areas[i]);
                cpu::run_on_with(i as u32, &d, disable_here);
                match d.answer() {
                    BUSY => {
                        done.busy |= bit;
                        continue;
                    }
                    ON => done.off |= bit,
                    _ => {}
                }
            }
            /* A CPU that is not running, or went between the mask and the
             * IPI: nothing there to look at the slot again. */
            self.host_areas[i].store(0, Ordering::Release);

            /* The page goes back only after the CPU that was using it has
             * said it is done -- or has stopped, in which case nothing will
             * touch it again either. Dropping it here frees it: off the
             * machine's own lock only in the sense that matters, from task
             * context, where the page allocator's TLB shootdown can wait. */
            cpus.page[i] = None;
        }
        done
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        /* The module is going: leave no CPU in a state the kernel did not
         * boot in. Whoever drops this has already stopped whatever was
         * running guests -- and dropped them, or a CPU stays on. */
        self.disable(u64::MAX);
    }
}
