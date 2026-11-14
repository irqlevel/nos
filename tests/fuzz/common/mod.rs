//! What the fuzzers of the kernel's layers share, each including it as its
//! own module (`#[path]`), so that the stand-ins below are defined in the
//! program itself and the linker never has to find them in a library: the
//! machine the kernel's crates run on (`machine`), the input read as a
//! stream of choices (`input`), and the runner that gives each input a
//! process of its own and reports what goes wrong (`runner`).

/// A finding that is not a panic: something the code did that it must not.
macro_rules! invariant {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            panic!("invariant: {}", format!($($fmt)*));
        }
    };
}

pub mod input;
pub mod machine;
pub mod runner;
