//! What a call ends with: a code the C ABI carries (0, or below it) and a
//! name a client reads -- the API module puts the name in its error replies
//! and chooses the HTTP status by it.

use crate::writer::Writer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    /// No provider by that name is registered.
    NoProvider = -1,
    /// The provider has no method by that name.
    NoMethod = -2,
    /// The parameters were refused.
    Invalid = -3,
    /// Too much at once: try again later.
    Busy = -4,
    /// The method ran and failed; its message says why.
    Failed = -5,
    /// A job cancelled before it was done.
    Cancelled = -6,
    /// The reply, or what was asked, was larger than it may be.
    TooLarge = -7,
    /// The provider went while the call was made.
    Gone = -8,
    /// The method is not of the kind asked for: a job called as a call, or
    /// the other way round.
    Kind = -9,
    /// Not on this machine: no hypervisor on it, say.
    Unsupported = -10,
    /// Refused for the state of what it is about: a VM running that would
    /// have to be stopped, a name taken.
    Conflict = -11,
    /// What it is about does not exist.
    NotFound = -12,
    /// The caller may not.
    Forbidden = -13,
}

const ALL: [Status; 14] = [
    Status::Ok,
    Status::NoProvider,
    Status::NoMethod,
    Status::Invalid,
    Status::Busy,
    Status::Failed,
    Status::Cancelled,
    Status::TooLarge,
    Status::Gone,
    Status::Kind,
    Status::Unsupported,
    Status::Conflict,
    Status::NotFound,
    Status::Forbidden,
];

impl Status {
    pub fn code(self) -> i32 {
        self as i32
    }

    /// The status a code is; Failed for one no status has.
    pub fn from_code(code: i32) -> Status {
        ALL.iter().copied().find(|s| s.code() == code).unwrap_or(Status::Failed)
    }

    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NoProvider => "no_provider",
            Status::NoMethod => "no_method",
            Status::Invalid => "invalid",
            Status::Busy => "busy",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::TooLarge => "too_large",
            Status::Gone => "gone",
            Status::Kind => "kind",
            Status::Unsupported => "unsupported",
            Status::Conflict => "conflict",
            Status::NotFound => "not_found",
            Status::Forbidden => "forbidden",
        }
    }

    /// `{"error": {"code": ..., "message": ..., "field": ...}}`: what a
    /// failed call answers.
    pub fn write_error(self, w: &mut Writer, message: &str, field: Option<&str>) {
        w.begin_object();
        w.key("error");
        w.begin_object();
        w.field_str("code", self.name());
        w.field_str("message", message);
        if let Some(f) = field {
            w.field_str("field", f);
        }
        w.end_object();
        w.end_object();
    }
}
