//! Compile-fail fixture for the reserved-name diagnostic.
//!
//! `define_sp!` emits a `compile_error!` for these names, but the same
//! expansion also produces duplicate definitions. The CI job checks that the
//! friendly messages are present, not merely that this crate fails.
//!
//! Covers the 0.5.0 reserved list (D26): the 8 fixed public names
//! (registration methods, module constructors, dispatch fn, gated-step
//! constructor) plus the `__sp_`-prefixed private machinery names. Plain
//! names like `value`/`state`/`new` are legal hook names (the machinery lives
//! in a private submodule) and are NOT listed here.

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
        __sp_new(),
        __sp_state(),
        __sp_from_owner(),
        __sp_value(),
        __sp_calls(),
        __sp_every(),
        __sp_expected(),
        __sp_installed_at(),
        __sp_reported(),
        __sp_sequence(),
        __sp_sequence_cv(),
        __sp_shared(),
    }
}
