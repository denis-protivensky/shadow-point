//! Minimal consumer crate for the generated-rustdoc link gate.
//!
//! `define_sp!` is invoked here without `cfg(test)`, so `cargo doc` expands
//! it and resolves the generated items' doc comments in a consumer context.
//! The crate's own docs only see a `cfg(test)` invocation, so they never
//! resolve these links. The CI job then rejects explicit literal-path
//! `crate::`/`Self::`/`self::`/`super::` hrefs in the rendered output:
//! rustdoc does not warn about such a link inside a macro expansion whose
//! span belongs to a dependency.

shadow_point::define_sp! {
    pub prefix Consumer {
        before_store(value: u32),
        flush(),
    }
}
