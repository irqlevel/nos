//! The kernel's events Rust records: their numbers and their payloads, and
//! nothing of the kernel besides -- so that a fuzzer compiles this file as it
//! is (tests/fuzz/hv) and its sites build the payloads the kernel's do.

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

    /// Why ext4 committed its group (`ext4_commit`'s `why`)
    CommitWhy {
        /// A sync -- or a call that is to return with what it did on the
        /// medium (`mark_written`)
        Sync = "sync",
        /// Its first transaction staged five seconds ago or more: the
        /// background task's look, or the next transaction's
        Age = "age",
        /// The next transaction would make it bigger than the journal takes
        /// at once
        Full = "full",
        /// An allocation wanted the blocks it freed
        Room = "room",
        /// No task commits in the background: each transaction commits as
        /// it is staged
        Alone = "alone",
        /// The unmount
        Unmount = "unmount",
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

    /// Which way a packet a firewall kept out went (`fw_deny`'s `dir`)
    FwDir {
        /// To the VM
        In = "in",
        /// From it
        Out = "out",
    }

    /// What kept it out (`fw_deny`'s `why`)
    FwWhy {
        /// A rule of its firewall's
        Rule = "rule",
        /// Its direction's policy
        Policy = "policy",
        /// An address not the VM's: to it for another, or from it as another
        Stray = "stray",
        /// An IPv4 fragment, which carries no ports
        Fragment = "fragment",
        /// IPv4 that does not hold together
        Broken = "broken",
        /// IPv6 its firewall does not let through
        V6 = "ipv6",
        /// A port with no table or no firewall to judge it by
        Unready = "unready",
    }

    /// What a datagram of vnet's tunnel carried (`tunnel_drop`'s `kind`):
    /// vnetwire's numbers, and none for one with no header
    TunnelKind {
        None = "none",
        Port = "port",
        ToGateway = "to_gateway",
        FromGateway = "from_gateway",
        Echo = "echo",
        EchoAnswer = "echo_answer",
    }

    /// Why the tunnel took a datagram, or sent a packet, no further
    /// (`tunnel_drop`'s `why`)
    TunnelWhy {
        /// From no host of the cluster's
        Unknown = "unknown",
        /// Under no key known -- or, sending, with none to send by
        NoKey = "no_key",
        /// A counter its key's window has seen
        Replayed = "replayed",
        /// A tag that does not hold
        Forged = "forged",
        /// What the table does not let through
        Stale = "stale",
        /// No header
        Malformed = "malformed",
        /// No room on the uplink, or in the pool
        NoRoom = "no_room",
        /// No next hop known
        NoHop = "no_hop",
    }

    /// Which side of a migration a record is of (`mig_take`'s and
    /// `mig_end`'s `side`)
    MigSide {
        /// The host it leaves
        Old = "old",
        /// The host it comes to
        New = "new",
    }

    /// What NBS answered a take of a volume (`mig_take`'s `took`)
    MigTook {
        Took = "took",
        /// Its writer is not of the generation named: the other side's take
        /// landed first
        Refused = "refused",
        /// Nothing to go by: asked again
        Unknown = "unknown",
    }

    /// How a side's migration ended (`mig_end`'s `how`)
    MigHow {
        /// The VM runs on the new host
        There = "there",
        /// It runs on the old one
        Here = "here",
        /// It runs on neither: its volume's writer one that cannot run it
        Stopped = "stopped",
        /// Refused before anything was paused
        Refused = "refused",
        /// Given up before its Done
        Cancelled = "cancelled",
    }

    /// A message the new host takes (`mig_recv`'s `kind`)
    MigKind {
        Offer = "offer",
        File = "file",
        Pages = "pages",
        State = "state",
        Done = "done",
        Cancel = "cancel",
        Flows = "flows",
    }

    /// What narrowed a guest's nested table (`mig_sync`'s `what`)
    MigSyncWhat {
        /// The dirty log begun: every page read-only
        Start = "start",
        /// A round: the pages written since the last read-only again
        Round = "round",
        /// The log over
        Stop = "stop",
    }

    /// What happened to the tunnel's keys with a host (`tunnel_key`'s `what`)
    TunnelKeyWhat {
        /// A key given it, to seal to this host with
        Given = "given",
        /// One taken from it
        Taken = "taken",
        /// Its keys dropped: a new instance of its hv's, or its going
        Dropped = "dropped",
        /// A key asked of it
        Asked = "asked",
        /// Its echoes stopped: the key to it no longer trusted
        Silent = "silent",
        /// The way to it does not carry the networks' MTU
        Narrow = "narrow",
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

    /// What an NBS request asks (`nbs_req`'s and the chunk server's `op`):
    /// nbswire's `Op`, in its order
    NbsOp {
        Fence = "fence",
        Write = "write",
        Read = "read",
        Flush = "flush",
        Close = "close",
        Held = "held",
        Ping = "ping",
        Holdings = "holdings",
        Pull = "pull",
        Sums = "sums",
        Scrub = "scrub",
        CopyUp = "copy_up",
    }

    /// What a chunk server's answer says (`nbs_ans`'s, `nbs_cs_done`'s
    /// `status`): done, or nbswire's `Refusal`, in its order
    NbsStatus {
        Ok = "ok",
        Fenced = "fenced",
        NotFenced = "not_fenced",
        StaleChunk = "stale_chunk",
        Epoch = "epoch",
        NoRoom = "no_room",
        Bad = "bad",
        Failed = "failed",
        Io = "io",
        TooManyFences = "too_many_fences",
        NotTheMap = "not_the_map",
        NotHere = "not_here",
        NotAttached = "not_attached",
        Behind = "behind",
        NoMds = "no_mds",
        NotReady = "not_ready",
        NotHeld = "not_held",
        NotTheJoiner = "not_the_joiner",
    }

    /// A chunk server's storage operation's stage (`nbs_cs_step`).
    NbsCsStepStage {
        ZeroStart = "zero_start",
        ZeroDone = "zero_done",
        PutStart = "put_start",
        PutDone = "put_done",
        WriteStart = "write_start",
        WriteDone = "write_done",
        FlushStart = "flush_start",
        FlushDone = "flush_done",
        CommitStart = "commit_start",
        CommitDone = "commit_done",
    }

    /// A turn ticket's stage (`nbs_turn`).
    NbsTurnStage {
        Wait = "wait",
        Acquire = "acquire",
        Release = "release",
    }

    /// Why a message came back to its sender (`nbs_bounce`'s `why`): ipc's
    /// `Bounce`
    BounceWhy {
        /// The mailbox it was for full
        Full = "full",
        /// Its owner gone, or no way to it
        Gone = "gone",
        /// Never sent by the channel
        Unsent = "unsent",
    }

    /// Which raft a member is of (`raft_*`'s `of`)
    RaftOf {
        /// The cluster's registry (cluster.ko)
        Registry = "registry",
        /// NBS's metadata (nbs-mds.ko)
        Nbs = "nbs",
        /// vnet's state: the cluster's networks for VMs (vnet.ko)
        Vnet = "vnet",
    }

    /// A raft member's role (`raft_role`'s `role`): raft's `Role`
    MemberRole {
        Follower = "follower",
        PreCandidate = "pre_candidate",
        Candidate = "candidate",
        Leader = "leader",
    }

    /// A raft message's kind (`raft_send`'s and `raft_recv`'s `kind`):
    /// raft's `Body`, a vote's and its answer's pre-vote apart
    RaftKind {
        Vote = "vote",
        PreVote = "pre_vote",
        VoteResp = "vote_resp",
        PreVoteResp = "pre_vote_resp",
        Append = "append",
        AppendResp = "append_resp",
        Heartbeat = "heartbeat",
        HeartbeatResp = "heartbeat_resp",
        Snapshot = "snapshot",
        SnapshotResp = "snapshot_resp",
        TimeoutNow = "timeout_now",
        ReadIndex = "read_index",
        ReadIndexResp = "read_index_resp",
    }

    /// Why ipcnet bounced a message (`ipcnet_bounce`'s `why`)
    IpcnetWhy {
        /// Its class's queue past its bytes, or no room for its entry
        Full = "full",
        /// The peer not up, or up in another session than the message's
        Down = "down",
        /// Queued, and its session lost before it went
        Lost = "lost",
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

    /// `ext4_commit`: ext4's group of transactions committed to its journal
    Ext4Commit = 26, "ext4_commit" {
        seq: u32,
        blocks: u32,
        ns: u32,
        checkpoint_ns: u32,
        pid: u32,
        ok: u8,
        why: u8 as CommitWhy,
        /// The transactions staged in the group it committed
        txns: u16,
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

    /// `nbs_req`: a request a volume's client sent a chunk server
    NbsReq = 39, "nbs_req" {
        /// Its session's id: what the answer, the chunk server's records
        /// and ipcnet's `reference` name it by
        id: u64,
        volume: u32,
        /// The chunk's number; 0 for a request of no chunk
        chunk: u32,
        /// Where in the chunk
        offset: u32,
        /// A write's bytes, a read's asked
        len: u32,
        pid: u32,
        /// The chunk server, by its number in NBS's metadata
        cs: u16,
        op: u8 as NbsOp,
        _pad: u8,
    }

    /// `nbs_ans`: a chunk server's answer to it, at the client
    NbsAns = 40, "nbs_ans" {
        id: u64,
        bytes: u32,
        pid: u32,
        /// The host it came from, by its number in the channel; 0 this one
        from: u16,
        status: u8 as NbsStatus,
        _pad: [u8; 5],
    }

    /// `nbs_bounce`: a request that never reached its chunk server
    NbsBounce = 41, "nbs_bounce" {
        id: u64,
        pid: u32,
        why: u8 as BounceWhy,
        _pad: [u8; 3],
    }

    /// `nbs_cs_req`: a request a chunk server took
    NbsCsReq = 42, "nbs_cs_req" {
        id: u64,
        volume: u32,
        chunk: u32,
        offset: u32,
        len: u32,
        pid: u32,
        from: u16,
        op: u8 as NbsOp,
        _pad: u8,
    }

    /// `nbs_cs_done`: the chunk server's answer to it
    NbsCsDone = 43, "nbs_cs_done" {
        id: u64,
        /// From its taking to its answer
        ns: u32,
        pid: u32,
        from: u16,
        op: u8 as NbsOp,
        status: u8 as NbsStatus,
        _pad: [u8; 4],
    }

    /// `raft_role`: a raft member's role, term or leader changed
    RaftRole = 44, "raft_role" {
        term: u64,
        /// Its member's node id; 0 none
        leader: u64,
        pid: u32,
        of: u8 as RaftOf,
        role: u8 as MemberRole,
        _pad: [u8; 2],
    }

    /// `raft_send`: a raft message a member sent
    RaftSend = 45, "raft_send" {
        peer: u64,
        term: u64,
        /// What it is about: an append's previous index, a vote's last, a
        /// response's, a heartbeat's commit, a snapshot's
        index: u64,
        /// The entries it carries; a snapshot piece's bytes
        count: u32,
        pid: u32,
        of: u8 as RaftOf,
        kind: u8 as RaftKind,
        /// A response's: granted, or taken; 1 for any other message
        ok: u8,
        _pad: [u8; 5],
    }

    /// `raft_recv`: a raft message a member took in
    RaftRecv = 46, "raft_recv" {
        peer: u64,
        term: u64,
        index: u64,
        count: u32,
        pid: u32,
        of: u8 as RaftOf,
        kind: u8 as RaftKind,
        ok: u8,
        _pad: [u8; 5],
    }

    /// `raft_commit`: what a member has committed and applied, as either
    /// moved on
    RaftCommit = 47, "raft_commit" {
        committed: u64,
        applied: u64,
        pid: u32,
        of: u8 as RaftOf,
        _pad: [u8; 3],
    }

    /// `ipcnet_link`: a peer's session up, or lost
    IpcnetLink = 48, "ipcnet_link" {
        ups: u32,
        losses: u32,
        pid: u32,
        /// Its host number in the channel
        peer: u16,
        /// 1 up, 0 lost
        up: u8,
        _pad: u8,
    }

    /// `ipcnet_queue`: a message queued for a peer
    IpcnetQueue = 49, "ipcnet_queue" {
        /// The message's reference: an NBS request's id
        reference: u64,
        bytes: u32,
        /// Its class's queue's bytes, with it
        queued: u32,
        pid: u32,
        peer: u16,
        /// 1 the bulk class's queue, 0 the control one's
        bulk: u8,
        _pad: u8,
    }

    /// `ipcnet_bounce`: a message for a peer bounced back to its sender
    IpcnetBounce = 50, "ipcnet_bounce" {
        reference: u64,
        bytes: u32,
        queued: u32,
        pid: u32,
        peer: u16,
        bulk: u8,
        why: u8 as IpcnetWhy,
    }

    /// `fw_deny`: a packet to or from a VM its firewall kept out -- in the
    /// hv module's switch, or a client the forward's relay held to it
    FwDeny = 51, "fw_deny" {
        /// The other end's IPv4 address; 0 where the packet has none
        peer: u32,
        /// The VM's port and the other end's, where the protocol has them
        lport: u16,
        rport: u16,
        /// The switch's port the VM is on, and its firewall's place there
        port: u8,
        place: u8,
        dir: u8 as FwDir,
        why: u8 as FwWhy,
        /// The rule that kept it out, from 1; 0 where none did
        rule: u8,
        proto: u8,
        _pad: [u8; 2],
    }

    /// `tunnel_drop`: a datagram of vnet's tunnel not taken, or a packet
    /// not sent into it -- in the hv module's tunnel
    TunnelDrop = 52, "tunnel_drop" {
        /// The other host's underlay address, and its port below: as the
        /// table has them, or as the datagram came where no host's key
        /// opens it
        peer: u32,
        /// The network it was of, its key's number and its counter's low
        /// word: 0 for what it had not
        network: u32,
        key: u32,
        counter: u32,
        peer_port: u16,
        kind: u8 as TunnelKind,
        why: u8 as TunnelWhy,
    }

    /// `tunnel_key`: the tunnel's keys with another host
    TunnelKey = 53, "tunnel_key" {
        /// The other host's underlay address, and its port below
        peer: u32,
        /// The key's number where one is meant; 0 else
        number: u32,
        peer_port: u16,
        what: u8 as TunnelKeyWhat,
        _pad: u8,
    }

    /// `mig_round`: a round of a migration's pages sent by the old host --
    /// in the hv module's `migrate.rs`
    MigRound = 54, "mig_round" {
        /// The migration
        id: u64,
        round: u32,
        /// Its pages, and how many of them zeros: a mark and no data
        pages: u32,
        zeros: u32,
        /// How long it took to send, and the pages marked written by its end
        ms: u32,
        marked: u32,
        /// How slowed the guest was, percent; and 1 for the last round, the
        /// guest paused
        throttle: u8,
        last: u8,
        _pad: [u8; 2],
    }

    /// `mig_take`: a side's take of a volume in a migration's handover
    MigTake = 55, "mig_take" {
        id: u64,
        /// The writer's generation it was taken from
        generation: u64,
        /// How long NBS took to answer
        ms: u32,
        side: u8 as MigSide,
        took: u8 as MigTook,
        /// The guest's disk the volume is
        disk: u8,
        _pad: u8,
    }

    /// `mig_end`: a side's end of a migration
    MigEnd = 56, "mig_end" {
        id: u64,
        /// How long its guest was paused for it, its rounds, and the MiB of
        /// memory sent or taken
        paused_ms: u32,
        rounds: u32,
        mib: u32,
        side: u8 as MigSide,
        how: u8 as MigHow,
        _pad: [u8; 2],
    }

    /// `mig_recv`: a message of a migration taken by the new host
    MigRecv = 57, "mig_recv" {
        id: u64,
        /// Its sequence number's low word
        seq: u32,
        bytes: u32,
        /// How long it took to handle: written, answered
        ns: u32,
        kind: u8 as MigKind,
        _pad: [u8; 3],
    }

    /// `mig_sync`: a guest's nested table narrowed by a migration's dirty
    /// log, and the wait for its CPUs to drop what their TLBs kept of it --
    /// in hv's run loop
    MigSync = 58, "mig_sync" {
        /// The table's new identity
        table: u64,
        /// How long the narrowing took, and the wait after it
        narrow_ns: u32,
        wait_ns: u32,
        /// The pages narrowed
        pages: u32,
        cpus: u16,
        what: u8 as MigSyncWhat,
        _pad: u8,
    }

    /// `nbs_cs_step`: a chunk server's storage operation's stage
    NbsCsStep = 59, "nbs_cs_step" {
        volume: u32,
        chunk: u32,
        offset: u32,
        len: u32,
        ns: u32,
        pid: u32,
        stage: u8 as NbsCsStepStage,
        _pad: [u8; 3],
    }

    /// `nbs_turn`: a turn ticket's wait, acquisition and release
    NbsTurn = 60, "nbs_turn" {
        id: u64,
        key: u32,
        ns: u32,
        pid: u32,
        alone: u8,
        stage: u8 as NbsTurnStage,
        _pad: [u8; 2],
    }
}
