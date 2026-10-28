//! The bus checked at every boot, by a provider of its own: calls answered
//! and refused as they should be, an answer held to its limit, a job run to
//! its end and one told to stop by its provider's unregister -- which waits
//! for it -- and the name free again after. A failure panics, as the boot's
//! other self-tests do.

use crate::{call, job_info, job_notes, job_result, job_start, register, Call, JobState, Kind, Method, Provider, Status};
use alloc::vec::Vec;
use kcore::trace;

struct Test;

/// How long the waiting job waits, at most, before it says it was never told
/// to stop.
const WAIT_LIMIT_MS: u64 = 10_000;

impl Provider for Test {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status {
        match method {
            "echo" => {
                call.reply(params);
                Status::Ok
            }
            "refuse" => {
                call.json(|w| Status::Conflict.write_error(w, "refused, as asked", Some("name")));
                Status::Conflict
            }
            "big" => {
                for _ in 0..64 {
                    if !call.reply(&[b'x'; 1024]) {
                        return Status::TooLarge;
                    }
                }
                Status::Ok
            }
            "quick" => {
                call.progress(1, 1);
                call.note("quick: done");
                call.reply(br#"{"done":true}"#);
                Status::Ok
            }
            "wait" => {
                call.note("wait: waiting to be told to stop");
                let mut ms = 0;
                while !call.cancelled() {
                    call.progress(ms, 0);
                    kcore::task::sleep_ms(1);
                    ms += 1;
                    if ms > WAIT_LIMIT_MS {
                        return Status::Failed;
                    }
                }
                Status::Cancelled
            }
            _ => Status::NoMethod,
        }
    }
}

static TEST: Test = Test;
static METHODS: [Method; 5] = [
    Method { name: "echo", kind: Kind::Read },
    Method { name: "refuse", kind: Kind::Action },
    Method { name: "big", kind: Kind::Read },
    Method { name: "quick", kind: Kind::Job },
    Method { name: "wait", kind: Kind::Job },
];

fn check(ok: bool, what: &str) {
    if !ok {
        panic!("mgmt selftest: {}", what);
    }
}

fn call_text(provider: &str, method: &str, params: &[u8], max: usize) -> (Status, Vec<u8>) {
    let mut got = Vec::new();
    let status = call(provider, method, params, max, &mut |b| {
        got.extend_from_slice(b);
        true
    });
    (status, got)
}

/// The job's record, once it is in `state`, waited for at most 10 s.
fn wait_for(id: u64, state: JobState) -> crate::JobInfo {
    for _ in 0..10_000 {
        if let Some(info) = job_info(id).filter(|i| i.state == state) {
            return info;
        }
        kcore::task::sleep_ms(1);
    }
    panic!("mgmt selftest: job {} never {}", id, state.name());
}

pub fn selftest() {
    crate::init();
    let reg = register("selftest", &METHODS, "{}", &TEST);
    check(reg.is_ok(), "the provider registered");
    check(matches!(register("selftest", &METHODS, "{}", &TEST), Err(Status::Conflict)), "a name twice refused");

    let (s, got) = call_text("selftest", "echo", br#"{"n":42}"#, 64);
    check(s == Status::Ok && got == br#"{"n":42}"#, "a call answered");
    check(call_text("selftest", "nope", b"", 64).0 == Status::NoMethod, "no such method");
    check(call_text("nobody", "echo", b"", 64).0 == Status::NoProvider, "no such provider");
    check(call_text("selftest", "quick", b"", 64).0 == Status::Kind, "a job called as a call");
    let (s, got) = call_text("selftest", "refuse", b"", 256);
    check(s == Status::Conflict && got.windows(10).any(|w| w == b"\"conflict\""), "a refusal and its reason");
    check(call_text("selftest", "big", b"", 10 * 1024).0 == Status::TooLarge, "an answer past its limit");
    check(matches!(job_start("selftest", "echo", b""), Err(Status::Kind)), "a call started as a job");

    let quick = job_start("selftest", "quick", b"");
    check(quick.is_ok(), "a job started");
    let quick = quick.unwrap_or(0);
    let info = wait_for(quick, JobState::Done);
    check(info.status == Status::Ok && info.done == 1 && info.total == 1, "a job done, its progress kept");
    let mut result = Vec::new();
    check(job_result(quick, &mut |b| {
        result.extend_from_slice(b);
        true
    }) == Some(Status::Ok) && result == br#"{"done":true}"#, "a job's result");
    let mut lines = 0;
    let next = job_notes(quick, 0, &mut |_, text| {
        lines += usize::from(text == "quick: done");
    });
    check(lines == 1 && next == Some(1), "a job's log");

    /* A job that runs until it is told to stop -- told by its provider's
     * unregister, which returns only once it has */
    let wait = job_start("selftest", "wait", b"");
    check(wait.is_ok(), "a waiting job started");
    let wait = wait.unwrap_or(0);
    wait_for(wait, JobState::Running);
    drop(reg);
    let info = job_info(wait);
    check(info.is_some_and(|i| i.state == JobState::Done && i.status == Status::Cancelled),
          "the unregister told the job to stop and waited for it");
    check(call_text("selftest", "echo", b"", 64).0 == Status::NoProvider, "no call of a provider gone");
    let again = register("selftest", &METHODS, "{}", &TEST);
    check(again.is_ok(), "the name free again");
    drop(again);
    trace!(0, "mgmt: selftest passed");
}
