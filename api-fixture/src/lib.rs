//! Compile-only fixture for the macro-expanded public surface.
//!
//! `cargo public-api` on the crate cannot see items `define_sp!` generates,
//! so this crate names every 0.3.x generated item and the deprecated
//! count/optional methods. It must compile against `v0.3.2` and against the
//! current tree: 0.4.0 adds names and deprecates, it removes nothing.
//!
//! The fixture is excluded from the published package. It is not a test
//! target: compiling it is the assertion.

#![allow(deprecated)]

use std::time::Duration;

shadow_point::define_sp! {
    pub prefix Api {
        before_store(value: u32),
        flush(),
    }
}

/// Name the generated types, associated constants, trait, and builders.
pub fn generated_surface_names() {
    let _hook: ApiHook = ApiSp::before_store;
    let _flush_hook: ApiHook = ApiSp::flush;
    let _entry = ApiSp;
    let _guard: Option<ApiSpGuard<u32>> = None;
    let _shared: Option<ApiSharedSp<u32>> = None;
    let _shared_guard: Option<ApiSharedGuard<u32>> = None;
    let _seq: Option<ApiSeqBuilder<u32>> = None;
    let _every: Option<EveryClosures<u32>> = None;
    let _expect: Option<SpExpect<'_, u32>> = None;
    let _trait_object: Option<&dyn ApiSyncPoint> = None;
    let _default_budget: Duration = shadow_point::PARK_TIMEOUT;
    let _shared_entry = ApiSp::install_shared(0);
    let _with_dyn: () = ApiSp::with_dyn(|_| ());
}

/// Name the generated trait methods.
pub fn trait_methods(sp: &dyn ApiSyncPoint) {
    sp.before_store(0);
    sp.flush();
}

/// Name the 0.3.x registration, count, and optional sequence methods.
pub fn old_registration_surface(g: &ApiSpGuard<u32>, shared: &ApiSharedSp<u32>) {
    g.expect_calls(ApiSp::before_store, 1);
    shared.expect_calls(ApiSp::before_store, 1);

    g.before_store(|_, _| ()).expect(1);
    g.flush(|_| ()).expect(1);
    shared.before_store(|_, _| ()).expect(1);
    shared.flush(|_| ()).expect(1);

    g.sequence(|s| {
        s.before_store(|_, _| ())
            .flush(|_| ())
            .before_store_when(|_| true, |_, _| ())
            .flush_when(|| true, |_| ())
            .optional();
    });
    shared.sequence(|s| {
        s.before_store(|_, _| ()).flush(|_| ()).optional();
    });

    g.every(|e| {
        e.before_store(|_, _| ());
        e.flush(|_| ());
    });
    shared.every(|e| {
        e.before_store(|_, _| ());
        e.flush(|_| ());
    });
}

/// Name both constructors and a worker install.
pub fn constructors(shared: &std::sync::Arc<ApiSharedSp<u32>>) {
    let _private = ApiSp::install_guard(0);
    let _shared_entry = ApiSp::install_shared(0);
    let _worker = shared.install();
}
