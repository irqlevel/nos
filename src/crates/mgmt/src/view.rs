//! The bus as JSON, for what hands it on as it is -- the host API module's
//! answers: the providers and their methods, a job's record, every job's.
//! Written with mgmtwire's Writer, so a client reads it by the rules of
//! the rest: whole numbers, a count past 2^53 as its digits.

use crate::{job_info, job_list, job_notes, job_result, providers, JobInfo, JobState, Kind, Status, Writer};
use alloc::string::String;
use alloc::vec::Vec;
use mgmtwire::{json, Limits};

/// Which kinds of method a listing shows: a reader's sees the reads alone.
pub const KIND_READ: u32 = 1;
pub const KIND_ACTION: u32 = 2;
pub const KIND_JOB: u32 = 4;

fn bit(kind: Kind) -> u32 {
    match kind {
        Kind::Read => KIND_READ,
        Kind::Action => KIND_ACTION,
        Kind::Job => KIND_JOB,
    }
}

/// What a job's result is read as, before it is written as the JSON it is.
const RESULT_LIMITS: Limits =
    Limits { bytes: crate::job::RESULT_MAX, depth: 16, values: 4096, members: 256, string: crate::job::RESULT_MAX };

/// `{"providers": [{"name", "module", "descriptor_bytes", "methods": [{"name",
/// "kind"}]}]}`, the methods of the kinds in `kinds` alone -- and a provider
/// with none of them left out.
pub fn write_providers(w: &mut Writer, kinds: u32) {
    let all = providers();
    w.begin_object();
    w.key("providers");
    w.begin_list();
    for p in all.iter().filter(|p| p.methods.iter().any(|(_, k)| kinds & bit(*k) != 0)) {
        w.begin_object();
        w.field_str("name", &p.name);
        w.field_bool("module", p.module);
        w.field_uint("descriptor_bytes", p.descriptor_len as u64);
        w.key("methods");
        w.begin_list();
        for (name, kind) in p.methods.iter().filter(|(_, k)| kinds & bit(*k) != 0) {
            w.begin_object();
            w.field_str("name", name);
            w.field_str("kind", kind.name());
            w.end_object();
        }
        w.end_list();
        w.end_object();
    }
    w.end_list();
    w.end_object();
}

fn write_info(w: &mut Writer, j: &JobInfo) {
    w.field_uint("id", j.id);
    w.field_str("provider", &j.provider);
    w.field_str("method", &j.method);
    w.field_str("state", j.state.name());
    w.key("status");
    if j.state == JobState::Done {
        w.str(j.status.name());
    } else {
        w.null();
    }
    w.field_uint("done", j.done);
    w.field_uint("total", j.total);
    w.field_uint("queued_ms", j.queued_ms);
    w.field_uint("started_ms", j.started_ms);
    w.field_uint("ended_ms", j.ended_ms);
    w.field_uint("notes_next", j.notes_next);
}

/// `{"jobs": [...]}`: every record the bus keeps, oldest first, without its
/// log or result.
pub fn write_jobs(w: &mut Writer) {
    let jobs = job_list();
    w.begin_object();
    w.key("jobs");
    w.begin_list();
    for j in &jobs {
        w.begin_object();
        write_info(w, j);
        w.end_object();
    }
    w.end_list();
    w.end_object();
}

/// The job `id`'s record, its log from line `notes_since` on, and its
/// result once it is done -- as the JSON it is, or as a string if it is
/// not JSON: NotFound, and nothing written, for no such job.
pub fn write_job(w: &mut Writer, id: u64, notes_since: u64) -> Status {
    let Some(info) = job_info(id) else { return Status::NotFound };
    let mut notes: Vec<(u64, String)> = Vec::new();
    let _ = job_notes(id, notes_since, &mut |seq, text| {
        if notes.try_reserve(1).is_ok() {
            if let Ok(t) = crate::copy(text) {
                notes.push((seq, t));
            }
        }
    });
    let mut result = Vec::new();
    let done = job_result(id, &mut |b| {
        result.try_reserve(b.len()).is_ok() && {
            result.extend_from_slice(b);
            true
        }
    })
    .is_some();
    w.begin_object();
    write_info(w, &info);
    w.key("notes");
    w.begin_list();
    for (seq, text) in &notes {
        w.begin_object();
        w.field_uint("seq", *seq);
        w.field_str("text", text);
        w.end_object();
    }
    w.end_list();
    w.key("result");
    if !done || result.is_empty() {
        w.null();
    } else if json::parse(&result, &RESULT_LIMITS).is_ok() {
        w.raw_value(&result);
    } else {
        w.str_lossy(&result);
    }
    w.end_object();
    Status::Ok
}
