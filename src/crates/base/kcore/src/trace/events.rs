//! The kernel's events Rust records: their numbers and their payloads, and
//! nothing of the kernel besides -- so that a fuzzer compiles this file as it
//! is (fuzz/hv) and its sites build the payloads the kernel's do.

use crate::pod::Pod;

/// A payload's field, as the self-test holds it to the kernel's description:
/// and for an enumeration, what its values from 0 up are called.
#[derive(Debug)]
pub struct FieldAt {
    pub name: &'static str,
    pub offset: usize,
    pub size: usize,
    /// An enumeration's names, value by value; none for any other field
    pub names: &'static [&'static str],
}

/// An event's payload: plain data of the layout the kernel's description
/// gives it, and the event it is a record of -- so that no payload can be
/// emitted as another event's.
pub trait Event: Pod {
    const ID: Id;
    /// The event's name, as the kernel's description has it
    const NAME: &'static str;
    /// Every field, padding -- a name that starts with `_` -- included
    const FIELDS: &'static [FieldAt];
}

/// A payload as the self-test compares it with the kernel's description.
pub struct Described {
    pub id: Id,
    pub name: &'static str,
    pub size: usize,
    pub fields: &'static [FieldAt],
}

const fn described<E: Event>() -> Described {
    Described { id: E::ID, name: E::NAME, size: core::mem::size_of::<E>(), fields: E::FIELDS }
}

/// What an enumerated field's values are, from one list: the type a site
/// says a value by -- `#[repr(u8)]`, its variants numbered from 0 in the
/// list's order -- and `NAMES`, what the kernel's description calls each,
/// which the boot's self-test holds to the kernel's value by value. A value
/// is written into its payload's field as `Kind::Variant as u8`.
macro_rules! names {
    ($(
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $text:literal,)*
        }
    )*) => {
        $(
            $(#[$meta])*
            #[repr(u8)]
            #[derive(Clone, Copy, PartialEq, Eq, Debug)]
            pub enum $name {
                $($(#[$vmeta])* $variant,)*
            }

            impl $name {
                /// What each value is called, from 0 up: the kernel's names
                pub const NAMES: &'static [&'static str] = &[$($text,)*];
            }
        )*
    };
}

/// The names a field's `as` gives it, or none.
macro_rules! field_names {
    () => {
        &[]
    };
    ($names:ident) => {
        $names::NAMES
    };
}

/// The events, from one list: `Id` and its numbers, each payload's struct --
/// `#[repr(C)]`, each field an integer or an array of them, and no padding,
/// which is asserted -- the event it is a record of, and `EVENTS`, which the
/// boot's self-test (`check`) walks. An event added to the list is in all of
/// them at once: none can be left out of the check. A field `as` a type of
/// `names!` holds that type's values, by their names.
macro_rules! events {
    ($(
        $(#[$meta:meta])*
        $name:ident = $num:literal, $text:literal {
            $($(#[$fmeta:meta])* $field:ident: $ty:ty $(as $names:ident)?,)*
        }
    )*) => {
        /// The kernel's events this side records, by the kernel's numbers
        /// (`tp::Id`, kernel/tracepoint.h): appended to, never reordered --
        /// a saved trace names events by number.
        #[repr(u16)]
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Id {
            $($name = $num,)*
        }

        /// One more than the largest `Id`: how many of the kernel's flags
        /// this side reads.
        pub const ID_LIMIT: usize = {
            let ids = [$($num as usize,)*];
            let mut most = 0;
            let mut i = 0;
            while i < ids.len() {
                if ids[i] + 1 > most {
                    most = ids[i] + 1;
                }
                i += 1;
            }
            most
        };

        $(
            $(#[$meta])*
            #[repr(C)]
            #[derive(Clone, Copy, Debug, Default)]
            pub struct $name {
                $($(#[$fmeta])* pub $field: $ty,)*
            }

            const _: () = assert!(
                core::mem::size_of::<$name>() == 0 $(+ core::mem::size_of::<$ty>())*,
                "a payload with padding, which the kernel would copy out"
            );

            /* SAFETY: #[repr(C)], made of integers and arrays of them, and
               with no padding (the assertion above) */
            unsafe impl Pod for $name {}

            impl Event for $name {
                const ID: Id = Id::$name;
                const NAME: &'static str = $text;
                const FIELDS: &'static [FieldAt] = &[
                    $(FieldAt {
                        name: stringify!($field),
                        offset: core::mem::offset_of!($name, $field),
                        size: core::mem::size_of::<$ty>(),
                        names: field_names!($($names)?),
                    },)*
                ];
            }
        )*

        /// Every payload declared here, each held to the kernel's
        /// description by the boot's self-test (`check`).
        pub const EVENTS: &[Described] = &[$(described::<$name>(),)*];
    };
}

names! {
    /// Why a guest CPU came out (`hv_exit`'s `reason`): one value a kind of
    /// hv's `Exit` (src/crates/hypervisor/hv/src/svm.rs)
    ExitReason {
        Host = "host",
        Kicked = "kicked",
        Io = "io",
        Hlt = "hlt",
        Cpuid = "cpuid",
        Msr = "msr",
        Hypercall = "hypercall",
        IrqWindow = "irq_window",
        NmiWindow = "nmi_window",
        NestedFault = "npf",
        Exception = "exception",
        MachineCheck = "machine_check",
        Shutdown = "shutdown",
        Invalid = "invalid",
        Cr8 = "cr8",
        Cr0 = "cr0",
        Cr4 = "cr4",
        Dr = "dr",
        Eoi = "eoi",
        Pause = "pause",
        Undefined = "undefined",
        Other = "other",
    }

    /// What goes in at an entry (`hv_inject`'s `kind`)
    InjectKind {
        /// An exception of the guest's own, given back
        Exception = "exception",
        Nmi = "nmi",
        /// The 8259's, through LINT0
        Pic = "pic",
        /// Its local APIC's
        Apic = "apic",
        /// None it can take yet: the CPU is to exit when it can
        Window = "window",
    }

    /// What a halted CPU sleeps until (`hv_halt`'s `until`)
    HaltUntil {
        /// Its local APIC timer's edge
        Timer = "timer",
        /// The PIT's next edge: the first CPU's
        Pit = "pit",
        /// The devices' next round: the first CPU's
        Device = "device",
        /// The end of its run
        Deadline = "deadline",
        /// Nothing armed: the longest it waits without a look
        Nothing = "nothing",
    }

    /// How a halt ended (`hv_wake`'s `how`)
    WakeHow {
        /// Rung within the spin, never asleep
        Polled = "polled",
        /// Rung while asleep
        Rung = "rung",
        /// Its time came
        Timed = "timed",
        /// Woke short of its timer's edge, and spun the rest
        Advanced = "advanced",
    }

    /// What a CPU's task did before the round that found its timer's edge
    /// (`hv_timer`'s `how`, the run loop's `Step`): what made the interrupt
    /// late, when it was
    TimerHow {
        /// No step yet: a count's, never a record's
        Nothing = "nothing",
        /// Slept to a time with the edge in it, and woke late
        WokeLate = "woke_late",
        /// Slept so, and woke on time
        WokeOnTime = "woke_on_time",
        /// Slept to a time without that edge: none armed, or another
        NotSleptFor = "not_slept_for",
        /// Spun, or slept for good
        Spun = "spun",
        /// Ran its guest and came round long after the exit: off its CPU, or
        /// busy
        OffCpu = "off_cpu",
        /// Ran its guest until an interrupt of the host's took it out
        Host = "host",
        /// Ran its guest until an exit of its own
        Exit = "exit",
        Other = "other",
    }

    /// How a CPU was rung (`hv_ring`'s `how`)
    RingHow {
        /// Its event signalled, and its kick sent
        Rung = "rung",
        /// Found spinning for a ring: the word and the kick alone
        Polling = "polling",
        /// A posted interrupt: the CPU it runs on notified to take it in the
        /// guest
        Notified = "notified",
        /// Its event alone, for what can wait for its next exit
        Signalled = "signalled",
    }

    /// What a guest disk's request is (`blk_*`'s and `disk_*`'s `op`): hv's
    /// `Op`, a value a kind
    BlkOp {
        Read = "read",
        Write = "write",
        Flush = "flush",
        Discard = "discard",
        WriteZeroes = "write_zeroes",
    }

    /// How the disk's task served a request (`disk_done`'s `how`)
    DiskHow {
        /// With the device, and given back by the image's reap
        Async = "async",
        /// Done by the image's start then and there -- its synchronous path,
        /// zeros, or a refusal
        AtOnce = "at_once",
        /// A flush, a discard or a write of zeros, served on its own
        Command = "command",
    }

    /// What sent a TCP segment (`tcp_send`'s `why`)
    TcpWhy {
        /// Data the program wrote, sent for the first time
        New = "new",
        /// A copy of what was found lost: a recovery's, NewReno's
        Again = "again",
        /// A loss probe: the newest segment again (RFC 8985 7)
        Probe = "probe",
        /// The retransmission timeout's
        Timeout = "timeout",
        /// A SYN or a SYN-ACK
        Handshake = "handshake",
        /// The FIN
        Close = "close",
        Reset = "reset",
        /// The persist timer's probe of a shut window
        Persist = "persist",
    }

    /// The sender's recovery as a segment went (`tcp_send`'s `recovery`)
    TcpRecovery {
        None = "none",
        Fast = "fast",
        Timeout = "timeout",
    }

    /// What became of a segment a connection took in (`tcp_recv`'s
    /// `verdict`)
    TcpVerdict {
        /// Taken, as the state machine has it
        Taken = "taken",
        /// A copy of what had come already, answered with a D-SACK
        Copy = "copy",
        /// Outside the window (RFC 9293 3.10.7.4), answered at most twice a
        /// second
        Outside = "outside",
        /// An acknowledgement of nothing sent, or older than the peer's
        /// largest window (RFC 5961 5.2)
        BadAck = "bad_ack",
        /// A SYN on a synchronized connection (RFC 5961 4.2)
        Syn = "syn",
        /// A reset, taken
        Reset = "reset",
        /// A reset not at RCV.NXT, answered (RFC 5961 3.2)
        ResetRefused = "reset_refused",
    }

    /// What a TCP sender made of a loss (`tcp_loss`'s `what`)
    TcpLossWhat {
        /// RACK found a loss, and a recovery began
        Recovery = "recovery",
        /// Everything outstanding at its start acknowledged
        End = "end",
        Timeout = "timeout",
        /// A timeout an original's arrival showed was for nothing
        Spurious = "spurious",
        /// A recovery every copy of which the peer said came twice
        Undo = "undo",
        /// A loss probe that repaired a loss: the window halved
        ProbeRepair = "probe_repair",
        /// A peer that let go of what it had SACKed
        Reneged = "reneged",
    }

    /// What a program's TCP call waited for (`tcp_wait`'s `what`)
    TcpWaitFor {
        /// Room in the send buffer
        Buffer = "buffer",
        /// The peer's window
        Window = "window",
        /// The congestion window
        Cwnd = "cwnd",
        /// What was lost, going again first
        Lost = "lost",
        /// Room in the table of what is in flight
        Table = "table",
        /// Data, for a receive
        Data = "data",
    }
}

events! {
    /// `test`: what `trace test` and the boot's self-test record -- here,
    /// what the self-test has Rust record, by the path a module's sites take
    Test = 0, "test" {
        seq: u64,
        value: u64,
    }

    /// `hv_exit`: a guest CPU out of its guest -- or turned back on its way
    /// in -- recorded for every entry (`hv::vm::Cpu::enter`). Its times are
    /// the cycle counter's around the world switch (`hvarch`'s `Run`)
    HvExit = 12, "hv_exit" {
        /// What it was about, by its reason (`hv::tp::exit_fields`)
        detail: u64,
        rip: u64,
        pid: u32,
        /// From the entry to the exit
        guest_ns: u32,
        /// From the exit before to this entry: 0 when that one was not timed
        out_ns: u32,
        /// From the exit to the record: held as the counter's low half at
        /// the exit (0: not timed), the kernel reading it back as the
        /// nanoseconds from it to the record's stamp (`Kind::Since`)
        since_ns: u32,
        /// More of it, by its reason
        info: u32,
        vcpu: u16,
        reason: u8 as ExitReason,
        _pad: u8,
    }

    /// `hv_inject`: what goes into a guest CPU at its entry
    HvInject = 13, "hv_inject" {
        pid: u32,
        vcpu: u16,
        vector: u8,
        kind: u8 as InjectKind,
    }

    /// `hv_halt`: a halted CPU with nothing to take, about to spin and sleep
    HvHalt = 14, "hv_halt" {
        pid: u32,
        wait_ns: u32,
        /// The spin first: 0 for none
        poll_ns: u32,
        vcpu: u16,
        until: u8 as HaltUntil,
        _pad: u8,
    }

    /// `hv_wake`: that halt over
    HvWake = 15, "hv_wake" {
        pid: u32,
        halted_ns: u32,
        /// Past the time it slept to
        late_ns: u32,
        vcpu: u16,
        how: u8 as WakeHow,
        _pad: u8,
    }

    /// `hv_timer`: a CPU's APIC timer interrupt asked for
    HvTimer = 16, "hv_timer" {
        pid: u32,
        /// Past its edge
        late_ns: u32,
        vcpu: u16,
        how: u8 as TimerHow,
        _pad: u8,
    }

    /// `hv_ring`: a guest CPU rung, or notified
    HvRing = 17, "hv_ring" {
        /// Whoever rang it
        pid: u32,
        /// The CPU rung
        vcpu: u16,
        how: u8 as RingHow,
        _pad: u8,
    }

    /// `blk_submit`: a guest's request taken off its virtio disk's ring and
    /// handed to the backend (`hv::devices::blk`)
    BlkSubmit = 18, "blk_submit" {
        offset: u64,
        len: u32,
        pid: u32,
        slot: u16,
        /// Its disk among its guest's, vda 0
        disk: u8,
        op: u8 as BlkOp,
        _pad: [u8; 4],
    }

    /// `blk_complete`: that request back from the backend, to be given back
    BlkComplete = 19, "blk_complete" {
        offset: u64,
        len: u32,
        pid: u32,
        slot: u16,
        disk: u8,
        op: u8 as BlkOp,
        ok: u8,
        _pad: [u8; 3],
    }

    /// `disk_start`: the request picked up by the disk's task
    DiskStart = 20, "disk_start" {
        offset: u64,
        len: u32,
        pid: u32,
        slot: u16,
        disk: u8,
        op: u8 as BlkOp,
        _pad: [u8; 4],
    }

    /// `disk_done`: the request done in the disk's task
    DiskDone = 21, "disk_done" {
        pid: u32,
        slot: u16,
        disk: u8,
        op: u8 as BlkOp,
        how: u8 as DiskHow,
        ok: u8,
        _pad: [u8; 2],
    }

    /// `image_sync`: a request an image served then and there
    ImageSync = 22, "image_sync" {
        offset: u64,
        ns: u32,
        len: u32,
        pid: u32,
        write: u8,
        ok: u8,
        _pad: [u8; 2],
    }

    /// `image_note`: an image's blocks noted written over unwritten extents
    ImageNote = 23, "image_note" {
        from: u64,
        ns: u32,
        blocks: u32,
        pid: u32,
        ok: u8,
        _pad: [u8; 3],
    }

    /// `bio_submit`: a command handed to a device through the block layer's
    /// asynchronous queue
    BioSubmit = 24, "bio_submit" {
        sector: u64,
        count: u32,
        pid: u32,
        entry: u16,
        buffer: u16,
        in_flight: u16,
        write: u8,
        _pad: u8,
    }

    /// `bio_done`: its completion, or its refusal
    BioDone = 25, "bio_done" {
        entry: u16,
        buffer: u16,
        ok: u8,
        refused: u8,
        _pad: [u8; 2],
    }

    /// `ext4_commit`: an ext4 transaction committed to its journal
    Ext4Commit = 26, "ext4_commit" {
        seq: u32,
        blocks: u32,
        ns: u32,
        checkpoint_ns: u32,
        pid: u32,
        ok: u8,
        _pad: [u8; 3],
    }

    /// `tcp_send`: a segment a connection sent, a pure acknowledgement apart
    TcpSend = 27, "tcp_send" {
        seq: u32,
        ack: u32,
        cwnd: u32,
        /// What was in flight against the congestion window
        pipe: u32,
        pid: u32,
        lport: u16,
        rport: u16,
        len: u16,
        /// The window field it offered
        wnd: u16,
        slot: u8,
        flags: u8,
        why: u8 as TcpWhy,
        recovery: u8 as TcpRecovery,
    }

    /// `tcp_ack`: a pure acknowledgement a connection sent
    TcpAck = 28, "tcp_ack" {
        ack: u32,
        /// The right edge of the window offered
        edge: u32,
        /// The receive buffer's room
        free: u32,
        lport: u16,
        rport: u16,
        wnd: u16,
        slot: u8,
        blocks: u8,
        dsack: u8,
        ooo: u8,
        _pad: [u8; 2],
    }

    /// `tcp_recv`: a segment a connection took in, and what became of it
    TcpRecv = 29, "tcp_recv" {
        seq: u32,
        ack: u32,
        rcv_nxt: u32,
        free: u32,
        /// How far past RCV.NXT the window last offered reaches
        owed: u32,
        lport: u16,
        rport: u16,
        len: u16,
        wnd: u16,
        slot: u8,
        flags: u8,
        verdict: u8 as TcpVerdict,
        sacks: u8,
    }

    /// `tcp_loss`: what a connection's sender made of a loss
    TcpLoss = 30, "tcp_loss" {
        cwnd: u32,
        ssthresh: u32,
        pipe: u32,
        sacked: u32,
        lost: u32,
        una: u32,
        nxt: u32,
        lport: u16,
        rport: u16,
        slot: u8,
        what: u8 as TcpLossWhat,
        _pad: [u8; 2],
    }

    /// `tcp_rtt`: a round trip a connection measured
    TcpRtt = 31, "tcp_rtt" {
        sample_us: u32,
        srtt_us: u32,
        rttvar_us: u32,
        rto_ms: u32,
        min_rtt_us: u32,
        lport: u16,
        rport: u16,
        slot: u8,
        _pad: [u8; 3],
    }

    /// `tcp_wait`: a program's send or receive that waited
    TcpWait = 32, "tcp_wait" {
        ns: u32,
        pid: u32,
        lport: u16,
        rport: u16,
        slot: u8,
        what: u8 as TcpWaitFor,
        _pad: [u8; 2],
    }
}
