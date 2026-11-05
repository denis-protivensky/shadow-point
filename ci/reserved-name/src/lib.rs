//! Compile-fail fixture for the reserved-name diagnostic.
//!
//! `define_sp!` emits a `compile_error!` for these names, but the same
//! expansion also produces duplicate definitions. The CI job checks that the
//! friendly messages are present, not merely that this crate fails.

shadow_point::define_sp! {
    prefix Reserved {
        sequence(),
        value(),
        install(),
    }
}
