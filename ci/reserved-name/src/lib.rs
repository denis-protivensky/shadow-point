//! Compile-fail fixture for the reserved-name diagnostic.
//!
//! `define_sp!` emits a `compile_error!` for these names, but the same
//! expansion also produces duplicate definitions. The CI job checks that the
//! friendly messages are present, not merely that this crate fails.
//!
//! Covers the 0.5.0 reserved list (D26): registration methods, the module
//! constructors and dispatch fn, the gated-step constructor, and private
//! state fields.

shadow_point::define_sp! {
    mod reserved_sp {
        sequence(),
        every(),
        fires(),
        verify(),
        guard(),
        shared(),
        with_dyn(),
        when(),
        value(),
        calls(),
        expected(),
        installed_at(),
        sequence_cv(),
        __sp_reported(),
    }
}
