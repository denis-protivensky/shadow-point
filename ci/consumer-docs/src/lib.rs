//! Minimal consumer crate for the generated-rustdoc link gate.
//!
//! `define_sp!` is invoked in library code (not behind `cfg(test)`) so that
//! `cargo doc` expands it and resolves the generated items' doc comments in a
//! consumer context. The CI job then rejects literal `crate::`/`Self::` hrefs
//! in the rendered output: rustdoc does not warn about an intra-doc link
//! inside a macro expansion whose span belongs to a dependency.

shadow_point::define_sp! {
    pub prefix Consumer {
        before_store(value: u32),
        flush(),
    }
}
