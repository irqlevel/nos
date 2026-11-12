//! Whose something is that was opened through the kernel's C ABI: the
//! image's, or a module's, by the name the loader knows it by -- found from
//! an address in the module's own code ([`here`], [`Owner::at`]). What every
//! table a module opens something in keeps beside it, so that the loader's
//! take-down closes what a module left open, and says so: a mailbox
//! (layers/ipc), a device claimed or a queue on one (layers/block), an image
//! or a file (layers/fs), a TCP port or connection and NAT (layers/net), an
//! event (base/events). Once only the mailboxes were: the rest stayed for
//! the uptime, a device "held by" a name nothing answered to.

use ffi::system;

/// A module's name at most, as the module table keeps it: kmod's NAME_LEN,
/// less the NUL.
pub const NAME_MAX: usize = 31;

/// Whose something is: the image's, or a module's, by its name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    len: u8,
    name: [u8; NAME_MAX],
}

impl Owner {
    pub const IMAGE: Owner = Owner { len: 0, name: [0; NAME_MAX] };

    /// A module's, by its name: None for one that is no module's name.
    pub fn module(name: &[u8]) -> Option<Owner> {
        if name.is_empty() || name.len() > NAME_MAX || core::str::from_utf8(name).is_err() {
            return None;
        }
        let mut o = Owner { len: name.len() as u8, name: [0; NAME_MAX] };
        o.name[..name.len()].copy_from_slice(name);
        Some(o)
    }

    /// The owner of the code at `addr`: the module whose image holds it --
    /// or the image, for an address in no module.
    pub fn at(addr: usize) -> Owner {
        let mut name = [0u8; NAME_MAX + 1];
        /* `name` takes its length's bytes */
        let n = unsafe { system::kernel_module_name_at(addr, name.as_mut_ptr(), name.len()) };
        if n == 0 || n > NAME_MAX {
            return Owner::IMAGE;
        }
        Owner::module(&name[..n]).unwrap_or(Owner::IMAGE)
    }

    pub fn is_image(&self) -> bool {
        self.len == 0
    }

    /// The module's name; nothing for the image.
    pub fn name(&self) -> &[u8] {
        &self.name[..usize::from(self.len)]
    }

    /// Whether this is the module called `name`.
    pub fn is_module(&self, name: &[u8]) -> bool {
        !self.is_image() && self.name() == name
    }

    /// The module's name, or "kernel" for the image.
    pub fn as_str(&self) -> &str {
        if self.is_image() {
            return "kernel";
        }
        /* Only a UTF-8 name is ever put in */
        core::str::from_utf8(self.name()).unwrap_or("?")
    }
}

/// An address in the code of whoever calls it -- the module's, for a module,
/// kcore being linked into each, and the image's for the image -- for an
/// open to hand the kernel as who it is: what `Owner::at` reads back.
#[inline(never)]
pub fn here() -> usize {
    here as fn() -> usize as usize
}
