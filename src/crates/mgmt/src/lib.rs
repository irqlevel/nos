//! The management bus: where a provider -- one of the kernel's layers, or a
//! module -- answers by name what it is asked, and whoever asks -- the host
//! API, the `mgmt` command -- finds it. Modules cannot call each other; the
//! bus is in the image, which they all reach.
//!
//! A provider registers its name, its methods -- each a name and a kind:
//! read, action or job -- and its descriptor (JSON for a client to read: the
//! bus keeps it and does not read it); everything is copied. A call is
//! looked up by name, counted in its provider's `running`, and made without
//! the bus's lock; its answer goes into a buffer of the call's own, bounded
//! by what the caller takes, and to the caller after. A job is a call made
//! on a task of the bus's (`job`), its progress, log and result kept after.
//!
//! What keeps it sound when a provider goes, as the shell's table of module
//! commands does (`Cmd::unregister_dynamic`): its unregister stops new calls
//! (`removing`), tells its jobs to stop, and returns only once its `running`
//! is 0 -- saying every 10 s whom it waits for -- so a module's unload, which
//! unregisters, never leaves a call in code that is gone. A handle and a
//! call's id carry a generation and are looked up, never followed; an
//! answer is taken only from the task its call runs on.

#![no_std]

extern crate alloc;

mod abi;
mod job;
mod selftest;
mod shell;

pub use job::{job_cancel, job_info, job_list, job_notes, job_result, job_start, JobInfo, JobState};
pub use mgmtwire::{Invalid, Params, Status, Value, Writer};
pub use selftest::selftest;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::marker::PhantomData;
use kcore::consts::{NS_PER_MS, NS_PER_SEC};
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::time::boot_time_ns;
use kcore::trace;

/// Providers at once.
pub const MAX_PROVIDERS: usize = 32;
/// Methods a provider has, at most.
pub const MAX_METHODS: usize = 48;
/// A descriptor's bytes, at most.
pub const MAX_DESCRIPTOR: usize = 16 * 1024;
/// Calls and jobs running at once, in all.
pub const MAX_CALLS: usize = 64;
/// An answer's bytes, at most, whatever its caller would take.
pub const MAX_REPLY: usize = 256 * 1024;

/// The low bits of a handle and of a call's id: its slot, from 1.
const INDEX_BITS: u32 = 8;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
/// How often an unregister looks again, and says whom it waits for.
const POLL_MS: u64 = 1;
const REPORT_NS: u64 = 10 * NS_PER_SEC;
/// A call that takes longer than this is said to: what takes long is a job.
const SLOW_CALL_NS: u64 = NS_PER_SEC;

const _: () = assert!(MAX_PROVIDERS < (1 << INDEX_BITS) && MAX_CALLS < (1 << INDEX_BITS));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Asks, and changes nothing.
    Read,
    /// Changes something, and is done when it returns.
    Action,
    /// Takes long: started as a job, on a task of the bus's own.
    Job,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Action => "action",
            Kind::Job => "job",
        }
    }
}

/// A method of a provider in the image.
pub struct Method {
    pub name: &'static str,
    pub kind: Kind,
}

/// A provider in the image: its methods' calls, answered through `call`.
pub trait Provider: Sync {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status;
}

/// The ctx a module registered with: handed back to its handler, never
/// read here.
#[derive(Clone, Copy)]
pub(crate) struct Opaque(pub(crate) *mut c_void);

/* Only ever passed back to the module that gave it, on whichever task a call
 * runs: what it points at is the module's to keep sound across tasks, as a
 * shell command's ctx is (kernel_cmd_register's). */
unsafe impl Send for Opaque {}
unsafe impl Sync for Opaque {}

#[derive(Clone, Copy)]
pub(crate) enum Handler {
    Image(&'static dyn Provider),
    Module { f: ffi::mgmt::MgmtHandler, ctx: Opaque },
}

struct Slot {
    handle: u64,
    name: String,
    methods: Vec<(String, Kind)>,
    descriptor: Vec<u8>,
    handler: Handler,
    /// Its calls and jobs running now.
    running: u32,
    /// Unregistering: no call starts.
    removing: bool,
}

/// A call running: what an answer from a module is looked up by.
struct Active {
    id: u64,
    task: usize,
    slot: usize,
    method: usize,
    state: Arc<CallState>,
}

struct Registry {
    slots: Vec<Option<Slot>>,
    calls: Vec<Option<Active>>,
    generation: u64,
    /// The halt has begun: no call, job or provider starts.
    stopped: bool,
}

/// What a call's answer goes into, and the job it is, if it is one.
pub(crate) struct CallState {
    reply: Mutex<Reply>,
    job: Option<Arc<job::Job>>,
}

struct Reply {
    buf: Vec<u8>,
    max: usize,
    /// Past `max`, or no memory: the answer is not whole.
    over: bool,
}

impl CallState {
    pub(crate) fn new(max: usize, job: Option<Arc<job::Job>>) -> Option<Arc<CallState>> {
        let reply = Mutex::new(Reply { buf: Vec::new(), max: max.min(MAX_REPLY), over: false })?;
        Some(Arc::new(CallState { reply, job }))
    }

    pub(crate) fn append(&self, b: &[u8]) -> bool {
        let mut r = self.reply.lock();
        if r.over || r.buf.len().saturating_add(b.len()) > r.max || r.buf.try_reserve(b.len()).is_err() {
            r.over = true;
            return false;
        }
        r.buf.extend_from_slice(b);
        true
    }

    /// The answer, and whether it is whole.
    pub(crate) fn take(&self) -> (Vec<u8>, bool) {
        let mut r = self.reply.lock();
        (core::mem::take(&mut r.buf), !r.over)
    }

    pub(crate) fn job(&self) -> Option<&Arc<job::Job>> {
        self.job.as_ref()
    }
}

static BUS: OnceBox<Mutex<Registry>> = OnceBox::new();

fn bus() -> Option<&'static Mutex<Registry>> {
    BUS.get_or_try_init(|| {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_PROVIDERS).ok()?;
        slots.resize_with(MAX_PROVIDERS, || None);
        let mut calls = Vec::new();
        calls.try_reserve_exact(MAX_CALLS).ok()?;
        calls.resize_with(MAX_CALLS, || None);
        let registry = Mutex::new(Registry { slots, calls, generation: 0, stopped: false })?;
        Some(Box::new(registry))
    })
}

/// The bus made, and its command put in front of whoever runs one: from
/// rust_init. The bus itself is made by whatever needs it first -- a module
/// loaded by the boot's test of modules, before rust_init -- and works off a
/// task's stack too: a call, a registration and an unregister ask for the
/// task they run on with `current_id_or_none`, which is 0 there.
pub fn init() {
    if bus().is_none() || job::jobs().is_none() {
        trace!(0, "mgmt: no memory for the bus");
        return;
    }
    shell::init();
}

fn copy(s: &str) -> Result<String, Status> {
    let mut c = String::new();
    c.try_reserve_exact(s.len()).map_err(|_| Status::Busy)?;
    c.push_str(s);
    Ok(c)
}

fn slot_index(handle: u64) -> Option<usize> {
    let i = (handle & INDEX_MASK) as usize;
    if i == 0 || i > MAX_PROVIDERS { None } else { Some(i - 1) }
}

/// A provider of the image's, registered: dropped, it is unregistered --
/// which waits out its calls and jobs. One that stays for good is
/// `forget`-ten.
pub struct Registration {
    handle: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        unregister(self.handle);
    }
}

/// Registers a provider of the image's.
pub fn register(name: &str, methods: &[Method], descriptor: &'static str, provider: &'static dyn Provider)
    -> Result<Registration, Status> {
    let handle = add(name, methods.iter().map(|m| (m.name, m.kind)), methods.len(), descriptor.as_bytes(),
                     Handler::Image(provider))?;
    Ok(Registration { handle })
}

/// Adds a provider: its name, `count` methods, its descriptor -- all copied
/// first, with no lock held -- and its handler.
pub(crate) fn add<'a>(name: &str, methods: impl Iterator<Item = (&'a str, Kind)>, count: usize, descriptor: &[u8],
                      handler: Handler) -> Result<u64, Status> {
    if !mgmtwire::valid_provider(name) || count == 0 || count > MAX_METHODS || descriptor.len() > MAX_DESCRIPTOR {
        return Err(Status::Invalid);
    }
    let mut copies: Vec<(String, Kind)> = Vec::new();
    copies.try_reserve_exact(count).map_err(|_| Status::Busy)?;
    for (m, kind) in methods.take(count) {
        if !mgmtwire::valid_method(m) || copies.iter().any(|(c, _)| c == m) {
            return Err(Status::Invalid);
        }
        copies.push((copy(m)?, kind));
    }
    if copies.len() != count {
        return Err(Status::Invalid);
    }
    /* What a client builds its views from says what the provider has, or
     * the provider is not taken: checked here, before anything is
     * registered */
    let mut named: Vec<(&str, &str)> = Vec::new();
    named.try_reserve_exact(count).map_err(|_| Status::Busy)?;
    named.extend(copies.iter().map(|(m, k)| (m.as_str(), k.name())));
    if let Err(e) = mgmtwire::descriptor::check(descriptor, name, &named) {
        trace!(0, "mgmt: provider {}'s descriptor refused: {} {}", name, e.at, e.why);
        return Err(Status::Invalid);
    }
    drop(named);
    let name = copy(name)?;
    let mut desc = Vec::new();
    desc.try_reserve_exact(descriptor.len()).map_err(|_| Status::Busy)?;
    desc.extend_from_slice(descriptor);

    let bus = bus().ok_or(Status::Busy)?;
    let mut r = bus.lock();
    if r.stopped {
        return Err(Status::Gone);
    }
    if r.slots.iter().flatten().any(|s| s.name == name) {
        return Err(Status::Conflict);
    }
    let index = r.slots.iter().position(Option::is_none).ok_or(Status::Busy)?;
    r.generation += 1;
    let handle = (r.generation << INDEX_BITS) | (index as u64 + 1);
    r.slots[index] = Some(Slot { handle, name, methods: copies, descriptor: desc, handler, running: 0, removing: false });
    Ok(handle)
}

/// Removes a provider: no call of it starts from here on, its jobs are told
/// to stop, and it returns once none of its calls and jobs runs. A handle
/// that names nothing -- gone already -- is nothing. From inside one of the
/// provider's own calls it would wait for itself: that is a bug, and a panic
/// says so rather than a hang.
pub(crate) fn unregister(handle: u64) {
    let (Some(bus), Some(index)) = (BUS.get(), slot_index(handle)) else { return };
    let me = kcore::task::current_id_or_none();
    {
        let mut r = bus.lock();
        let own_call = r.calls.iter().flatten().any(|c| c.slot == index && c.task == me);
        let Some(slot) = r.slots[index].as_mut().filter(|s| s.handle == handle) else { return };
        slot.removing = true;
        if own_call {
            panic!("mgmt: provider {} unregistered from inside a call of its own -- it would wait for itself", slot.name);
        }
    }
    job::cancel_provider(handle);

    let start = boot_time_ns();
    let mut said = start;
    loop {
        /* The slot, taken once nothing runs in it -- and dropped out of the
         * lock; or how many run, and what, for the report */
        let mut waiting = 0;
        let mut what: [(usize, usize); 4] = [(usize::MAX, 0); 4];
        let gone = {
            let mut r = bus.lock();
            match r.slots[index].as_ref() {
                Some(s) if s.handle == handle && s.running > 0 => {
                    waiting = s.running;
                    for (w, c) in what.iter_mut().zip(r.calls.iter().flatten().filter(|c| c.slot == index)) {
                        *w = (c.method, c.task);
                    }
                    None
                }
                Some(s) if s.handle == handle => r.slots[index].take(),
                /* A second unregister of it finished first */
                _ => return,
            }
        };
        if let Some(slot) = gone {
            drop(slot);
            return;
        }
        let now = boot_time_ns();
        if now.saturating_sub(said) >= REPORT_NS {
            said = now;
            describe_wait(bus, index, waiting, &what, now.saturating_sub(start));
        }
        kcore::task::sleep_ms(POLL_MS);
    }
}

/// Whom an unregister waits for, said: the provider, how long, and the
/// methods and tasks of the first of its calls.
fn describe_wait(bus: &Mutex<Registry>, index: usize, waiting: u32, what: &[(usize, usize); 4], ns: u64) {
    /* Names copied under the lock, said after it */
    let mut name = [0u8; 24];
    let mut methods = [[0u8; 32]; 4];
    let mut lens = [0usize; 5];
    {
        let r = bus.lock();
        let Some(s) = r.slots[index].as_ref() else { return };
        lens[4] = s.name.len().min(name.len());
        name[..lens[4]].copy_from_slice(&s.name.as_bytes()[..lens[4]]);
        for (i, (m, _)) in what.iter().enumerate() {
            if let Some((mname, _)) = s.methods.get(*m) {
                lens[i] = mname.len().min(32);
                methods[i][..lens[i]].copy_from_slice(&mname.as_bytes()[..lens[i]]);
            }
        }
    }
    fn text(b: &[u8]) -> &str {
        core::str::from_utf8(b).unwrap_or("?")
    }
    trace!(0, "mgmt: provider {}'s unregister has waited {} s for {} calls and jobs of it to return",
           text(&name[..lens[4]]), ns / NS_PER_SEC, waiting);
    for (i, (_, task)) in what.iter().enumerate().filter(|(_, (m, _))| *m != usize::MAX) {
        trace!(0, "mgmt:   {} on task {}", text(&methods[i][..lens[i]]), task);
    }
}

/// A call begun: counted in its provider's slot and on the list of calls
/// running, until this is dropped.
pub(crate) struct Begun {
    bus: &'static Mutex<Registry>,
    call: usize,
    slot: usize,
    pub(crate) id: u64,
}

impl Drop for Begun {
    fn drop(&mut self) {
        let mut r = self.bus.lock();
        r.calls[self.call] = None;
        /* The slot cannot have gone: its unregister waits for this */
        if let Some(s) = r.slots[self.slot].as_mut() {
            s.running -= 1;
        }
    }
}

/// `provider`'s `method` begun, if there is one of the kind asked for (a job
/// or not) -- of the registration `handle` names, when given.
pub(crate) fn begin(provider: &str, method: &str, state: &Arc<CallState>, job: bool, handle: Option<u64>)
    -> Result<(Handler, Begun), Status> {
    let bus = bus().ok_or(Status::Busy)?;
    let me = kcore::task::current_id_or_none();
    let mut r = bus.lock();
    if r.stopped {
        return Err(Status::Gone);
    }
    let index = r
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|s| s.name == provider && !s.removing && handle.is_none_or(|h| h == s.handle)))
        .ok_or(Status::NoProvider)?;
    let (handler, m) = {
        let Some(slot) = r.slots[index].as_ref() else { return Err(Status::NoProvider) };
        let m = slot.methods.iter().position(|(n, _)| n == method).ok_or(Status::NoMethod)?;
        if (slot.methods[m].1 == Kind::Job) != job {
            return Err(Status::Kind);
        }
        (slot.handler, m)
    };
    let call = r.calls.iter().position(Option::is_none).ok_or(Status::Busy)?;
    r.generation += 1;
    let id = (r.generation << INDEX_BITS) | (call as u64 + 1);
    r.calls[call] = Some(Active { id, task: me, slot: index, method: m, state: state.clone() });
    if let Some(s) = r.slots[index].as_mut() {
        s.running += 1;
    }
    Ok((handler, Begun { bus, call, slot: index, id }))
}

/// The call made: the handler run, with no lock held.
pub(crate) fn run(handler: Handler, begun: &Begun, method: &str, params: &[u8], state: &Arc<CallState>) -> Status {
    match handler {
        Handler::Image(p) => {
            let mut call = Call { state: state.clone(), _task: PhantomData };
            p.call(method, params, &mut call)
        }
        Handler::Module { f, ctx } => {
            /* The module's handler, with the ctx it registered: its code is
             * mapped while this call counts in its slot's `running`, which
             * its unregister -- and so its unload -- waits out; `method` and
             * `params` are valid for their lengths until it returns. */
            let code = unsafe { f(ctx.0, method.as_ptr(), method.len(), params.as_ptr(), params.len(), begun.id) };
            Status::from_code(code)
        }
    }
}

/// `provider`'s `method` called with `params` (JSON), its answer -- no
/// longer than `max_reply` -- handed to `sink`. What the call ended with:
/// TooLarge for an answer longer than that, or than `sink` takes.
pub fn call(provider: &str, method: &str, params: &[u8], max_reply: usize, sink: &mut dyn FnMut(&[u8]) -> bool)
    -> Status {
    let Some(state) = CallState::new(max_reply, None) else { return Status::Busy };
    let (handler, begun) = match begin(provider, method, &state, false, None) {
        Ok(b) => b,
        Err(s) => return s,
    };
    let start = boot_time_ns();
    let status = run(handler, &begun, method, params, &state);
    drop(begun);
    let took = boot_time_ns().saturating_sub(start);
    if took >= SLOW_CALL_NS {
        trace!(0, "mgmt: {}.{} took {} ms -- what takes long is a job", provider, method, took / NS_PER_MS);
    }
    let (reply, whole) = state.take();
    if !whole || (!reply.is_empty() && !sink(&reply)) {
        return Status::TooLarge;
    }
    status
}

/// The call being answered, as a provider of the image's has it.
pub struct Call {
    state: Arc<CallState>,
    /* Not Send: answered from the call's own task */
    _task: PhantomData<*const ()>,
}

impl Call {
    /// Adds `bytes` to the answer: false once it would be longer than the
    /// caller takes.
    pub fn reply(&mut self, bytes: &[u8]) -> bool {
        self.state.append(bytes)
    }

    /// The answer written as JSON by `write`; false if it was not all taken,
    /// or not whole.
    pub fn json(&mut self, write: impl FnOnce(&mut Writer)) -> bool {
        let state = &self.state;
        let mut sink = |b: &[u8]| state.append(b);
        let mut w = Writer::new(&mut sink);
        write(&mut w);
        w.ok()
    }

    pub fn progress(&mut self, done: u64, total: u64) {
        if let Some(j) = self.state.job() {
            j.progress(done, total);
        }
    }

    pub fn note(&mut self, line: &str) {
        if let Some(j) = self.state.job() {
            j.note(line);
        }
    }

    /// Whether the job has been told to stop.
    pub fn cancelled(&self) -> bool {
        self.state.job().is_some_and(|j| j.cancelled())
    }

    /// The answer written as JSON by `write`: Ok, or TooLarge if it was not
    /// all taken.
    pub fn answer(&mut self, write: impl FnOnce(&mut Writer)) -> Status {
        if self.json(write) { Status::Ok } else { Status::TooLarge }
    }

    /// The call refused: `{"error": ...}` answered, and `status` returned
    /// for the call to end with.
    pub fn refuse(&mut self, status: Status, message: &str, field: Option<&str>) -> Status {
        self.json(|w| status.write_error(w, message, field));
        status
    }

    /// A parameter refused, as `refuse` says it.
    pub fn invalid(&mut self, e: Invalid) -> Status {
        self.refuse(Status::Invalid, e.why, Some(e.field))
    }
}

/// Text made with `write!` into `N` bytes on the stack -- an address, a MAC
/// -- for an answer's `Writer`. What does not fit is cut, at a character.
pub struct Text<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    pub fn new() -> Self {
        Text { buf: [0; N], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        /* Only whole characters are ever put in */
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Write for Text<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut take = s.len().min(N - self.len);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// `args` as a `Text` of `N` bytes: `mgmt::text::<16>(format_args!(...))`.
pub fn text<const N: usize>(args: core::fmt::Arguments) -> Text<N> {
    let mut t = Text::new();
    let _ = core::fmt::Write::write_fmt(&mut t, args);
    t
}

/// A method's parameters read, as the bus takes them: none at all is none.
pub fn parse_params(params: &[u8]) -> Result<Value, Status> {
    if params.is_empty() {
        return Ok(Value::Null);
    }
    mgmtwire::json::parse(params, &mgmtwire::Limits::PARAMS).map_err(|_| Status::Invalid)
}

/// The state of the call `id`, if it runs on this task: what a module's
/// answer is looked up by.
pub(crate) fn active(id: u64) -> Option<Arc<CallState>> {
    let i = (id & INDEX_MASK) as usize;
    if i == 0 || i > MAX_CALLS {
        return None;
    }
    let me = kcore::task::current_id_or_none();
    let r = bus()?.lock();
    r.calls[i - 1].as_ref().filter(|c| c.id == id && c.task == me).map(|c| c.state.clone())
}

/// A provider as `mgmt` lists it.
pub struct ProviderInfo {
    pub name: String,
    pub methods: Vec<(String, Kind)>,
    pub descriptor_len: usize,
    pub running: u32,
    pub module: bool,
}

/// Every provider, copied out under the lock.
pub fn providers() -> Vec<ProviderInfo> {
    let mut out = Vec::new();
    let Some(bus) = bus() else { return out };
    let r = bus.lock();
    if out.try_reserve_exact(MAX_PROVIDERS).is_err() {
        return out;
    }
    for s in r.slots.iter().flatten() {
        let mut methods = Vec::new();
        if methods.try_reserve_exact(s.methods.len()).is_err() {
            break;
        }
        for (m, k) in &s.methods {
            let Ok(m) = copy(m) else { break };
            methods.push((m, *k));
        }
        let Ok(name) = copy(&s.name) else { break };
        out.push(ProviderInfo {
            name,
            methods,
            descriptor_len: s.descriptor.len(),
            running: s.running,
            module: matches!(s.handler, Handler::Module { .. }),
        });
    }
    out
}

/// A provider's descriptor, copied.
pub fn descriptor(name: &str) -> Option<Vec<u8>> {
    let r = bus()?.lock();
    let s = r.slots.iter().flatten().find(|s| s.name == name)?;
    let mut d = Vec::new();
    d.try_reserve_exact(s.descriptor.len()).ok()?;
    d.extend_from_slice(&s.descriptor);
    Some(d)
}

/// The registration of `provider`, if it has `method` and that is a job.
pub(crate) fn job_target(provider: &str, method: &str) -> Result<u64, Status> {
    let r = bus().ok_or(Status::Busy)?.lock();
    if r.stopped {
        return Err(Status::Gone);
    }
    let s = r.slots.iter().flatten().find(|s| s.name == provider && !s.removing).ok_or(Status::NoProvider)?;
    let (_, kind) = s.methods.iter().find(|(n, _)| n == method).ok_or(Status::NoMethod)?;
    if *kind != Kind::Job {
        return Err(Status::Kind);
    }
    Ok(s.handle)
}

/// The halt: no call, job or provider starts from here on; every job is
/// told to stop and waited for; and a module's provider still registered --
/// its module not unloaded, which the halt does first -- is said.
pub fn stop() {
    let Some(bus) = BUS.get() else { return };
    bus.lock().stopped = true;
    job::stop_all();
    for p in providers().iter().filter(|p| p.module) {
        trace!(0, "mgmt: provider {} of a module still registered at the halt", p.name);
    }
}
