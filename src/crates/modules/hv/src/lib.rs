#![no_std]

//! The hypervisor, as a module that can be put in and taken out of a running
//! kernel.
//!
//! Why a module rather than part of the image: a machine that runs no guests
//! should carry none of this -- not the code, not the per-CPU pages, and
//! above all not the CPU left in a state it did not boot in. And a
//! hypervisor is the subsystem it is least pleasant to debug by rebooting:
//! `insmod`, try it, `rmmod`, change one field, `insmod` again is a loop of
//! seconds where a rebuild and a boot is a loop of minutes.
//!
//! What that costs is stated plainly in [`docs/hypervisor.md`]: a module
//! cannot register a block or a net driver, so a guest's virtual disks and
//! NICs will be served by this module over the kernel's own, never by
//! registering new ones.
//!
//!     insmod /hv.ko    -- vms=<folder>: where the VMs' folders are, /vms if not;
//!                         stop_secs=N: what rmmod gives the guests (120 s)
//!     hv info          -- what this machine has
//!     hv on            -- turn the extension on
//!     hv run all       -- run the built-in guests, each in a VM of its own
//!     hv boot /bzImage initrd=/initrd secs=60   -- a Linux guest, for a while
//!     hv start /bzImage initrd=/initrd          -- one that runs until stopped
//!     hv exec 0 uname -a                        -- a line typed at its shell
//!     hv stop 0        -- its power button pressed: it shuts down, or is stopped
//!     hv convert /noble.img /noble.raw          -- a cloud's qcow2 image made a raw disk
//!     hv off
//!     rmmod hv         -- every guest's power button pressed first, and the
//!                         ones still running after stop_secs stopped where
//!                         they are -- as nos's own poweroff and reboot do
//!
//! The built-in guests are a few bytes each (`hv::guests`); the Linux ones
//! are x86-64 only, so far (`plans/03-hypervisor.md`).

extern crate alloc;

#[cfg(target_arch = "x86_64")]
mod boot;
#[cfg(target_arch = "x86_64")]
mod clone;
#[cfg(target_arch = "x86_64")]
mod cloud;
#[cfg(target_arch = "x86_64")]
mod config;
#[cfg(target_arch = "x86_64")]
mod cores;
#[cfg(target_arch = "x86_64")]
mod dhcp;
#[cfg(target_arch = "x86_64")]
mod disk;
#[cfg(target_arch = "x86_64")]
mod guest;
#[cfg(target_arch = "x86_64")]
mod forward;
mod image;
#[cfg(target_arch = "x86_64")]
mod net;
#[cfg(target_arch = "x86_64")]
mod provider;
#[cfg(target_arch = "x86_64")]
mod relay;
#[cfg(target_arch = "x86_64")]
mod store;
#[cfg(target_arch = "x86_64")]
mod vmlog;
#[cfg(target_arch = "x86_64")]
mod vms;
mod work;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::{AtomicU32, Ordering};

use hv::Machine;
use kcore::cmd::{Command, Output};
use work::Shell;
use kcore::consts::MAX_CPUS;
use kcore::sync::Mutex;

const HELP: &str = "hv [info|on|off|run|bench|boot|start|create|clone|delete|vms|list|console|attach|send|exec|wait|restart|stop|forward|cores|convert|help] - the CPU's virtualization extension, and guests under it";

/// What `hv help` prints: every subcommand, a line each.
const USAGE: &str = "\
hv [info]                          what the CPU has, and which CPUs the extension is on for
hv on|off [cpu|all]                turn the extension on or off
hv run <guest|all> [cpu]           run the built-in guests
hv bench [cpu=N] [exits=N] [pages=N] [flush] [profile] [mmio]
                                   what a VM exit costs: CPUIDs, each after a read of
                                   that many pages, and with the whole TLB flushed on
                                   every entry, as before ASIDs, when told; profile
                                   splits an entry into its parts; mmio times MMIO
                                   loads instead, each decoded and performed
hv boot <kernel> [mem=MiB] [cpus=N] [secs=N] [cpu=N] [xapic] [ioapic] [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=path[:ro]]... [input=...] [cmdline=...]
                                   a Linux guest for secs, then its console and how it ended;
                                   cpus gives it N CPUs, each on a host CPU of its own, the
                                   first on cpu. The kernel is a bzImage, or a PVH ELF:
                                   /lib/hvboot boots the one its disk's grub.cfg names
hv start <kernel> [mem=MiB] [cpus=N] [cpu=N] [xapic] [ioapic] [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] [uuid=U] [net] [network=name] [port=N] [forward=nos:guest]... [cmdline=...]
                                   a Linux guest that runs until hv stop; restart boots it
                                   again when it resets itself or panics; uuid= what its
                                   SMBIOS says it is; network= the guests it reaches on the
                                   switch, none without; port= its own port of the switch,
                                   forward= nos's ports relayed to its own
hv start <name|folder>             the vm in the vms' folder of that name, or in that
                                   folder, as its vm.toml says: its console to console.log
                                   there. The vms' folder is /vms, or what insmod hv.ko
                                   was given as vms=<folder>; hv says which
hv create <name> <kernel> [the words hv start takes]
                                   <name>/vm.toml in the vms' folder, written from them --
                                   the paths the folder's unless they begin with /
hv create <name> <http(s)://image> [size=N[K|M|G|T]] [password=...] [hvboot=path] [words]
                                   a vm of a cloud's image, qcow2 or raw: downloaded into
                                   its folder and made its disk, grown to size=; cloud-init
                                   given the keys nos's sshd lets in, and the password; and
                                   a vm.toml booting it by hvboot, on the switch, 1 GiB
hv clone <from> <to> [port=N]      a vm's folder copied into a new one: its disks with their
                                   holes left out, a new UUID, a port of its own if it had
                                   one, no forwards, and cloud-init's seed made anew
hv delete <name> [keep-disks] [force]
                                   a stopped vm's folder taken away, all of it or all but its
                                   disks; force for one whose vm.toml says protected
hv vms                             the vms in the vms' folder, and which of them run
hv cores [own|shared]              whether a guest's cores are its own -- whole cores no
                                   other guest has a CPU on, its CPUs filling their
                                   threads, and refused where there are none -- or may be
                                   shared; and which guests hold which CPUs
hv list                            the started guests
hv console <vm> [bytes=N]          the end of one's console -- a vm is its number or name
hv attach <vm>                     its console, live, typed at -- ^] detaches (ssh -t)
hv send <vm> <text>                type at it (\\n for a newline)
hv exec <vm> [secs=N] <line>       type a line and print the answer, up to the next prompt
hv wait <vm> [secs=N] [boot=N] <text>
                                   until its console shows text in this boot -- or,
                                   with boot=N, once it has restarted N times --
                                   or it stops
hv restart <vm>                    boot it again from its files, running or stopped
hv stop <vm|all> [secs=N]          press its power button and give it secs (30) to turn
                                   itself off, then stop it; say how it ended, take it off
                                   the list -- secs=0 stops it without asking. rmmod hv,
                                   and nos's poweroff and reboot, stop all of them so,
                                   given what insmod hv.ko was as stop_secs=N (120)
hv forward [add <port> <vm> <guest-port> | del <port>]
                                   a port of nos's relayed to a guest's (net guests) -- and
                                   kept in, or taken out of, a vm's vm.toml when it has one
hv convert <image> <disk>          a qcow2 image -- a cloud's, as QEMU writes them -- made
                                   the raw disk a guest is given: a new file, its holes
                                   kept, and removed again if the conversion fails
";

/// What the command works on: the machine, and the guests started on it.
struct State {
    machine: Arc<Machine>,
    #[cfg(target_arch = "x86_64")]
    vms: vms::Vms,
    /// Which guests hold which host CPUs, and whether a guest's cores are
    /// its own: what every Linux guest is placed by.
    #[cfg(target_arch = "x86_64")]
    cores: cores::Cores,
    #[cfg(target_arch = "x86_64")]
    forwards: forward::Forwards,
    /// Where the VMs' folders are: `vms=` of `insmod`'s, or `/vms`.
    #[cfg(target_arch = "x86_64")]
    vms_root: String,
    /// What the module's going gives the guests to turn themselves off once
    /// their power buttons are pressed: `stop_secs=` of `insmod`'s, or
    /// `vms::UNLOAD_STOP_DEFAULT_S`. 0 stops them where they are.
    #[cfg(target_arch = "x86_64")]
    stop_secs: u64,
    /// Set as the module starts to go: an `hv boot` running then stops at
    /// once instead of at the end of its time, and a download, a conversion
    /// or a clone where it is, taking back what it made.
    unloading: Arc<core::sync::atomic::AtomicBool>,
}

struct Hv {
    /// The `vm` provider on the management bus: unregistered first, which
    /// waits out its calls and jobs -- told to stop -- before anything they
    /// use goes.
    #[cfg(target_arch = "x86_64")]
    provider: Option<kcore::mgmt::Provider>,
    /// In an `Option` so that unregistering -- which waits out a call of the
    /// command still running on another CPU -- happens before the machine is
    /// let go of, and not in whatever order the fields happen to be in.
    cmd: Option<Command>,
    state: Arc<State>,
}

impl kmod::Module for Hv {}

impl Drop for Hv {
    fn drop(&mut self) {
        /* The guests first: an `hv boot` running, and every `hv start`ed one
         * -- so that an `hv wait` or `hv exec` waiting on one returns, and it
         * is those calls the unregister below waits for. A download, a
         * conversion or a clone stops where it is, as told by the flag. */
        self.state.unloading.store(true, core::sync::atomic::Ordering::Release);
        #[cfg(target_arch = "x86_64")]
        {
            /* The provider before them: its jobs -- a start, a clone, a
             * create -- stop where they are, and its unregister waits for
             * them, so none starts a guest or a forward behind the close. */
            drop(self.provider.take());
            self.state.forwards.close();
            self.state.vms.close(self.state.stop_secs);
        }

        /* Then the command: `kernel_cmd_unregister` returns only once any
         * call of it still running has, so from here nothing can ask the
         * machine anything -- nor start a guest on it, since `close` turned
         * every later `hv start` away. */
        drop(self.cmd.take());

        /* A forward added while the guests stopped, before the command went:
         * its tasks are this module's code, and go before it does. */
        #[cfg(target_arch = "x86_64")]
        self.state.forwards.close();

        /* Then the CPUs, here rather than in the machine's own drop, so that
         * what is left can be read back afterwards and said out loud. A
         * hypervisor module that goes and leaves the extension on is the one
         * failure this shape must not have: the code that would turn it off
         * has just been freed. */
        let machine = &self.state.machine;
        let was = machine.disable(u64::MAX);
        if was.busy != 0 {
            /* Cannot be: every guest was stopped and dropped above, and a
             * dropped guest's VMCS is current nowhere. Said anyway. */
            kcore::trace!(0, "hv: WARNING -- a guest's VMCS is still current on cpu mask 0x{:x}", was.busy);
        }
        let left = machine.hardware_mask();
        if left != 0 {
            kcore::trace!(0, "hv: WARNING -- unloading with the extension still on for cpu mask 0x{:x}", left);
        }
        kcore::trace!(0, "hv: unloaded, extension off for cpu mask 0x{:x}", was.off);
    }
}

/// What `insmod` was told.
#[cfg(target_arch = "x86_64")]
struct Params {
    vms_root: String,
    stop_secs: u64,
}

/// `insmod /hv.ko [vms=<folder>] [stop_secs=N]`: where the VMs' folders are
/// -- `/vms` unless given -- and what the module's going gives the guests to
/// turn themselves off. Anything else is refused, said on insmod's output.
#[cfg(target_arch = "x86_64")]
fn params(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Params> {
    let mut root = None;
    let mut stop_secs = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("vms", Some(path)) if root.is_none() => match store::root(path) {
                Ok(r) => root = Some(r),
                Err(e) => {
                    let _ = writeln!(out, "hv: vms={} {}", path, e);
                    return Err(kcore::error::Error::InvalidValue);
                }
            },
            ("vms", _) => {
                let _ = writeln!(out, "hv: vms= takes a folder, and is given once");
                return Err(kcore::error::Error::InvalidValue);
            }
            ("stop_secs", Some(v)) if stop_secs.is_none() => match v.parse::<u64>() {
                Ok(secs) if secs <= vms::WAIT_MAX_S => stop_secs = Some(secs),
                _ => {
                    let _ = writeln!(out, "hv: stop_secs={} is not a number of seconds 0..{}", v, vms::WAIT_MAX_S);
                    return Err(kcore::error::Error::InvalidValue);
                }
            },
            ("stop_secs", _) => {
                let _ = writeln!(out, "hv: stop_secs= takes a number of seconds, and is given once");
                return Err(kcore::error::Error::InvalidValue);
            }
            _ => {
                let _ = writeln!(out, "hv: \"{}\" is not a parameter of hv's -- vms=<folder> and stop_secs=N are",
                                 key);
                return Err(kcore::error::Error::InvalidValue);
            }
        }
    }
    Ok(Params {
        vms_root: root.unwrap_or_else(|| String::from(store::DEFAULT_ROOT)),
        stop_secs: stop_secs.unwrap_or(vms::UNLOAD_STOP_DEFAULT_S),
    })
}

/// No Linux guest runs here yet, so there is nothing for a parameter to say:
/// one given is refused rather than kept and never used.
#[cfg(not(target_arch = "x86_64"))]
fn params(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<()> {
    if let Some((key, _)) = params.words().next() {
        let _ = writeln!(out, "hv: \"{}\" is not a parameter of hv's on this architecture -- it runs no Linux guest \
                               yet, and takes none", key);
        return Err(kcore::error::Error::InvalidValue);
    }
    Ok(())
}

fn init(given: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    #[cfg(target_arch = "x86_64")]
    let Params { vms_root, stop_secs } = params(given, out)?;
    #[cfg(not(target_arch = "x86_64"))]
    params(given, out)?;

    /* A machine with no virtualization still gets a `Machine` and still
     * loads: `hv info` is a diagnostic, and the machine that most needs one
     * is the machine where a guest will not start. */
    let machine = Arc::new(Machine::new().map_err(kernel_error)?);
    let state = Arc::new(State {
        machine: machine.clone(),
        #[cfg(target_arch = "x86_64")]
        vms: vms::Vms::new().ok_or(kcore::error::Error::NoMemory)?,
        #[cfg(target_arch = "x86_64")]
        cores: cores::Cores::new().ok_or(kcore::error::Error::NoMemory)?,
        #[cfg(target_arch = "x86_64")]
        forwards: forward::Forwards::new().ok_or(kcore::error::Error::NoMemory)?,
        #[cfg(target_arch = "x86_64")]
        vms_root,
        #[cfg(target_arch = "x86_64")]
        stop_secs,
        unloading: Arc::new(core::sync::atomic::AtomicBool::new(false)),
    });

    let for_cmd = state.clone();
    let cmd = Command::register("hv", HELP, move |args, out| command(&for_cmd, args, out))?;
    /* The VMs on the management bus, where the guests are: refused only by
     * a bus that is full or stopping, which the module runs without. */
    #[cfg(target_arch = "x86_64")]
    let provider = match provider::register(state.clone()) {
        Ok(p) => Some(p),
        Err(status) => {
            kcore::trace!(0, "hv: the vm provider is not on the management bus -- {}", status.name());
            None
        }
    };

    match machine.ext() {
        Ok(ext) => {
            /* Nothing else in this kernel turns the extension on, so a CPU
             * that has it on at load was left that way by a module that did
             * not clean up -- worth a line, since the pages it was using are
             * gone and the next `hv on` would hand it different ones. */
            let left_on = machine.hardware_mask();
            if left_on != 0 {
                kcore::trace!(0, "hv: WARNING -- already on for cpu mask 0x{:x} before this load", left_on);
            }
            kcore::trace!(0, "hv: loaded -- {}, ready", ext.vendor().name());
        }
        Err(e) => kcore::trace!(0, "hv: loaded -- no guest can run here: {}", e),
    }

    Ok(Box::new(Hv {
        #[cfg(target_arch = "x86_64")]
        provider,
        cmd: Some(cmd),
        state,
    }))
}

/// An `hv::Error` as the kernel's, for the one place the two meet: what
/// `insmod` gets back when the module will not start. The hypervisor's own
/// reasons are richer than the kernel's eight codes and are what the report
/// prints; this is only the word `insmod` prints.
fn kernel_error(e: hv::Error) -> kcore::error::Error {
    match e {
        hv::Error::NoMemory => kcore::error::Error::NoMemory,
        hv::Error::NoSuchCpu => kcore::error::Error::InvalidValue,
        _ => kcore::error::Error::DeviceError,
    }
}

/// `hv cores [own|shared]`: set the rule new guests are placed by, and say
/// it and who holds what.
#[cfg(target_arch = "x86_64")]
fn cores_command(cores: &cores::Cores, word: Option<&str>, out: &mut Output) {
    match word {
        None => {}
        Some("own") => cores.set_own(true),
        Some("shared") => cores.set_own(false),
        Some(other) => {
            let _ = writeln!(out, "hv: cores own or cores shared, not \"{}\"", other);
            return;
        }
    }
    cores.report(out);
}

fn command(state: &State, args: &str, out: &mut Output) {
    let machine = &state.machine;
    /* The subcommand, and what follows it spaces and all: a command line, the
     * text to type at a guest. Split where `char` says, so that both halves
     * start on a character. */
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    let (first, rest) = (&args[..end], args[end..].trim_start());
    let mut words = rest.split_ascii_whitespace();
    match first {
        "" => {
            status(machine, out);
            #[cfg(target_arch = "x86_64")]
            vms_status(state, out);
        }
        "info" => {
            let _ = machine.caps().report(out);
            pool_report(out);
            status(machine, out);
            #[cfg(target_arch = "x86_64")]
            vms_status(state, out);
        }
        "help" => {
            let _ = write!(out, "{}", USAGE);
        }
        "on" => switch(state, words.next(), true, out),
        "off" => switch(state, words.next(), false, out),
        "run" => run(machine, words.next(), words.next(), out),
        "convert" => image::command(rest, &mut Shell { out, unloading: &state.unloading }),
        #[cfg(target_arch = "x86_64")]
        "bench" => bench(machine, words, out),
        #[cfg(target_arch = "x86_64")]
        "boot" => boot::boot(machine, rest, &state.cores, &state.unloading, state.vms.claims(), out),
        #[cfg(target_arch = "x86_64")]
        "start" => state.vms.start(machine, &state.cores, &state.forwards, &state.vms_root, rest, out),
        #[cfg(target_arch = "x86_64")]
        "create" => store::create(&state.vms_root, &state.vms, rest, &mut Shell { out, unloading: &state.unloading }),
        #[cfg(target_arch = "x86_64")]
        "clone" => clone::command(&state.vms_root, &state.vms, rest, &mut Shell { out, unloading: &state.unloading }),
        #[cfg(target_arch = "x86_64")]
        "delete" => store::delete_command(&state.vms_root, &state.vms, rest, out),
        #[cfg(target_arch = "x86_64")]
        "vms" => store::list(&state.vms_root, &state.vms, out),
        #[cfg(target_arch = "x86_64")]
        "cores" => cores_command(&state.cores, words.next(), out),
        #[cfg(target_arch = "x86_64")]
        "list" => state.vms.list(out),
        #[cfg(target_arch = "x86_64")]
        "console" => state.vms.console(rest, out),
        #[cfg(target_arch = "x86_64")]
        "send" => state.vms.send(rest, out),
        #[cfg(target_arch = "x86_64")]
        "exec" => state.vms.exec(rest, out),
        #[cfg(target_arch = "x86_64")]
        "wait" => state.vms.wait(rest, out),
        #[cfg(target_arch = "x86_64")]
        "attach" => state.vms.attach(rest, out),
        #[cfg(target_arch = "x86_64")]
        "restart" => state.vms.restart(rest, out),
        #[cfg(target_arch = "x86_64")]
        "stop" => state.vms.stop(rest, out, &state.forwards),
        #[cfg(target_arch = "x86_64")]
        "forward" => forward_command(state, rest, out),
        #[cfg(not(target_arch = "x86_64"))]
        "boot" | "start" | "create" | "clone" | "delete" | "vms" | "list" | "console" | "attach" | "send" | "exec"
        | "wait" | "restart" | "stop" | "forward" | "bench" | "cores" => {
            let _ = writeln!(out, "hv: no Linux guest on this architecture yet");
        }
        other => {
            let _ = writeln!(out, "hv: no such thing as \"{}\"", other);
            let _ = write!(out, "{}", USAGE);
        }
    }
}

/// Where the VMs' folders are, and what the module's going gives the guests,
/// under what `hv` says of the CPUs.
#[cfg(target_arch = "x86_64")]
fn vms_status(state: &State, out: &mut Output) {
    let _ = writeln!(out, "hv: vms kept in {}{}", state.vms_root,
                     if state.vms_root == store::DEFAULT_ROOT { "" } else { ", as insmod was told" });
    let told = if state.stop_secs == vms::UNLOAD_STOP_DEFAULT_S { "" } else { ", as insmod was told" };
    let _ = match state.stop_secs {
        0 => writeln!(out, "hv: an unload -- rmmod, nos's poweroff or reboot -- stops the guests where they are, \
                            without pressing their power buttons{}", told),
        secs => writeln!(out, "hv: an unload -- rmmod, nos's poweroff or reboot -- presses the guests' power buttons \
                               and gives them {} s to turn themselves off{}", secs, told),
    };
}

/// `hv forward [add <port> <vm> <guest-port> | del <port>]`.
#[cfg(target_arch = "x86_64")]
fn forward_command(state: &State, args: &str, out: &mut Output) {
    let mut w = args.split_ascii_whitespace();
    match (w.next(), w.next(), w.next(), w.next()) {
        (None, ..) => state.forwards.list(out),
        (Some("add"), Some(port), Some(vm), Some(guest_port)) => {
            let (Ok(port), Ok(guest_port)) = (port.parse::<u16>(), guest_port.parse::<u16>()) else {
                let _ = writeln!(out, "{}", forward::usage());
                return;
            };
            let vm = match state.vms.id_of(vm) {
                Ok(id) => id,
                Err(e) => {
                    let _ = writeln!(out, "hv: {}", e);
                    return;
                }
            };
            if port == 0 || guest_port == 0 {
                let _ = writeln!(out, "hv: ports are 1..65535");
                return;
            }
            let Some(ip) = state.vms.address_of(vm) else {
                let _ = writeln!(out, "hv: vm {} is not a running guest with a NIC (hv start ... net)", vm);
                return;
            };
            let Some(host) = state.vms.host_nic() else {
                let _ = writeln!(out, "hv: the guests' switch is not up");
                return;
            };
            match state.forwards.add(port, vm, guest_port, ip, host, out) {
                Ok(()) => keep_forward(state, vm, port, Some(guest_port), out),
                Err(e) => {
                    let _ = writeln!(out, "hv: {}", e);
                }
            }
        }
        (Some("del"), Some(port), None, None) => match port.parse::<u16>() {
            Ok(port) => match state.forwards.del(port) {
                Ok(vm) => {
                    let _ = writeln!(out, "hv: port {} no longer forwarded", port);
                    keep_forward(state, vm, port, None, out);
                }
                Err(e) => {
                    let _ = writeln!(out, "hv: {}", e);
                }
            },
            Err(_) => {
                let _ = writeln!(out, "{}", forward::usage());
            }
        },
        _ => {
            let _ = writeln!(out, "{}", forward::usage());
        }
    }
}

/// A forward of nos's `port` -- to the guest's `guest_port`, or taken away
/// -- kept in the vm.toml of VM `vm`, when it was started from its folder:
/// at its next start it has it, or not, as now.
#[cfg(target_arch = "x86_64")]
fn keep_forward(state: &State, vm: u32, port: u16, guest_port: Option<u16>, out: &mut Output) {
    let Some(name) = state.vms.name_of(vm) else { return };
    let kept = store::folder_of(&state.vms_root, &name).and_then(|folder| {
        store::update(&folder, |c| {
            c.options.forwards.retain(|&(host, _)| host != port);
            if let Some(guest) = guest_port {
                c.options.forwards.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                c.options.forwards.push((port, guest));
            }
            Ok(())
        })
        .map(|changed| (folder, changed))
    });
    match kept {
        Ok((_, false)) => {}
        Ok((folder, true)) => {
            let _ = writeln!(out, "hv: {} {}/{}", if guest_port.is_some() { "kept in" } else { "taken out of" },
                             folder.path, store::CONFIG);
        }
        Err(e) => {
            let _ = writeln!(out, "hv: not changed in {}'s {} -- {}", name, store::CONFIG, e);
        }
    }
}

/// What keeps one guest's speculation off the host and the next guest
/// (`hv::spec`), and how often it was needed -- for VT-x, as the CPU this
/// runs on has it; what of it a guest has of its own; and a warning for
/// every CPU of `hardware`, those the extension is on for, whose
/// IA32_SPEC_CTRL is not what the host runs with there -- what `hv on` set
/// undone, or a guest's left over it -- asked of each CPU's register.
#[cfg(target_arch = "x86_64")]
fn speculation(out: &mut Output, hardware: u64) {
    use hv::spec::Ibrs;
    let mut undone = 0u64;
    for i in 0..MAX_CPUS {
        if hardware & (1u64 << i) == 0 {
            continue;
        }
        let answer = AtomicU32::new(SPEC_UNASKED);
        kcore::cpu::run_on_with(i as u32, &answer, ask_spec_ctrl);
        if answer.load(Ordering::Acquire) == SPEC_UNDONE {
            undone |= 1u64 << i;
        }
    }
    if undone != 0 {
        let _ = write!(out, "hv: WARNING -- IA32_SPEC_CTRL on cpu ");
        write_mask(out, undone);
        let _ = writeln!(out, " is not what the host runs with there: what hv on set undone, or a guest's bits \
                               left over it");
    }
    let r = hv::spec::report();
    let a = r.amd;
    if a.ibpb_exit {
        let _ = writeln!(out, "hv: guests switched on a CPU {} times; an IBPB after every exit, {} issued, is between \
                               each two", r.switches, r.exit_barriers);
    } else if r.ibpb {
        let _ = writeln!(out, "hv: guests switched on a CPU {} times, an IBPB between each two ({} issued)",
                         r.switches, r.barriers);
    } else {
        let _ = writeln!(out, "hv: guests switched on a CPU {} times; the CPU has no IBPB to put between them",
                         r.switches);
    }
    if r.asked != 0 {
        if a.ibpb_exit {
            let _ = writeln!(out, "hv: guests ran on {} times after an IBPB they asked for, the one after every exit \
                                   between", r.asked);
        } else {
            let _ = writeln!(out, "hv: guests ran on {} times after an IBPB they asked for, each issued before ({} \
                                   issued)", r.asked, r.asked_barriers);
        }
    }
    let c = r.controls;
    if c.spec_ctrl != 0 || c.pred_cmd != 0 {
        let _ = writeln!(out, "hv: a guest's own speculation controls -- IA32_SPEC_CTRL {:#x} over the host's for its \
                               run, IA32_PRED_CMD {:#x}", c.spec_ctrl, c.pred_cmd);
    }
    if r.auto_ibrs {
        let _ = writeln!(out, "hv: automatic IBRS on -- the host's indirect branches are not a guest's to steer");
    }
    if a.ibpb_exit {
        let _ = write!(out, "hv: an IBPB after every exit --");
        let mut sep = " ";
        for (open, name) in [(a.bti, "BTI"), (a.retbleed, "Retbleed"), (a.srso && !a.srso_open, "SRSO")] {
            if open {
                let _ = write!(out, "{}{}", sep, name);
                sep = ", ";
            }
        }
        let _ = writeln!(out, ": what a guest trained steers none of the host's branches and returns");
    }
    if a.stibp {
        let _ = writeln!(out, "hv: STIBP on -- what a guest trains on one thread of a core steers nothing on the other");
    }
    if a.branches_open {
        let _ = writeln!(out, "hv: this CPU is open to {} and has no IBPB to close it: the host's branches are a \
                               guest's to steer", if a.retbleed { "BTI and Retbleed" } else { "BTI" });
    }
    if a.srso_open {
        let _ = writeln!(out, "hv: this CPU is open to SRSO and is not said to flush return predictions with its IBPB \
                               (IBPB_BRTYPE): the host's returns are a guest's to steer");
    }
    if a.smt_open {
        let _ = writeln!(out, "hv: a core's two threads share their branch predictors here, and the CPU has no STIBP to \
                               keep them apart");
    }
    let i = r.intel;
    let bhi = if i.bhi_dis_s { ", and BHI_DIS_S: nor by the branch history a guest leaves" } else { "" };
    match i.ibrs {
        Some(Ibrs::Enhanced) => {
            let _ = writeln!(out, "hv: enhanced IBRS on -- the host's indirect branches are not a guest's to steer{}",
                             bhi);
        }
        Some(Ibrs::EveryExit) => {
            let _ = writeln!(out, "hv: IBRS written after every exit -- the host's indirect branches are not a \
                                   guest's to steer{}", bhi);
        }
        None => {}
    }
    if i.verw {
        let _ = writeln!(out, "hv: the CPU's buffers cleared (VERW) before every entry -- MDS, TAA or RFDS");
    }
    if i.l1d_flush {
        let _ = writeln!(out, "hv: the L1 data cache flushed before every entry -- L1TF");
    }
    if i.bhi_open {
        let _ = writeln!(out, "hv: this CPU is open to branch history injection and has no BHI_DIS_S to close it: \
                               the host's indirect branches are a guest's to steer through the history it leaves");
    }
}

/* What `ask_spec_ctrl` answers: not asked (a CPU that did not run it),
 * nothing set there to keep, kept, undone. */
#[cfg(target_arch = "x86_64")]
const SPEC_UNASKED: u32 = 0;
#[cfg(target_arch = "x86_64")]
const SPEC_NONE: u32 = 1;
#[cfg(target_arch = "x86_64")]
const SPEC_HELD: u32 = 2;
#[cfg(target_arch = "x86_64")]
const SPEC_UNDONE: u32 = 3;

/// Whether the CPU this runs on still has the IA32_SPEC_CTRL `hv on` set
/// there: the IPI `speculation` sends each CPU.
#[cfg(target_arch = "x86_64")]
fn ask_spec_ctrl(answer: &AtomicU32) {
    let held = match hv::spec::held() {
        None => SPEC_NONE,
        Some(true) => SPEC_HELD,
        Some(false) => SPEC_UNDONE,
    };
    answer.store(held, Ordering::Release);
}

/// The huge pool a guest's memory comes from, when the kernel has one.
fn pool_report(out: &mut Output) {
    const MIB: u64 = 1024 * 1024;
    let pool = kcore::frame::huge_pool();
    if pool.blocks == 0 {
        let _ = writeln!(out, "  {:<21}none -- hugepages= not given: a guest's memory is 4 KiB frames", "huge pool");
    } else {
        let _ = writeln!(out, "  {:<21}{} of {} 2 MiB pages free ({} of {} MiB): every guest's memory", "huge pool",
                         pool.free, pool.blocks, pool.free * kcore::frame::HUGE_SIZE / MIB,
                         pool.blocks * kcore::frame::HUGE_SIZE / MIB);
    }
}

fn status(machine: &Machine, out: &mut Output) {
    match machine.ext() {
        Ok(ext) => {
            /* What the CPUs say, not what this module remembers telling
             * them: the two can only differ through a bug here, and the
             * whole point of asking is to see it. */
            let hardware = machine.hardware_mask();
            let _ = write!(out, "hv: {} on for cpu ", ext.vendor().name());
            write_mask(out, hardware);
            let _ = write!(out, " of ");
            write_mask(out, kcore::cpu::online_mask());
            let _ = writeln!(out);

            /* Each way round says something different, so each is said
             * on its own, naming the CPUs. */
            let ours = machine.enabled_mask();
            let stray = hardware & !ours;
            if stray != 0 {
                let _ = write!(out, "hv: WARNING -- on for cpu ");
                write_mask(out, stray);
                let _ = writeln!(out, " without a page from this module: left on by an earlier load? hv off clears it");
            }
            let lost = ours & !hardware;
            if lost != 0 {
                let _ = write!(out, "hv: WARNING -- off for cpu ");
                write_mask(out, lost);
                let _ = writeln!(out, " though this module turned it on: something turned it off behind its back");
            }
            #[cfg(target_arch = "x86_64")]
            speculation(out, hardware);
        }
        Err(e) => {
            let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        }
    }
}

/// Guests to run, and where what they did is written: what a vCPU's task
/// shares with the command that is waiting for it.
struct Job {
    machine: Arc<Machine>,
    guests: Vec<&'static str>,
    report: Mutex<String>,
}

impl Job {
    /// The vCPU's task: every guest of the job in turn, on whichever CPU
    /// the task was bound to.
    fn run(job: Arc<Job>) {
        let mut report = String::new();
        let mut ok = 0usize;
        for name in &job.guests {
            if hv::guests::run_one(&job.machine, name, &mut report) == Some(true) {
                ok += 1;
            }
        }
        if job.guests.len() > 1 {
            let _ = writeln!(report, "hv: {} of {} guests ok", ok, job.guests.len());
        }
        *job.report.lock() = report;
    }
}

/// `hv run <guest|all> [cpu]`: each guest in a VM of its own, on a task of
/// its own -- a guest's CPU is a task, here as it will be for a Linux one --
/// bound to `cpu` when one is named, and waited for.
fn run(machine: &Arc<Machine>, which: Option<&str>, cpu: Option<&str>, out: &mut Output) {
    /* First, so that a machine with nothing to run a guest under says that,
     * and not that it has no such guest. */
    if let Err(e) = machine.ext() {
        let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        return;
    }
    let guests: Vec<&'static str> = match which {
        Some("all") => hv::guests::names().collect(),
        Some(name) => hv::guests::names().filter(|g| *g == name).collect(),
        None => Vec::new(),
    };
    if guests.is_empty() {
        let _ = write!(out, "hv: run which guest? all");
        for name in hv::guests::names() {
            let _ = write!(out, ", {}", name);
        }
        let _ = writeln!(out);
        return;
    }

    let affinity = match cpu {
        None => None,
        Some(word) => match word.parse::<usize>() {
            Ok(cpu) if cpu < MAX_CPUS && kcore::cpu::online_mask() & (1u64 << cpu) != 0 => Some(1u64 << cpu),
            _ => {
                let _ = writeln!(out, "hv: \"{}\" is not a running CPU", word);
                return;
            }
        },
    };

    let report = match Mutex::new(String::new()) {
        Some(report) => report,
        None => {
            let _ = writeln!(out, "hv: out of memory");
            return;
        }
    };
    let job = Arc::new(Job { machine: machine.clone(), guests, report });
    let task = match affinity {
        Some(mask) => kcore::task::spawn_on_with("hv/vcpu", mask, job.clone(), Job::run),
        None => kcore::task::spawn_with("hv/vcpu", job.clone(), Job::run),
    };
    match task {
        /* Dropping the handle waits for the task. */
        Some(task) => drop(task),
        None => {
            let _ = writeln!(out, "hv: no task for the guest");
            return;
        }
    }
    out.write_bytes(job.report.lock().as_bytes());
}

/// The benchmark's default count of exits, and its most.
#[cfg(target_arch = "x86_64")]
const BENCH_DEFAULT: u32 = 100_000;
#[cfg(target_arch = "x86_64")]
const BENCH_MAX: u32 = 10_000_000;
#[cfg(target_arch = "x86_64")]
const BENCH_USAGE: &str = "hv bench [cpu=N] [exits=N] [pages=N] [flush] [profile] [mmio]";

/// What `hv bench` runs on its CPU.
#[cfg(target_arch = "x86_64")]
struct BenchJob {
    machine: Arc<Machine>,
    exits: u32,
    pages: u32,
    flush: bool,
    profile: bool,
    mmio: bool,
    result: Mutex<Option<core::result::Result<hv::guests::Bench, String>>>,
}

/// `hv bench [cpu=N] [exits=N] [pages=N] [flush] [profile]`: a guest of
/// CPUIDs, each after a read of each of `pages` pages, on a task bound to
/// `cpu` (the extension's last CPU unless told), timed -- with a flush of the
/// whole TLB on every entry when `flush` says so, for what that costs, and
/// each entry timed in its parts when `profile` does.
#[cfg(target_arch = "x86_64")]
fn bench<'a>(machine: &Arc<Machine>, words: impl Iterator<Item = &'a str>, out: &mut Output) {
    if let Err(e) = hv::run::ensure_runnable(machine) {
        let _ = writeln!(out, "hv: no guest can run here -- {}", e);
        return;
    }
    let enabled = machine.enabled_mask();
    if enabled == 0 {
        let _ = writeln!(out, "hv: the extension is on for no cpu -- hv on first");
        return;
    }
    let mut cpu = u64::BITS - 1 - enabled.leading_zeros();
    let (mut exits, mut pages, mut flush, mut profile, mut mmio) = (BENCH_DEFAULT, 0u32, false, false, false);
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        let number = value.parse::<u32>();
        match (key, number) {
            ("cpu", Ok(c)) if c < u64::BITS && enabled & (1u64 << c) != 0 => cpu = c,
            ("cpu", _) => {
                let _ = writeln!(out, "hv: the extension is not on for cpu \"{}\"", value);
                return;
            }
            ("exits", Ok(n)) if (1..=BENCH_MAX).contains(&n) => exits = n,
            ("pages", Ok(n)) if n <= hv::guests::BENCH_MAX_PAGES => pages = n,
            ("flush", _) if value.is_empty() => flush = true,
            ("profile", _) if value.is_empty() => profile = true,
            ("mmio", _) if value.is_empty() => mmio = true,
            _ => {
                let _ = writeln!(out, "hv: {} -- exits 1..{}, pages 0..{}", BENCH_USAGE, BENCH_MAX,
                                 hv::guests::BENCH_MAX_PAGES);
                return;
            }
        }
    }
    /* The flush is AMD-V's: the entry with no ASID kept. On VT-x every
     * entry flushes anyway, so a number under that word would not be what
     * was asked for. */
    let vmx = machine.ext() == Ok(hv::Ext::Vmx);
    if mmio && (pages != 0 || flush) {
        let _ = writeln!(out, "hv: mmio is a loop of MMIO loads alone -- without pages or flush");
        return;
    }
    if flush && vmx {
        let _ = writeln!(out, "hv: flush is AMD-V's -- under VT-x the TLB is flushed on every entry as it is");
        return;
    }
    let Some(result) = Mutex::new(None) else {
        let _ = writeln!(out, "hv: out of memory");
        return;
    };
    let job = Arc::new(BenchJob { machine: machine.clone(), exits, pages, flush, profile, mmio, result });
    let task = kcore::task::spawn_on_with("hv/bench", 1u64 << cpu, job.clone(), |job: Arc<BenchJob>| {
        let r = hv::guests::bench(&job.machine, job.exits, job.pages, job.flush, job.profile, job.mmio);
        *job.result.lock() = Some(r);
    });
    match task {
        /* Dropping the handle waits for the task. */
        Some(task) => drop(task),
        None => {
            let _ = writeln!(out, "hv: no task for the guest");
            return;
        }
    }
    let result = job.result.lock().take();
    match result {
        Some(Ok(b)) => {
            let ns = b.ns.max(1);
            let exits = b.exits.max(1);
            let _ = if mmio {
                writeln!(out, "hv: {} MMIO loads on cpu {}, each a nested fault decoded and performed: {} ms, {} ns an access, {} a second ({} host interrupts among them)",
                    b.exits, cpu, ns / 1_000_000, ns / exits, b.exits * 1_000_000_000 / ns, b.host)
            } else {
                writeln!(out, "hv: {} exits on cpu {}, {} pages read between, {}: {} ms, {} ns an exit, {} a second ({} host interrupts among them)",
                    b.exits, cpu, pages, if flush { "the TLB flushed on every entry" } else { "ASIDs kept" },
                    ns / 1_000_000, ns / exits, b.exits * 1_000_000_000 / ns, b.host)
            };
            if let Some(p) = b.profile {
                /* Ticks to nanoseconds by the two clocks' own ratio over the
                 * run: whatever the TSC's rate, the parts add up to the whole. */
                let part = |ticks: u64| ticks * ns / b.ticks.max(1) / p.entries.max(1);
                let entry = part(p.checks) + part(p.sync_in) + part(p.switch_in) + part(p.world)
                    + part(p.switch_out) + part(p.sync_out);
                let _ = if vmx {
                    write!(out, "hv: an exit, in ns: checks and VMPTRLD {}, VMCS written {}, x87/SSE in {}, vmresume to exit {}, x87/SSE out {}, VMCS read {}; the rest -- the exit handled, the loop -- {}",
                        part(p.checks), part(p.sync_in), part(p.switch_in), part(p.world), part(p.switch_out),
                        part(p.sync_out), (ns / exits).saturating_sub(entry))
                } else {
                    write!(out, "hv: an exit, in ns: checks and ASID {}, x87/SSE in {}, vmrun to #vmexit {}, x87/SSE out {}; the rest -- the exit handled, the loop -- {}",
                        part(p.checks), part(p.switch_in), part(p.world), part(p.switch_out),
                        (ns / exits).saturating_sub(entry))
                };
                let _ = if mmio {
                    writeln!(out, ", of which the access -- the walk to its instruction, the fetch, the decoding, the device -- {}",
                        part(b.emulate))
                } else {
                    writeln!(out)
                };
            }
        }
        Some(Err(why)) => {
            let _ = writeln!(out, "hv: the bench did not finish -- {}", why);
        }
        None => {
            let _ = writeln!(out, "hv: the bench's task said nothing");
        }
    }
}

/// `hv on [cpu|all]` and `hv off [cpu|all]`, which differ in one word and
/// in nothing else worth writing twice -- but for a guest `hv start` left
/// running, which `hv off` will not turn the extension off under.
fn switch(state: &State, which: Option<&str>, on: bool, out: &mut Output) {
    let machine = &*state.machine;
    let mask = match which {
        None | Some("all") => kcore::cpu::online_mask(),
        Some(word) => match word.parse::<usize>() {
            Ok(cpu) if cpu < MAX_CPUS => 1u64 << cpu,
            _ => {
                let _ = writeln!(out, "hv: \"{}\" is not a CPU -- a number under {}, or all",
                                 word, MAX_CPUS);
                return;
            }
        },
    };

    /* A courtesy, not what keeps the CPU safe: a guest started between this
     * look and the switch finds the extension off at its next entry, which
     * checks with interrupts off, and stops there -- "not entered" in its
     * `hv list` line. */
    #[cfg(target_arch = "x86_64")]
    if !on {
        if let Some((id, cpu)) = state.vms.running_on(mask) {
            let _ = writeln!(out, "hv: vm {} is running on cpu {} -- hv stop it first", id, cpu);
            return;
        }
    }

    let changed = if on {
        match machine.enable(mask) {
            Ok(changed) => changed,
            Err(refused) => {
                let _ = match refused.cpu {
                    Some(cpu) => writeln!(out, "hv: cpu {} would not take it -- {}", cpu, refused.error),
                    None => writeln!(out, "hv: cannot turn it on -- {}", refused.error),
                };
                /* Whatever did come on before the CPU that refused stays on,
                 * and the status line below says which. */
                status(machine, out);
                return;
            }
        }
    } else {
        let was = machine.disable(mask);
        if was.busy != 0 {
            /* What the courtesy check above did not see -- an `hv boot` or
             * `hv run` in another shell, a guest started since: the CPU
             * itself refused, and stays on. */
            let _ = write!(out, "hv: a guest's VMCS is still current on cpu ");
            write_mask(out, was.busy);
            let _ = writeln!(out, " -- left on; stop the guest first");
        }
        was.off
    };

    let _ = write!(out, "hv: turned {} for cpu ", if on { "on" } else { "off" });
    write_mask(out, changed);
    let _ = writeln!(out);
    status(machine, out);
}

/// A CPU mask as people read one: `0-3`, `0,2-5`, or `none`.
fn write_mask(out: &mut Output, mask: u64) {
    if mask == 0 {
        let _ = write!(out, "none");
        return;
    }

    let mut first = true;
    let mut i = 0usize;
    while i < u64::BITS as usize {
        if mask & (1u64 << i) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < u64::BITS as usize && mask & (1u64 << i) != 0 {
            i += 1;
        }
        let _ = write!(out, "{}", if first { "" } else { "," });
        first = false;
        if start == i - 1 {
            let _ = write!(out, "{}", start);
        } else {
            let _ = write!(out, "{}-{}", start, i - 1);
        }
    }
}

kmod::module!(name: "hv", init_with_params: init);
