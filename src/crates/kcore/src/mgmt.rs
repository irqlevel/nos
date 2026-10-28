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
