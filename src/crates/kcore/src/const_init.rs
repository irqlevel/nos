//! A value a `static` can be filled with.

/// What a table in a `static` is built from: a constant of the type, which is
/// the one way to repeat a `T` that is not `Copy` -- `[const { T::INIT }; N]`
/// -- with no constructor to run. The kernel runs none.
pub trait ConstInit {
    const INIT: Self;
}
