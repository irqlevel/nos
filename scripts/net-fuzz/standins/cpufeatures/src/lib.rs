//! The real crate's `new!`, answering no for every feature: what its own
//! Miri support answers.

#![no_std]

#[macro_export]
macro_rules! new {
    ($mod_name:ident, $($tf:tt),+ $(,)?) => {
        mod $mod_name {
            /// Initialization token
            #[derive(Copy, Clone, Debug)]
            pub struct InitToken(());

            impl InitToken {
                /// Get initialized value
                #[inline(always)]
                pub fn get(&self) -> bool {
                    false
                }
            }

            /// Get stored value and initialization token.
            #[inline]
            pub fn init_get() -> (InitToken, bool) {
                (InitToken(()), false)
            }

            /// Get initialization token.
            #[inline]
            pub fn init() -> InitToken {
                init_get().0
            }

            /// Get stored value.
            #[inline]
            pub fn get() -> bool {
                false
            }
        }
    };
}
