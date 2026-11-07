//! ipcnet's control mailbox (`ipcnet.ctl`), as a group's module asks it:
//! who this host is, its peers, its key.

use alloc::string::String;
use alloc::vec::Vec;

use ipcwire::control::{self, Identity};
use kcore::ipc::{Limits, Mailbox};
use kcore::time::Duration;

/// How long a call waits for ipcnet's answer: a list of peers may wait out a
/// dropped peer's dial.
const CTL_MS: u64 = 15_000;

/// A request of ipcnet's control mailbox: its answer's bytes, or why not.
pub fn call(tag: u32, data: &[u8]) -> Result<Vec<u8>, String> {
    let mb = Mailbox::new(Limits { messages: 4, bytes: 4096 }).map_err(|e| alloc::format!("no mailbox: {}", e.name()))?;
    let to = kcore::ipc::lookup(control::NAME)
        .ok_or_else(|| String::from("ipcnet is not loaded -- `insmod ipcnet.ko`, and `ipcnet init` its hosts.toml"))?;
    let answer = mb.call(to, tag, data, Duration::from_millis(CTL_MS))
        .map_err(|_| String::from("ipcnet did not answer"))?;
    if answer.tag == control::TAG_REFUSED {
        return Err(alloc::format!("ipcnet: {}", String::from_utf8_lossy(&answer.data)));
    }
    Ok(answer.data)
}

/// Who this host is, as ipcnet has it.
pub fn identity() -> Result<Identity, String> {
    let data = call(control::TAG_ID, &[])?;
    Identity::decode(&data).ok_or_else(|| String::from("ipcnet said who this host is in no form it has"))
}
