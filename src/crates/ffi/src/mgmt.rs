use core::ffi::c_void;

/* The management bus as a loadable module reaches it (src/crates/mgmt
   defines these): a module is linked on its own, so a C ABI is the only seam
   it and the bus can share. A provider registers its methods -- each a name
   and a kind -- and a descriptor of what they are, all copied; its handler
   is called with a call's id, which is what the answer goes back by, from
   the task the call runs on. Code inside the kernel image registers and
   calls through the crate itself. */

/// A method a provider has: its name, and its kind -- `MGMT_READ`,
/// `MGMT_ACTION` or `MGMT_JOB`.
#[repr(C)]
pub struct MgmtMethod {
    pub name: *const u8,
    pub name_len: usize,
    pub kind: u32,
}

/// Asks, and changes nothing.
pub const MGMT_READ: u32 = 0;
/// Changes something, and is done when it returns.
pub const MGMT_ACTION: u32 = 1;
/// Takes long: run as a job, on a task of the bus's own.
pub const MGMT_JOB: u32 = 2;

/// A provider's handler: the context it registered with, the method's name,
/// the parameters (JSON), and the call's id. Answers a status code: 0, or
/// mgmtwire's Status below it.
pub type MgmtHandler = unsafe extern "C" fn(
    ctx: *mut c_void,
    method: *const u8,
    method_len: usize,
    params: *const u8,
    params_len: usize,
    call: u64,
) -> i32;

unsafe extern "C" {
    /// Adds a provider: `count` methods, the descriptor's bytes and the
    /// handler with its ctx. Returns its handle for `kernel_mgmt_unregister`,
    /// or 0 when anything is refused -- the name taken or not a name, a
    /// method's name twice or not a name, too many methods, a descriptor too
    /// long, the bus full or stopped.
    pub fn kernel_mgmt_register(
        name: *const u8,
        name_len: usize,
        methods: *const MgmtMethod,
        count: usize,
        descriptor: *const u8,
        descriptor_len: usize,
        handler: MgmtHandler,
        ctx: *mut c_void,
    ) -> u64;

    /// Removes it: no call starts from here on, its jobs are told to stop,
    /// and it returns once none of its calls and jobs runs any more -- it
    /// sleeps until then, so only from a task, and never from the
    /// provider's own handler. A handle that names nothing is nothing.
    pub safe fn kernel_mgmt_unregister(handle: u64);

    /// Adds `len` bytes to the call's answer: false once the answer would be
    /// longer than its caller takes, or for a call that is not running on
    /// this task.
    pub fn kernel_mgmt_reply(call: u64, buf: *const u8, len: usize) -> bool;

    /// A job's progress: `done` of `total` (0: not known).
    pub safe fn kernel_mgmt_progress(call: u64, done: u64, total: u64);

    /// A line of a job's log.
    pub fn kernel_mgmt_note(call: u64, line: *const u8, len: usize);

    /// Whether the job has been told to stop.
    pub safe fn kernel_mgmt_cancelled(call: u64) -> bool;
}

/* A module calling the bus -- the host API module's side: a call, what a
   method is, the providers, a descriptor and the jobs, each answered to a
   sink of the caller's, in pieces, for the length of the call. */

/// Where an answer goes: the ctx the caller passed, and a piece of it. False
/// when the caller takes no more.
pub type MgmtSink = unsafe extern "C" fn(ctx: *mut c_void, buf: *const u8, len: usize) -> bool;

/// Which kinds of method `kernel_mgmt_providers` lists.
pub const MGMT_KINDS_READ: u32 = 1;
pub const MGMT_KINDS_ACTION: u32 = 2;
pub const MGMT_KINDS_JOB: u32 = 4;
pub const MGMT_KINDS_ALL: u32 = 7;

unsafe extern "C" {
    /// `provider`'s `method` called with `params`, its answer, at most
    /// `max_reply` bytes, to `sink`: the status code it ended with. Sleeps
    /// for as long as the method takes, so from a task alone.
    pub fn kernel_mgmt_call(
        provider: *const u8,
        provider_len: usize,
        method: *const u8,
        method_len: usize,
        params: *const u8,
        params_len: usize,
        max_reply: usize,
        sink: MgmtSink,
        ctx: *mut c_void,
    ) -> i32;

    /// The kind `provider`'s `method` is registered as -- `MGMT_READ` and
    /// the rest -- or a status code below 0.
    pub fn kernel_mgmt_kind(provider: *const u8, provider_len: usize, method: *const u8, method_len: usize) -> i32;

    /// The providers with a method of `kinds`, as JSON, to `sink`.
    pub fn kernel_mgmt_providers(kinds: u32, sink: MgmtSink, ctx: *mut c_void) -> i32;

    /// `provider`'s descriptor to `sink`.
    pub fn kernel_mgmt_describe(provider: *const u8, provider_len: usize, sink: MgmtSink, ctx: *mut c_void) -> i32;

    /// A job started: its number above 0, or a status code below it.
    pub fn kernel_mgmt_job_start(
        provider: *const u8,
        provider_len: usize,
        method: *const u8,
        method_len: usize,
        params: *const u8,
        params_len: usize,
    ) -> i64;

    /// A job's record, its log from line `notes_since` and its result, as
    /// JSON, to `sink`.
    pub fn kernel_mgmt_job(id: u64, notes_since: u64, sink: MgmtSink, ctx: *mut c_void) -> i32;

    /// Every job's record, as JSON, to `sink`.
    pub fn kernel_mgmt_jobs(sink: MgmtSink, ctx: *mut c_void) -> i32;

    /// A job told to stop: 0, or a status code below it.
    pub safe fn kernel_mgmt_job_cancel(id: u64) -> i32;
}
