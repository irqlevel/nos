//! What the boot self-test checks of the block layer: the table.
//!
//! The devices themselves are whatever booted, so there is nothing here to
//! compare against a known answer. What can be checked is that the table is
//! consistent with itself from the outside: a name nothing carries finds
//! nothing, and every device the table counts answers to the name it is
//! registered under with the handle the walk gave.
//!
//! The partition readers are judged by `scripts/parttest.py`, which boots
//! with tables it writes itself; a self-test cannot, because a boot has only
//! the disks it was given.

use kcore::block::{self, Disk};
use kcore::trace;

fn check(what: &str, ok: bool) -> bool {
    if !ok {
        trace!(0, "block selftest: {} FAILED", what);
    }
    ok
}

fn lookups() -> bool {
    let mut ok = true;

    ok &= check("a name no device carries finds nothing", Disk::open("nonexistent").is_none());
    ok &= check("and neither does no name at all", Disk::open("").is_none());

    /* The table only grows, so a walk of it is stable: count is a lower
     * bound on what is there and every index below it stays valid. */
    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            /* A slot a registration has not finished; the caller skips it. */
            None => continue,
        };

        let mut buf = [0u8; 64];
        let name = match dev.name(&mut buf) {
            Some(name) => name,
            None => {
                ok &= check("every device in the table has a name", false);
                continue;
            }
        };

        ok &= check("and answers to it with the handle the walk gave",
            Disk::open(name).map(|found| found.handle()) == Some(dev.handle()));

        /* A partition's parent is a device of the table's, and not itself. */
        if let Some(parent) = dev.parent() {
            ok &= check("a partition's disk is a device of the table's",
                parent.handle() != dev.handle() && parent.sectors() >= dev.sectors());
        }
    }

    ok
}

/// Everything above. 0 when every check passed.
#[no_mangle]
pub extern "C" fn rust_block_selftest() -> i32 {
    if lookups() {
        trace!(0, "block selftest: passed");
        0
    } else {
        -1
    }
}
