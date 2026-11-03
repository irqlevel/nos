//! The kernel's events Rust records: their numbers and their payloads, and
//! nothing of the kernel besides -- so that a fuzzer compiles this file as it
//! is (fuzz/hv) and its sites build the payloads the kernel's do.

use crate::pod::Pod;

/// A payload's field, as the self-test holds it to the kernel's description.
#[derive(Debug)]
pub struct FieldAt {
    pub name: &'static str,
    pub offset: usize,
    pub size: usize,
}

/// An event's payload: plain data of the layout the kernel's description
/// gives it, and the event it is a record of -- so that no payload can be
/// emitted as another event's.
pub trait Event: Pod {
    const ID: Id;
    /// The event's name, as the kernel's description has it
    const NAME: &'static str;
    /// Every field, padding -- a name that starts with `_` -- included
    const FIELDS: &'static [FieldAt];
}

/// A payload as the self-test compares it with the kernel's description.
pub struct Described {
    pub id: Id,
    pub name: &'static str,
    pub size: usize,
    pub fields: &'static [FieldAt],
}

const fn described<E: Event>() -> Described {
    Described { id: E::ID, name: E::NAME, size: core::mem::size_of::<E>(), fields: E::FIELDS }
}

/// The events, from one list: `Id` and its numbers, each payload's struct --
/// `#[repr(C)]`, each field an integer or an array of them, and no padding,
/// which is asserted -- the event it is a record of, and `EVENTS`, which the
/// boot's self-test (`check`) walks. An event added to the list is in all of
/// them at once: none can be left out of the check.
macro_rules! events {
    ($(
        $(#[$meta:meta])*
        $name:ident = $num:literal, $text:literal {
            $($(#[$fmeta:meta])* $field:ident: $ty:ty,)*
        }
    )*) => {
        /// The kernel's events this side records, by the kernel's numbers
        /// (`tp::Id`, kernel/tracepoint.h): appended to, never reordered --
        /// a saved trace names events by number.
        #[repr(u16)]
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Id {
            $($name = $num,)*
        }

        /// One more than the largest `Id`: how many of the kernel's flags
        /// this side reads.
        pub const ID_LIMIT: usize = {
            let ids = [$($num as usize,)*];
            let mut most = 0;
            let mut i = 0;
            while i < ids.len() {
                if ids[i] + 1 > most {
                    most = ids[i] + 1;
                }
                i += 1;
            }
            most
        };

        $(
            $(#[$meta])*
            #[repr(C)]
            #[derive(Clone, Copy, Debug, Default)]
            pub struct $name {
                $($(#[$fmeta])* pub $field: $ty,)*
            }

            const _: () = assert!(
                core::mem::size_of::<$name>() == 0 $(+ core::mem::size_of::<$ty>())*,
                "a payload with padding, which the kernel would copy out"
            );

            /* SAFETY: #[repr(C)], made of integers and arrays of them, and
               with no padding (the assertion above) */
            unsafe impl Pod for $name {}

            impl Event for $name {
                const ID: Id = Id::$name;
                const NAME: &'static str = $text;
                const FIELDS: &'static [FieldAt] = &[
                    $(FieldAt {
                        name: stringify!($field),
                        offset: core::mem::offset_of!($name, $field),
                        size: core::mem::size_of::<$ty>(),
                    },)*
                ];
            }
        )*

        /// Every payload declared here, each held to the kernel's
        /// description by the boot's self-test (`check`).
        pub const EVENTS: &[Described] = &[$(described::<$name>(),)*];
    };
}

events! {
    /// `test`: what `trace test` and the boot's self-test record -- here,
    /// what the self-test has Rust record, by the path a module's sites take
    Test = 0, "test" {
        seq: u64,
        value: u64,
    }
}
