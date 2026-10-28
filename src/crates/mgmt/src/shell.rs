//! `mgmt`: the bus from the shell -- and so from SSH, for a script, and from
//! the boot's own tests. What it prints is gathered first and printed with
//! no lock held: whoever runs it may be slow to take it.
//!
//!     mgmt                                  the providers, their methods and kinds
//!     mgmt describe <provider>              its descriptor
//!     mgmt call <provider> <method> [json]  a call, and its answer
//!     mgmt start <provider> <method> [json] a job started: its number
//!     mgmt jobs                             every job's record
//!     mgmt job <id>                         one's record, its log and its result
//!     mgmt cancel <id>                      one told to stop

use crate::{call, descriptor, job_cancel, job_info, job_list, job_notes, job_result, job_start, providers, JobInfo,
            MAX_REPLY};
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use kcore::cmd::{Command, Output};
use kcore::trace;

const HELP: &str = "mgmt [describe P | call P M [json] | start P M [json] | jobs | job ID | cancel ID]";

pub(crate) fn init() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    match Command::register("mgmt", HELP, run) {
        /* The bus is the kernel's own, and its command stays for good */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "mgmt: cannot register the mgmt command"),
    }
}

/// The first word of `args`, and the rest.
fn word(args: &str) -> (&str, &str) {
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    (&args[..end], args[end..].trim())
}

fn run(args: &str, out: &mut Output) {
    let (verb, rest) = word(args);
    match verb {
        "" => list(out),
        "describe" => match descriptor(rest) {
            Some(d) => {
                out.write_bytes(&d);
                let _ = writeln!(out);
            }
            None => {
                let _ = writeln!(out, "mgmt: no provider {}", rest);
            }
        },
        "call" | "start" => {
            let (provider, rest) = word(rest);
            let (method, params) = word(rest);
            if provider.is_empty() || method.is_empty() {
                let _ = writeln!(out, "usage: {}", HELP);
                return;
            }
            let params = if params.is_empty() { "null" } else { params };
            if verb == "call" {
                let mut answered = false;
                let status = call(provider, method, params.as_bytes(), MAX_REPLY, &mut |b| {
                    out.write_bytes(b);
                    answered = true;
                    true
                });
                if answered {
                    let _ = writeln!(out);
                }
                if status != crate::Status::Ok {
                    let _ = writeln!(out, "mgmt: {}", status.name());
                }
            } else {
                match job_start(provider, method, params.as_bytes()) {
                    Ok(id) => {
                        let _ = writeln!(out, "mgmt: job {} started", id);
                    }
                    Err(s) => {
                        let _ = writeln!(out, "mgmt: {}", s.name());
                    }
                }
            }
        }
        "jobs" => {
            let jobs = job_list();
            if jobs.is_empty() {
                let _ = writeln!(out, "mgmt: no jobs");
            }
            for j in &jobs {
                line(out, j);
            }
        }
        "job" | "cancel" => {
            let Ok(id) = rest.parse::<u64>() else {
                let _ = writeln!(out, "usage: {}", HELP);
                return;
            };
            if verb == "cancel" {
                let _ = writeln!(out, "mgmt: {}", if job_cancel(id) { "told to stop" } else { "no such job running" });
                return;
            }
            let Some(info) = job_info(id) else {
                let _ = writeln!(out, "mgmt: no job {}", id);
                return;
            };
            line(out, &info);
            let _ = job_notes(id, 0, &mut |seq, text| {
                let _ = writeln!(out, "  {:>4} {}", seq, text);
            });
            let mut answered = false;
            if job_result(id, &mut |b| {
                out.write_bytes(b);
                answered = true;
                true
            })
            .is_some()
                && answered
            {
                let _ = writeln!(out);
            }
        }
        _ => {
            let _ = writeln!(out, "usage: {}", HELP);
        }
    }
}

fn list(out: &mut Output) {
    let all = providers();
    if all.is_empty() {
        let _ = writeln!(out, "mgmt: no providers");
    }
    for p in &all {
        let _ = write!(out, "{}{}: {} methods, a descriptor of {} bytes, {} running:",
                       p.name, if p.module { " (a module's)" } else { "" }, p.methods.len(), p.descriptor_len, p.running);
        for (m, k) in &p.methods {
            let _ = write!(out, " {} ({})", m, k.name());
        }
        let _ = writeln!(out);
    }
}

fn line(out: &mut Output, j: &JobInfo) {
    let _ = write!(out, "job {} {}.{} {}", j.id, j.provider, j.method, j.state.name());
    if j.state == crate::JobState::Done {
        let _ = write!(out, " {}", j.status.name());
    }
    if j.total != 0 || j.done != 0 {
        let _ = write!(out, " {} of {}", j.done, j.total);
    }
    let _ = writeln!(out, ", queued at {} ms, started at {} ms, ended at {} ms", j.queued_ms, j.started_ms, j.ended_ms);
}
