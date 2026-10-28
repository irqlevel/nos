//! The `block` provider on the management bus: every block device -- the
//! disks and the partitions the kernel found on them -- its size, the disk a
//! partition is on and where, and who holds it: a mount, the disk log, a
//! guest's disk. Read from the device table as `disks` reads it.

use crate::table;
use kcore::trace;
use mgmt::{Call, Kind, Method, Params, Provider, Status};

struct Block;

static BLOCK: Block = Block;

const DESCRIPTOR: &str = include_str!("block.descriptor.json");

static METHODS: [Method; 1] = [Method { name: "disks", kind: Kind::Read }];

/// The provider registered for good: from the layer's init.
pub fn init() {
    match mgmt::register("block", &METHODS, DESCRIPTOR, &BLOCK) {
        Ok(registration) => core::mem::forget(registration),
        Err(status) => trace!(0, "block: cannot register the provider: {}", status.name()),
    }
}

impl Provider for Block {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status {
        let value = match mgmt::parse_params(params) {
            Ok(v) => v,
            Err(s) => return call.refuse(s, "the parameters are not JSON as the bus reads it", None),
        };
        if let Err(e) = Params::new(&value).and_then(Params::finish) {
            return call.invalid(e);
        }
        match method {
            "disks" => disks(call),
            _ => Status::NoMethod,
        }
    }
}

fn disks(call: &mut Call) -> Status {
    call.answer(|w| {
        w.begin_object();
        w.key("disks");
        w.begin_list();
        for index in 0..table::count() {
            let handle = table::at(index);
            let Some(name) = table::name(handle) else { continue };
            let sectors = table::capacity(handle);
            let sector_bytes = table::sector_size(handle);
            w.begin_object();
            w.field_str("name", name);
            w.key("disk");
            match table::name(table::parent(handle)) {
                Some(parent) if table::parent(handle) != 0 => w.str(parent),
                _ => w.null(),
            }
            w.key("start_sector");
            match table::start(handle) {
                Some(start) => w.uint(start),
                None => w.null(),
            }
            w.field_uint("sectors", sectors);
            w.field_uint("sector_bytes", sector_bytes);
            w.field_uint("size_bytes", sectors.saturating_mul(sector_bytes));
            w.field_uint("partitions", u64::from(table::partitions(handle)));
            w.key("held_by");
            match table::holder(handle) {
                Some(holder) => w.str_lossy(holder.to_bytes()),
                None => w.null(),
            }
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}
