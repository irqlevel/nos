//! The management bus as a loadable module reaches it: a provider
//! registered with its methods and its descriptor, answering each call made
//! of it. Inside the kernel image the bus is the `mgmt` crate itself.
//!
//! A `Provider` is the registration: dropped, it is unregistered -- no call
//! starts after, its jobs are told to stop, and the drop returns once none
//! of its calls and jobs runs any more -- and only then is its handler
//! freed. So a module's unload, which drops it, waits them out, and no call
//! is ever made into a module that is gone. A `Call` is what a handler
//! answers through, on the task the call runs on: it is not `Send`, so an
//! answer cannot go back from anywhere else.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::marker::PhantomData;
use ffi::mgmt;

pub use mgmtwire::{Status, Writer};

/// What a method does: asks (changes nothing); changes something and is
/// done when it returns; or takes long, and runs as a job on a task of the
/// bus's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Action,
    Job,
}

pub struct Method {
    pub name: &'static str,
    pub kind: Kind,
}

/// The most methods a provider has.
pub const MAX_METHODS: usize = 48;

type Handler = dyn Fn(&str, &[u8], &mut Call) -> Status + Send + Sync;

pub struct Provider {
    handle: u64,
    handler: *mut Box<Handler>,
}

/* The handler behind the pointer is Send + Sync, and only Drop frees it --
 * after the unregister that waits out every call of it. */
unsafe impl Send for Provider {}
unsafe impl Sync for Provider {}

impl Provider {
    /// Registers `name`, its methods and its descriptor (copied), each call
    /// of them answered by `handler`: the method's name, its parameters
    /// (JSON), and the call to answer through. Refused (`Invalid`) for a
    /// name taken or not a name, a method's name twice or not one, more than
    /// MAX_METHODS, a descriptor too long, or a bus that is full or stopped.
    pub fn register<F>(name: &str, methods: &[Method], descriptor: &str, handler: F) -> Result<Provider, Status>
    where
        F: Fn(&str, &[u8], &mut Call) -> Status + Send + Sync + 'static,
    {
        if methods.is_empty() || methods.len() > MAX_METHODS {
            return Err(Status::Invalid);
        }
        let mut table = Vec::new();
        table.try_reserve_exact(methods.len()).map_err(|_| Status::Busy)?;
        table.extend(methods.iter().map(|m| mgmt::MgmtMethod {
            name: m.name.as_ptr(),
            name_len: m.name.len(),
            kind: match m.kind {
                Kind::Read => mgmt::MGMT_READ,
                Kind::Action => mgmt::MGMT_ACTION,
                Kind::Job => mgmt::MGMT_JOB,
            },
        }));
        let raw: *mut Box<Handler> = Box::into_raw(Box::new(Box::new(handler)));
        /* Every pointer is to bytes that outlive the call, which copies what
         * it keeps; the handler's box lives until Drop, after unregister. */
        let handle = unsafe {
            mgmt::kernel_mgmt_register(
                name.as_ptr(),
                name.len(),
                table.as_ptr(),
                table.len(),
                descriptor.as_ptr(),
                descriptor.len(),
                trampoline,
                raw as *mut c_void,
            )
        };
        if handle == 0 {
            drop(unsafe { Box::from_raw(raw) });
            return Err(Status::Invalid);
        }
        Ok(Provider { handle, handler: raw })
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        /* Returns once no call of it runs: then the handler is nobody's. */
        mgmt::kernel_mgmt_unregister(self.handle);
        drop(unsafe { Box::from_raw(self.handler) });
    }
}

/// # Safety
/// The bus calls it with the `ctx` it was registered with -- a live
/// `Box<Handler>`, which Drop frees only after the unregister that waits out
/// every call -- and `method` and `params` valid for their lengths while it
/// runs.
unsafe extern "C" fn trampoline(
    ctx: *mut c_void,
    method: *const u8,
    method_len: usize,
    params: *const u8,
    params_len: usize,
    call: u64,
) -> i32 {
    let handler = unsafe { &*(ctx as *const Box<Handler>) };
    let method = if method_len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(method, method_len) } };
    let Ok(method) = core::str::from_utf8(method) else { return Status::Invalid.code() };
    let params = if params_len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(params, params_len) } };
    let mut c = Call { id: call, _task: PhantomData };
    handler(method, params, &mut c).code()
}

/// A call being answered: on the task it runs on, and nowhere else.
pub struct Call {
    id: u64,
    /* Not Send: the bus takes an answer only from the call's own task. */
    _task: PhantomData<*const ()>,
}

impl Call {
    /// Adds `bytes` to the answer: false once it would be longer than the
    /// caller takes.
    pub fn reply(&mut self, bytes: &[u8]) -> bool {
        /* The bytes are the caller's, for the length given */
        unsafe { mgmt::kernel_mgmt_reply(self.id, bytes.as_ptr(), bytes.len()) }
    }

    /// The answer written as JSON by `write`; false if it was not all taken,
    /// or was not whole JSON.
    pub fn json(&mut self, write: impl FnOnce(&mut Writer)) -> bool {
        let id = self.id;
        let mut sink = |b: &[u8]| unsafe { mgmt::kernel_mgmt_reply(id, b.as_ptr(), b.len()) };
        let mut w = Writer::new(&mut sink);
        write(&mut w);
        w.ok()
    }

    /// A job's progress: `done` of `total` (0: not known).
    pub fn progress(&mut self, done: u64, total: u64) {
        mgmt::kernel_mgmt_progress(self.id, done, total);
    }

    /// A line of a job's log.
    pub fn note(&mut self, line: &str) {
        unsafe { mgmt::kernel_mgmt_note(self.id, line.as_ptr(), line.len()) };
    }

    /// Whether the job has been told to stop: its provider going, a cancel,
    /// the halt.
    pub fn cancelled(&self) -> bool {
        mgmt::kernel_mgmt_cancelled(self.id)
    }
}

/* ---- the bus asked: what the host API module calls ---- */

/// Which kinds of method `providers` lists.
pub const KINDS_READ: u32 = mgmt::MGMT_KINDS_READ;
pub const KINDS_ALL: u32 = mgmt::MGMT_KINDS_ALL;

type Sink<'a> = &'a mut dyn FnMut(&[u8]) -> bool;

/// # Safety
/// `ctx` is the `&mut Sink` one of the calls below passed, alive until it
/// returns, which is the bus's to keep to; `buf` is `len` bytes to read.
unsafe extern "C" fn sink_trampoline(ctx: *mut c_void, buf: *const u8, len: usize) -> bool {
    let sink = unsafe { &mut *(ctx as *mut Sink) };
    if len == 0 {
        return true;
    }
    sink(unsafe { core::slice::from_raw_parts(buf, len) })
}

/// `provider`'s `method` called with `params` (JSON), its answer -- at most
/// `max_reply` bytes -- handed to `sink`: what the call ended with. Sleeps
/// for as long as the method takes.
pub fn call(provider: &str, method: &str, params: &[u8], max_reply: usize, sink: &mut dyn FnMut(&[u8]) -> bool)
            -> Status {
    let mut sink: Sink = sink;
    let ctx = &mut sink as *mut Sink as *mut c_void;
    /* The strings and params are valid for their lengths, `ctx` until this
     * returns -- which is as long as the bus may call it */
    let code = unsafe {
        mgmt::kernel_mgmt_call(provider.as_ptr(), provider.len(), method.as_ptr(), method.len(), params.as_ptr(),
                               params.len(), max_reply, sink_trampoline, ctx)
    };
    Status::from_code(code)
}

/// What `provider`'s `method` is registered as.
pub fn kind(provider: &str, method: &str) -> Result<Kind, Status> {
    /* The strings are valid for their lengths */
    let code = unsafe { mgmt::kernel_mgmt_kind(provider.as_ptr(), provider.len(), method.as_ptr(), method.len()) };
    match code {
        c if c == mgmt::MGMT_READ as i32 => Ok(Kind::Read),
        c if c == mgmt::MGMT_ACTION as i32 => Ok(Kind::Action),
        c if c == mgmt::MGMT_JOB as i32 => Ok(Kind::Job),
        c => Err(Status::from_code(c)),
    }
}

/// Runs one of the bus's JSON answers into `sink`.
fn answered(sink: &mut dyn FnMut(&[u8]) -> bool, f: impl FnOnce(mgmt::MgmtSink, *mut c_void) -> i32) -> Status {
    let mut sink: Sink = sink;
    let ctx = &mut sink as *mut Sink as *mut c_void;
    Status::from_code(f(sink_trampoline, ctx))
}

/// The providers with a method of `kinds` (`KINDS_READ`, `KINDS_ALL`), as
/// JSON, to `sink`.
pub fn providers(kinds: u32, sink: &mut dyn FnMut(&[u8]) -> bool) -> Status {
    /* `ctx` is the sink, alive until the call returns */
    answered(sink, |s, ctx| unsafe { mgmt::kernel_mgmt_providers(kinds, s, ctx) })
}

/// `provider`'s descriptor, to `sink`.
pub fn describe(provider: &str, sink: &mut dyn FnMut(&[u8]) -> bool) -> Status {
    /* As providers'; the name is valid for its length */
    answered(sink, |s, ctx| unsafe { mgmt::kernel_mgmt_describe(provider.as_ptr(), provider.len(), s, ctx) })
}

/// A job of `provider`'s `method` started with `params`: its number.
pub fn job_start(provider: &str, method: &str, params: &[u8]) -> Result<u64, Status> {
    /* The strings and params are valid for their lengths */
    let id = unsafe {
        mgmt::kernel_mgmt_job_start(provider.as_ptr(), provider.len(), method.as_ptr(), method.len(), params.as_ptr(),
                                    params.len())
    };
    match u64::try_from(id) {
        Ok(id) if id != 0 => Ok(id),
        _ => Err(Status::from_code(i32::try_from(id).unwrap_or(Status::Failed.code()))),
    }
}

/// The job `id`'s record, its log from line `notes_since` and its result,
/// as JSON, to `sink`.
pub fn job(id: u64, notes_since: u64, sink: &mut dyn FnMut(&[u8]) -> bool) -> Status {
    /* As providers' */
    answered(sink, |s, ctx| unsafe { mgmt::kernel_mgmt_job(id, notes_since, s, ctx) })
}

/// Every job's record, as JSON, to `sink`.
pub fn jobs(sink: &mut dyn FnMut(&[u8]) -> bool) -> Status {
    /* As providers' */
    answered(sink, |s, ctx| unsafe { mgmt::kernel_mgmt_jobs(s, ctx) })
}

/// The job `id` told to stop: NotFound for none there is, or one done.
pub fn job_cancel(id: u64) -> Status {
    Status::from_code(mgmt::kernel_mgmt_job_cancel(id))
}
