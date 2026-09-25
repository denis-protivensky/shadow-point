//! TokioAsyncGate tests (feature = "tokio-async"): milestone coordination on a
//! tokio current_thread runtime, where the std `Gate` would deadlock the
//! executor. Runtimes are built via `Builder` (not `#[tokio::test]`) to keep
//! the runtime setup explicit.

#![cfg(feature = "tokio-async")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use shadow_point::TokioAsyncGate;
use tokio::runtime::Builder;

mod ag {
    shadow_point::define_sp! {
        pub prefix Ag
        {
            a(value: u32),
            b(value: u32),
        }
    }
}

/// The motivating case: an awaited future fires milestones that the test
/// body observes without parking. Determinism rests on `wait_at_least`
/// enabling its `Notify` slot before reading the counter — NOT on the
/// poll order of the spawned `firing` future: whichever runs first, either the
/// waiter sees `count == 0` and sleeps until a fire wakes it, or the fires
/// already landed and the post-`enable()` check returns immediately.
/// With std `Gate::wait` this exact shape hangs forever (single executor
/// thread parks).
#[test]
fn current_thread_body_waits_on_future_fires() {
    let rt = Builder::new_current_thread().build().unwrap();
    let gate = TokioAsyncGate::new();
    let seen = std::sync::Arc::new(AtomicU32::new(0));
    let shared = ag::AgSp::install_shared(());
    let g = gate.clone();
    let s = seen.clone();
    shared.every(|e| {
        e.a(move |_, _| {
            g.fire();
            s.fetch_add(1, Ordering::SeqCst);
        });
    });
    let bound = shared.clone();
    rt.block_on(async move {
        let _guard = bound.install();
        gate.wait_at_least(0).await; // n == 0 returns immediately
        let waiting = gate.wait_at_least(2);
        // `spawn`, not `join!`: keeps the `macros` feature (and its
        // proc-macro chain) out of the dev-dep graph. Order not
        // load-bearing — see doc; `spawn` already queued the fires.
        let firing = tokio::spawn(async {
            shadow_point::invoke!(ag::AgSp, a(0));
            shadow_point::invoke!(ag::AgSp, a(1));
        });
        waiting.await;
        firing.await.unwrap();
        assert_eq!(gate.count(), 2);
    });
    drop(shared);
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

/// Worker side runs on a blocking thread (per-thread guard + install_shared,
/// the only sound install for off-runtime threads), test body waits async.
/// The sleep makes both fires land BEFORE the waiter's first poll, so this
/// deterministically covers the fire-before-`enable()` window: the count
/// check after `enable()` must see the landed fires and return without
/// awaiting. The opposite window — waking an already-enabled waiter — is
/// covered deterministically by the first test (single-thread cooperative
/// polling: the waiter enables before the firing future runs).
#[test]
fn spawn_blocking_worker_fires_async_waiter() {
    let rt = Builder::new_current_thread().build().unwrap();
    let gate = TokioAsyncGate::new();
    let shared = ag::AgSp::install_shared(());
    let g = gate.clone();
    shared.every(|e| {
        e.b(move |_, _| g.fire());
    });
    let worker_shared = shared.clone();
    let worker = rt.spawn_blocking(move || {
        let _guard = worker_shared.install();
        shadow_point::invoke!(ag::AgSp, b(0));
        shadow_point::invoke!(ag::AgSp, b(1));
    });
    std::thread::sleep(Duration::from_millis(50));
    rt.block_on(async {
        gate.wait_at_least(2).await;
        worker.await.unwrap();
    });
    drop(shared);
    assert_eq!(gate.count(), 2);
}
