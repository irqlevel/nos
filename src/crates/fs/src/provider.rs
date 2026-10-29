//! The `fs` provider on the management bus: every mount -- where, what
//! filesystem, on which device, read-only or not -- and how big each is and
//! how much of it is free, from what the filesystem counts in memory
//! (`FileSystem::space`). Copied under the VFS's lock a mount at a time, as
//! `mounts` reads it, and written with no lock held.

use crate::vfs_instance;
use block::Disk;
use kcore::trace;
use mgmt::{Call, Kind, Method, Params, Provider, Status};

struct Fs;

static FS: Fs = Fs;

const DESCRIPTOR: &str = include_str!("fs.descriptor.json");

static METHODS: [Method; 1] = [Method { name: "mounts", kind: Kind::Read }];

/// The provider registered for good: from the layer's init.
pub fn init() {
    match mgmt::register("fs", &METHODS, DESCRIPTOR, &FS) {
        Ok(registration) => core::mem::forget(registration),
        Err(status) => trace!(0, "fs: cannot register the provider: {}", status.name()),
    }
}

impl Provider for Fs {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status {
        let value = match mgmt::parse_params(params) {
            Ok(v) => v,
            Err(s) => return call.refuse(s, "the parameters are not JSON as the bus reads it", None),
        };
        if let Err(e) = Params::new(&value).and_then(Params::finish) {
            return call.invalid(e);
        }
        match method {
            "mounts" => mounts(call),
            _ => Status::NoMethod,
        }
    }
}

fn mounts(call: &mut Call) -> Status {
    let Some(vfs) = vfs_instance() else {
        return call.refuse(Status::Busy, "no filesystem layer: no memory for it", None);
    };
    /* All of them copied under the lock, before any is written */
    let mounts = vfs.mount_infos();
    call.answer(|w| {
        w.begin_object();
        w.key("mounts");
        w.begin_list();
        for m in mounts.iter().flatten() {
            w.begin_object();
            w.field_str_lossy("path", m.path());
            w.field_str("type", m.fs_name);
            w.key("device");
            match Disk::from_handle(m.device) {
                Some(disk) => w.str(disk.name()),
                None => w.null(),
            }
            w.field_bool("read_only", m.read_only);
            w.field_str_lossy("info", m.info());
            w.key("total_bytes");
            match m.space {
                Some(s) => w.uint(s.total_bytes),
                None => w.null(),
            }
            w.key("free_bytes");
            match m.space {
                Some(s) => w.uint(s.free_bytes),
                None => w.null(),
            }
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}
