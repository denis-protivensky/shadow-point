//! Compile-fail fixture for the reserved-name diagnostic.
//!
//! `define_sp!` emits a `compile_error!` for these names, but the same
//! expansion also produces duplicate definitions. The CI job checks that the
//! friendly messages are present, not merely that this crate fails.
//!
//! Covers the 0.5.0 reserved list (D26): the 7 fixed public names
//! (registration methods, module constructor, dispatch fn, and gated-step
//! constructor), in plain and raw (`r#sequence`) form. Plain names like
//! `value`/`state`/`new`/`f`/`clone`/`NAMES`/`__SP_NAMES`/`__sp_state`/
//! `__sp_value`/`__sp_fixed` are legal hook names (the fixed fields and
//! functions live apart from the generated ones in a private submodule) and
//! are NOT listed here.

shadow_point::define_sp! {
    mod reserved_sp {
        sequence(),
        every(),
        fires(),
        verify(),
        guard(),
        with_dyn(),
        when(),
    }
}

// Raw identifiers are the same names to rustc; they must get the same
// friendly diagnostic, not a duplicate-definition cascade.
shadow_point::define_sp! {
    mod reserved_raw_sp {
        r#sequence(),
        r#every(),
        r#fires(),
        r#verify(),
        r#guard(),
        r#with_dyn(),
        r#when(),
    }
}
