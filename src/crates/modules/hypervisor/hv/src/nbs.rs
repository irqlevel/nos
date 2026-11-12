//! NBS as hv asks it (N10d3): through the management bus, as the host API
//! and the panel ask it -- nbs-mds is a module of its own, and a module
//! reaches another's only so. Its `nbs` provider's `volume`, and its
//! `volume_create`, `volume_resize`, `volume_freeze`, `volume_clone` and
//! `volume_delete`: a refusal said back as the provider says it.

use alloc::string::String;
use alloc::vec::Vec;

use kcore::mgmt::{Status, Writer};
use mgmtwire::Value;

/// The most of an answer taken: a volume's, with the hosts of its replicas.
const ANSWER_MAX: usize = 16 * 1024;
const MIB: u64 = 1024 * 1024;

/// What a volume is, as the provider says it.
pub struct Volume {
    pub size: u64,
    /// `volume`, `image` or `snapshot`.
    pub kind: String,
    /// Its own host: the one it was made on, or for.
    pub host: String,
}

/// The provider's `method` called with the parameters `params` writes: its
/// answer, or why not -- its message where it gives one.
fn call(method: &str, params: &dyn Fn(&mut Writer)) -> Result<Value, (Status, String)> {
    let mut text = Vec::new();
    let written = {
        let mut sink = |b: &[u8]| {
            text.extend_from_slice(b);
            true
        };
        let mut w = Writer::new(&mut sink);
        params(&mut w);
        w.ok()
    };
    if !written {
        return Err((Status::Busy, String::from("out of memory")));
    }
    let mut answer = Vec::new();
    let status = kcore::mgmt::call("nbs", method, &text, ANSWER_MAX, &mut |b: &[u8]| {
        answer.extend_from_slice(b);
        true
    });
    let value = mgmtwire::json::parse(&answer, &mgmtwire::Limits::PARAMS).ok();
    match status {
        Status::Ok => value.ok_or_else(|| (Status::Failed, alloc::format!("nbs {}: its answer is no JSON", method))),
        Status::NoProvider => Err((status, String::from("no NBS on this host -- `insmod /nbs-mds.ko` first"))),
        s => {
            let said = value.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("message"))
                .and_then(|m| m.as_str()).map(String::from);
            Err((s, said.unwrap_or_else(|| alloc::format!("nbs {}: {:?}", method, s))))
        }
    }
}

/// A volume's number, as the provider answers a change.
fn number(v: Value) -> Result<u32, String> {
    v.get("volume").and_then(|n| n.as_u64()).and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| String::from("nbs: an answer with no volume's number"))
}

/// Why not, said of volume `name`: what the provider says of a name, or
/// of what is done to one, does not say whose.
fn of(name: &str, why: String) -> String {
    alloc::format!("nbs:{}: {}", name, why)
}

/// Volume `name`, where there is one.
pub fn volume(name: &str) -> Result<Option<Volume>, String> {
    match call("volume", &|w| {
        w.begin_object();
        w.field_str("name", name);
        w.end_object();
    }) {
        Ok(v) => {
            let size = v.get("size").and_then(|n| n.as_u64()).unwrap_or(0);
            let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("volume");
            let host = v.get("host").and_then(|h| h.as_str()).unwrap_or("");
            Ok(Some(Volume { size, kind: String::from(kind), host: String::from(host) }))
        }
        Err((Status::NotFound, _)) => Ok(None),
        Err((_, why)) => Err(of(name, why)),
    }
}

/// Volume `name` made of `bytes` -- whole MiB, rounded up -- its own host
/// this one: its number.
pub fn create(name: &str, bytes: u64) -> Result<u32, String> {
    let size_mib = bytes.div_ceil(MIB);
    number(call("volume_create", &|w| {
        w.begin_object();
        w.field_str("name", name);
        w.field_uint("size_mib", size_mib);
        w.end_object();
    }).map_err(|(_, why)| of(name, why))?)
}

/// Volume `name` grown to `bytes`, whole MiB rounded up.
pub fn resize(name: &str, bytes: u64) -> Result<u32, String> {
    let size_mib = bytes.div_ceil(MIB);
    number(call("volume_resize", &|w| {
        w.begin_object();
        w.field_str("name", name);
        w.field_uint("size_mib", size_mib);
        w.end_object();
    }).map_err(|(_, why)| of(name, why))?)
}

/// Volume `name` made an image: read-only for good.
pub fn freeze(name: &str) -> Result<u32, String> {
    number(call("volume_freeze", &|w| {
        w.begin_object();
        w.field_str("name", name);
        w.end_object();
    }).map_err(|(_, why)| of(name, why))?)
}

/// Volume `name` cloned from image `from`, its own host this one.
pub fn clone(from: &str, name: &str) -> Result<u32, String> {
    number(call("volume_clone", &|w| {
        w.begin_object();
        w.field_str("from", from);
        w.field_str("name", name);
        w.end_object();
    }).map_err(|(_, why)| alloc::format!("nbs:{} of nbs:{}: {}", name, from, why))?)
}

/// Volume `name` deleted: what a make that failed leaves taken away.
pub fn delete(name: &str) -> Result<(), String> {
    call("volume_delete", &|w| {
        w.begin_object();
        w.field_str("name", name);
        w.end_object();
    }).map(|_| ()).map_err(|(_, why)| of(name, why))
}
