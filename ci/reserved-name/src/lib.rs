//! Compile-fail fixture for the reserved-name diagnostic.
//!
//! `define_sp!` emits a `compile_error!` for these names, but the same
//! expansion also produces duplicate definitions. The CI job checks that the
//! friendly messages are present, not merely that this crate fails.
//!
//! Covers the 0.5.0 reserved list (D26): the 9 fixed public names
//! (registration methods, module constructors, dispatch fn, gated-step
//! constructor, and `clone`). Plain names like `value`/`state`/`new`/`f`/
//! `NAMES`/`__SP_NAMES`/`__sp_state`/`__sp_value` are legal hook names (the
//! fixed fields and functions live apart from the generated ones in a
//! private submodule) and are NOT listed here.

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
        clone(),
    }
}
