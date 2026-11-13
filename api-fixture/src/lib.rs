//! Compile-only fixture for the 0.5.0 macro-expanded public surface.
//!
//! `cargo public-api` on the crate cannot see items `define_sp!` generates,
//! so this crate names every generated item of the module form. It must
//! compile against the current tree: a removal or rename fails here.
//!
//! The fixture is excluded from the published package. It is not a test
//! target: compiling it is the assertion.

use std::time::Duration;

shadow_point::define_sp! {
    pub mod api_sp {
        before_store(value: u32),
        flush(),
    }
}

/// The gated-step handle is generic over the predicate type `P`.
type WhenHandle<'a> = api_sp::When<'a, u32, fn(u32) -> bool>;

/// Name the generated types, hook constants, and hidden dispatch seam.
pub fn generated_surface_names() {
    let _hook: api_sp::Hook = api_sp::before_store;
    let _flush_hook: api_sp::Hook = api_sp::flush;
    let _guard: Option<api_sp::Guard<u32>> = None;
    let _shared: Option<api_sp::Shared<u32>> = None;
    let _seq: Option<api_sp::Seq<u32>> = None;
    let _when: Option<WhenHandle<'_>> = None;
    let _every: Option<api_sp::Every<u32>> = None;
    let _expect: Option<api_sp::Expect<'_, u32>> = None;
    let _shared_expect: Option<api_sp::SharedExpect<'_, u32>> = None;
    let _trait_object: Option<&dyn api_sp::Hooks> = None;
    let _budget: Duration = shadow_point::park_timeout();
    let _with_dyn: () = api_sp::with_dyn(|_| ());
}

/// Name the generated trait methods.
pub fn trait_methods(sp: &dyn api_sp::Hooks) {
    sp.before_store(0);
    sp.flush();
}

/// Name the registration, count, and gated-step methods on the guard.
pub fn guard_surface(g: &api_sp::Guard<u32>) {
    g.fires(api_sp::before_store, 1);
    g.before_store(|_, _| ()).fires(1);
    g.flush(|_| ()).fires(1);
    // Chained fire-once registrations (D21): fires returns &Guard.
    g.before_store(|_, _| ()).fires(2).flush(|_| ()).fires(1);

    g.sequence(|s| {
        s.before_store(|_, _| ()).flush(|_| ());
        s.when(|_| true).before_store(|_, _| ());
        s.when(|| true).flush(|_| ());
    });

    g.every(|e| {
        e.before_store(|_, _| ());
        e.flush(|_| ());
    });

    g.verify();
}

/// Name the same registration surface on the shared handle.
pub fn shared_surface(s: &api_sp::Shared<u32>) {
    s.fires(api_sp::before_store, 1);
    s.before_store(|_, _| ()).fires(1);
    s.flush(|_| ()).fires(1);
    s.before_store(|_, _| ()).fires(2).flush(|_| ()).fires(1);

    s.sequence(|q| {
        q.before_store(|_, _| ()).flush(|_| ());
        q.when(|_| true).before_store(|_, _| ());
    });

    s.every(|e| {
        e.before_store(|_, _| ());
        e.flush(|_| ());
    });

    s.verify();
}

/// Name both constructors, a worker attach, and Shared cloning.
pub fn constructors() {
    let _private = api_sp::guard(0);
    let shared = api_sp::shared(0);
    let _clone = shared.clone();
    let _worker = shared.guard();
}

/// Deref on both handles reaches the guarded value.
pub fn deref(g: &api_sp::Guard<u32>, s: &api_sp::Shared<u32>) {
    fn takes_value(_: &u32) {}
    takes_value(g);
    takes_value(s);
}
