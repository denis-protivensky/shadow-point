//! Regression tests for the single-threaded (guard) API.
//!
//! These pin the private dispatch branches: head-expects suppresses
//! fire-once, predicate-fail leaves the entry in place, ordering panics,
//! `optional` semantics, per-thread re-entry suppression (including
//! transitive A -> B -> A across two prefixes), and `current_fire()`.
//!
//! The suite existed as zero tests before the cross-thread rework; the
//! behavior here is the contract the rework must preserve.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

mod single {
    shadow_point::define_sp! {
        pub prefix Single
        {
            a(value: u32),
            b(value: u32),
            c(value: u32),
        }
    }
}

mod first {
    shadow_point::define_sp! {
        pub prefix First
        {
            ping(value: u32),
        }
    }
}

mod second {
    shadow_point::define_sp! {
        pub prefix Second
        {
            ping(value: u32),
        }
    }
}

/// Cloneable counter shared between a test body and captured closures.
#[derive(Clone)]
struct C(Arc<AtomicU32>);

impl C {
    fn new() -> Self {
        Self(Arc::new(AtomicU32::new(0)))
    }
    fn inc(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn get(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

fn push(log: &Mutex<String>, s: &str) {
    log.lock().unwrap().push_str(s);
}

// `invoke!` expands to a `#[cfg(test)]`-attributed block, which is only a
// legal *statement*: hoist fires that must run inside a closure (e.g. under
// `catch_unwind`) into plain functions.
fn fire_b(v: u32) {
    shadow_point::invoke!(single::SingleSp, b(v));
}

#[test]
fn sequence_out_of_order_panics() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|s| {
        // Both entries optional: the ordering panic below is the assertion,
        // and drop must not add a second panic on top of it.
        s.a(|(), _| {}).optional();
        s.b(|(), _| {}).optional();
    });
    let r = std::panic::catch_unwind(|| fire_b(1));
    assert!(
        r.is_err(),
        "firing `b` with `a` at the sequence head must panic"
    );
    assert_ordering_panic(&r, "`b` before `a`", "expected `a` next but `b` fired");
    // Nothing was consumed by the failed fire: `a` is still consumable.
    shadow_point::invoke!(single::SingleSp, a(0));
    drop(g);
}

#[test]
fn when_predicate_gates_until_match_and_blocks_fire_once() {
    let g = single::SingleSp::install_guard(());
    let steps = C::new();
    let fire_once = C::new();
    g.sequence(|s| {
        s.a_when(|v| v == 1, {
            let steps = steps.clone();
            move |(), _| steps.inc()
        });
    });
    g.a({
        let fire_once = fire_once.clone();
        move |(), _| fire_once.inc()
    });
    // a(0): predicate fails -> entry stays at the head; fire-once is NOT
    // triggered (the pending entry still expects this hook).
    shadow_point::invoke!(single::SingleSp, a(0));
    assert_eq!(steps.get(), 0, "predicate-failed fire must not consume");
    assert_eq!(fire_once.get(), 0, "pending head must suppress fire-once");
    // a(1): predicate passes -> consumed and step runs.
    shadow_point::invoke!(single::SingleSp, a(1));
    assert_eq!(steps.get(), 1);
    assert_eq!(fire_once.get(), 0, "fire-once stays suppressed");
    drop(g);
}

#[test]
fn optional_head_not_skipped_in_private_mode() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|s| {
        s.a(|(), _| {}).optional();
        s.b(|(), _| {}).optional();
    });
    // Private mode never skips an `optional` head on a mismatched fire: a
    // later matching entry still means ordering violation.
    let r = std::panic::catch_unwind(|| fire_b(1));
    assert!(r.is_err(), "optional head is not skipped in private mode");
    assert_ordering_panic(
        &r,
        "`b` before optional `a`",
        "expected `a` next but `b` fired",
    );
    drop(g);
}

/// Extract the panic payload of a `catch_unwind` result as a borrowed `&str`
/// (String or `&'static str` payloads; other payloads and no-panic are
/// distinguishable sentinels that fail any `contains` assertion).
fn panic_message(r: &std::thread::Result<()>) -> &str {
    match r {
        Err(p) => p
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| p.downcast_ref::<&'static str>().copied())
            .unwrap_or("<non-string panic>"),
        Ok(()) => "<no panic>",
    }
}

/// Pins the ordering-violation panic: the frozen prefix plus the
/// parameterized tail (`expected `{}` next but `{}` fired`), so
/// swapped/garbled hook names are caught, not just the prefix.
fn assert_ordering_panic(r: &std::thread::Result<()>, what: &str, tail: &str) {
    let msg = panic_message(r);
    assert!(
        msg.contains("sync point ordering violation") && msg.contains(tail),
        "unexpected panic for {what} (want tail {tail:?}): {msg:?}"
    );
}

#[test]
fn trailing_optional_entry_forgiven_at_drop() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|s| {
        s.a(|(), _| {}).b(|(), _| {}).c(|(), _| {}).optional();
    });
    shadow_point::invoke!(single::SingleSp, a(0));
    shadow_point::invoke!(single::SingleSp, b(0));
    // `c` never fires; optional -> drop forgives it.
    drop(g);
}

#[test]
fn fire_once_runs_once_and_expect_calls_pass() {
    let g = single::SingleSp::install_guard(());
    let runs = C::new();
    let every = C::new();
    g.a({
        let runs = runs.clone();
        move |(), _| runs.inc()
    })
    .expect(3);
    g.every(|e| {
        e.a({
            let every = every.clone();
            move |(), _| every.inc()
        });
    });
    shadow_point::invoke!(single::SingleSp, a(0));
    shadow_point::invoke!(single::SingleSp, a(1));
    shadow_point::invoke!(single::SingleSp, a(2));
    assert_eq!(runs.get(), 1, "fire-once runs exactly once");
    assert_eq!(every.get(), 3, "every runs per fire");
    drop(g);
}

#[test]
#[should_panic(expected = "fired 2 time(s), expected 3")]
fn expect_calls_mismatch_panics_at_drop() {
    let g = single::SingleSp::install_guard(());
    g.expect_calls(single::SingleSp::a, 3);
    shadow_point::invoke!(single::SingleSp, a(0));
    shadow_point::invoke!(single::SingleSp, a(1));
    // Explicit drop: the assertion panic originates here, inside the scope.
    drop(g);
}

// The drop assertions must not turn a test failure into a process abort.
//
// A panic raised while the thread is already unwinding is a double panic,
// and a double panic aborts the whole process: the harness stops
// reporting, every test that had not started is lost, and the real
// failure — the test's own panic — is buried under the abort. So when
// `assert_fires` runs during unwinding it prints the same diagnostic
// instead of raising it, and the assertion still fires normally on the
// clean-exit path.
//
// These tests are the in-process shape of that contract: the guarded
// value is dropped by an unwind the test starts on purpose, and the
// `catch_unwind` returns instead of the binary aborting. Without the
// fix the whole test binary dies on SIGABRT rather than reporting a
// single failed test — which is exactly why the regression can only be
// pinned this way, not asserted as a `JoinHandle` error.

#[test]
fn guard_drop_assertion_does_not_abort_while_unwinding() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(());
        g.sequence(|q| {
            q.a(|_, _| ());
        });
        // Fail while the guard is still alive: its Drop runs during the
        // unwinding, with the mandatory sequence entry unconsumed.
        panic!("the test's own failure");
    }));
    assert!(
        outcome.is_err(),
        "the scenario failed as designed, and the drop rode it out"
    );
    // The TLS install of this thread was restored by that Drop: a fire
    // now dispatches to the default no-op, so a fresh guard can assert
    // normally again — the unwind path left no half-installed state.
    let g = single::SingleSp::install_guard(());
    g.sequence(|q| {
        q.a(|_, _| ());
    });
    shadow_point::invoke!(single::SingleSp, a(1));
    drop(g);
}

#[test]
fn expect_mismatch_assertion_does_not_abort_while_unwinding() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(());
        g.expect_calls(single::SingleSp::a, 3);
        shadow_point::invoke!(single::SingleSp, a(0));
        panic!("the test's own failure");
    }));
    assert!(
        outcome.is_err(),
        "the count mismatch was reported, not raised"
    );
}

#[test]
fn shared_last_arc_assertion_does_not_abort_while_unwinding() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shared = single::SingleSp::install_shared(());
        shared.sequence(|q| {
            q.a(|_, _| ());
        });
        let _inst = shared.install();
        // Unwind drops `_inst` (one Arc clone) and then `shared` — the
        // last reference — so the shared assertion fires mid-unwinding.
        panic!("the test's own failure");
    }));
    assert!(outcome.is_err(), "the scenario failed as designed");
}

/// The clean-exit path keeps its teeth: a test that ends with an
/// unconsumed sequence still fails, on a thread that is not unwinding.
/// Pinned beside the unwind tests because both readings of `panicking()`
/// are one branch, and only fixing the loud half would be the real bug.
#[test]
#[should_panic(expected = "sequence not fully consumed")]
fn unconsumed_sequence_still_panics_on_a_clean_exit() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|q| {
        q.a(|_, _| ());
    });
    drop(g);
}

/// A second `sequence` call replaces the deque instead of appending to it:
/// the first registration's steps must not survive and strand a drop
/// assertion, and the second registration must be the only one enforced.
#[test]
fn second_sequence_replaces_the_first() {
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let g = single::SingleSp::install_guard(());
    g.sequence(|q| {
        q.a({
            let log = log.clone();
            move |_, _| log.lock().unwrap().push("first-a")
        });
        q.b(|_, _| panic!("the first registration must be gone"));
    });
    g.sequence(|q| {
        q.b({
            let log = log.clone();
            move |_, _| log.lock().unwrap().push("second-b")
        });
    });
    // Only the replacement is in force: `b` is the head now, and a fire of
    // `a` matches no entry at all (it falls through, it does not consume).
    shadow_point::invoke!(single::SingleSp, a(0));
    shadow_point::invoke!(single::SingleSp, b(1));
    drop(g);
    assert_eq!(
        log.lock().unwrap().clone(),
        vec!["second-b"],
        "the first registration ran nothing, and the head was the replacement's"
    );
}

#[test]
fn nested_invoke_of_same_sp_is_suppressed() {
    let g = single::SingleSp::install_guard(());
    let outer = C::new();
    g.a({
        let outer = outer.clone();
        move |(), _| {
            outer.inc();
            // Nested dispatch of the same hook on the same thread:
            // suppressed before it counts or re-runs.
            shadow_point::invoke!(single::SingleSp, a(9));
        }
    });
    g.expect_calls(single::SingleSp::a, 1);
    shadow_point::invoke!(single::SingleSp, a(0));
    assert_eq!(outer.get(), 1, "nested invoke must not re-run the closure");
    drop(g);
}

#[test]
fn transitive_reentry_across_prefixes_a_b_a() {
    let first = first::FirstSp::install_guard(());
    let second = second::SecondSp::install_guard(());
    let log = Arc::new(Mutex::new(String::new()));
    first.sequence(|s| {
        s.ping({
            let log = log.clone();
            move |(), _| {
                push(&log, "A-");
                shadow_point::invoke!(second::SecondSp, ping(0));
                push(&log, "-a");
            }
        });
    });
    second.ping({
        let log = log.clone();
        move |(), _| {
            push(&log, "B-");
            // Inner `First::ping` from within `Second::ping` must be
            // suppressed: the outer A is still dispatching on this thread
            // (per-sync-point re-entry, not a single global slot).
            shadow_point::invoke!(first::FirstSp, ping(0));
            push(&log, "-b");
        }
    });
    first.expect_calls(first::FirstSp::ping, 1);
    second.expect_calls(second::SecondSp::ping, 1);
    shadow_point::invoke!(first::FirstSp, ping(0));
    assert_eq!(*log.lock().unwrap(), "A-B--b-a");
    drop(first);
    drop(second);
}

#[test]
fn deref_and_current_fire() {
    struct V {
        n: u32,
    }
    assert!(
        shadow_point::current_fire().is_none(),
        "no fire outside a hook"
    );

    let g = single::SingleSp::install_guard(V { n: 42 });
    assert_eq!(g.n, 42, "guard derefs to the value");
    g.a(|v, arg| {
        assert_eq!(v.n, 42);
        assert_eq!(arg, 7);
        let f = shadow_point::current_fire().expect("fire info inside the hook");
        assert_eq!(f.hook, "a");
        assert_eq!(f.index, 0, "first fire has zero-based index 0");
    });
    // trailing comma in invoke! is supported:
    shadow_point::invoke!(single::SingleSp, a(7,));
    assert!(
        shadow_point::current_fire().is_none(),
        "fire info restored after the dispatch"
    );
    drop(g);
}

/// The cost the print-instead-of-raise trade accepts, pinned as a feature:
/// `thread::panicking()` is true inside a `#[should_panic]` test too — the
/// test unwinds by design, with no first failure behind it — so an
/// incomplete scenario there passes with the assertion printed, not raised.
/// The alternative was a double-panic abort of the whole binary (measured
/// on a real consumer: one failing sp-test reported 3 of 73 tests and died
/// on SIGABRT), so this is a deliberate, documented trade. If this test
/// ever fails, the print path silently became a raise again.
#[test]
#[should_panic(expected = "expected panic")]
fn should_panic_test_passes_with_an_incomplete_scenario() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|q| {
        q.a(|_, _| ());
    });
    // The guard is still alive: its drop assertions run while unwinding,
    // are printed, and cannot abort — so the expected panic is what the
    // harness sees, and the test passes.
    panic!("expected panic");
}

/// The way a scenario keeps its teeth across an expected panic: drop the
/// guard before panicking, and the assertion raises on the clean path,
/// aborting none of the reporting. Pinned so the escape hatch the docs
/// recommend is a behavior, not a hope.
#[test]
fn guard_dropped_before_the_expected_panic_still_asserts() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(());
        g.sequence(|q| {
            q.a(|_, _| ());
        });
        drop(g); // the assertion raises here, on a clean thread
        panic!("unreachable");
    }));
    let msg = panic_message(&outcome);
    assert!(
        msg.starts_with("sync point sequence not fully consumed"),
        "the raise is the drop-time assertion itself, and its custom message \
         is the payload (the escape hatch must not degrade into a wrapper \
         that only 'mentions' the assertion): {msg:?}"
    );
}

/// The raised path's payload, pinned for both assertion kinds, because
/// consumers match on it. The sequence path went `assert!(cond, msg)` →
/// `panic!(msg)` — the bare message, the same payload; the count path went
/// `assert_eq!` → `panic!` and was put back to `assert_eq!` after review
/// caught the loss of the `left == right` wrapper. An assertion that now
/// prints instead of raising also lands here on the clean path, so the
/// pin holds without unwinding.
#[test]
fn raised_assertions_keep_their_payload_shape() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(());
        g.expect_calls(single::SingleSp::a, 3);
        shadow_point::invoke!(single::SingleSp, a(0));
        drop(g);
    }));
    let msg = panic_message(&outcome);
    // The wrapper's own wording is version-dependent — stable prints
    // "assertion `left == right` failed" and 1.65 prints
    // "assertion failed: `(left == right)`" with backticked values — so pin
    // the two properties that hold under both: the wrapper is present at all
    // (a bare-message payload would mean the count path regressed to
    // `panic!`), and the custom message consumers match is byte-for-byte in
    // its place.
    assert!(
        msg.starts_with("assertion") && msg.contains("left") && msg.contains("right"),
        "the count path lost its assert_eq! wrapper: {msg:?}"
    );
    assert!(
        msg.contains(
            "sync point hook `a` fired 1 time(s), expected 3 (installed at tests/single_thread.rs:"
        ),
        "the count path's custom message changed: {msg:?}"
    );

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(());
        g.sequence(|q| {
            q.a(|_, _| ());
        });
        drop(g);
    }));
    let msg = panic_message(&outcome);
    assert!(
        msg.starts_with("sync point sequence not fully consumed"),
        "the sequence path must raise the bare message, no wrapper: {msg:?}"
    );
}

// ── Reclamation ──────────────────────────────────────────────────────
//
// The state used to be `Box::leak`ed, so the guarded value's `Drop` never
// ran: a test could not assert on a destructor, and a capture inside a
// registration lived until the process did. Both are now observable, and
// these tests are the contract — each one fails against the leaked build.

/// A guarded value whose `Drop` sets a flag, so a test can see the release
/// without holding a strong handle to the value itself.
struct MarksDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for MarksDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn the_guarded_value_is_dropped_when_the_guard_drops() {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g = single::SingleSp::install_guard(MarksDrop(flag.clone()));
    assert!(
        !flag.load(std::sync::atomic::Ordering::SeqCst),
        "the value is alive while the guard is"
    );
    drop(g);
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst),
        "guard drop released the last reference: the destructor ran"
    );
}

#[test]
fn a_weak_guard_does_not_keep_the_value_alive() {
    // The shape the guide recommends for asserting a destructor: T is the
    // Weak, so the state holds no strong reference, and the test alone
    // decides when the value goes.
    let v = Arc::new(());
    let w = Arc::downgrade(&v);
    let g = single::SingleSp::install_guard(w.clone());
    assert!(w.upgrade().is_some(), "the test holds the value alive");
    drop(g);
    assert!(w.upgrade().is_some(), "the guard was not the last owner");
    drop(v);
    assert!(w.upgrade().is_none(), "nothing leaked kept it alive");
}

#[test]
fn a_captured_handle_is_freed_with_the_state() {
    // The leak's other half: a closure an `every` registration holds is
    // never consumed, so under `Box::leak` its captures were immortal.
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let g = single::SingleSp::install_guard(());
        let s = seen.clone();
        g.every(|q| {
            q.a(move |(), _| s.lock().unwrap().push(1));
        });
        shadow_point::invoke!(single::SingleSp, a(0));
        assert_eq!(seen.lock().unwrap().len(), 1);
        drop(g);
    }
    // The registration's clone of `seen` is gone with the state; the only
    // reference left is this test's.
    assert_eq!(
        Arc::strong_count(&seen),
        1,
        "the every closure's capture was not released — the state still leaks"
    );
}

#[test]
fn a_fire_once_that_never_fired_is_freed_with_the_state() {
    let held = Arc::new(Mutex::new(0u32));
    let g = single::SingleSp::install_guard(());
    let h = held.clone();
    g.a(move |(), _| drop(h)); // registered, never fired
    drop(g);
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "an unconsumed fire-once slot kept its capture alive"
    );
}

#[test]
fn install_inside_a_hook_and_a_reinstall_do_not_break_dispatch() {
    // `with_dyn` must not call out through a live `RefCell` borrow: a hook
    // that installs (or drops) a guard takes the same TLS slot mutably, and
    // a live borrow would turn that into a harness-owned panic.
    let outer_hits = Arc::new(Mutex::new(0u32));
    let g = single::SingleSp::install_guard(());
    let o = outer_hits.clone();
    g.every(|q| {
        q.a(move |(), _| {
            *o.lock().unwrap() += 1;
            // A nested install on the same thread, dropped inside the fire.
            let inner = single::SingleSp::install_guard(());
            inner.b(|(), _| ());
            shadow_point::invoke!(single::SingleSp, b(0));
            drop(inner);
        });
    });
    g.sequence(|q| {
        q.a(|(), _| ());
    });
    shadow_point::invoke!(single::SingleSp, a(0));
    assert_eq!(
        *outer_hits.lock().unwrap(),
        1,
        "the hook re-entered TLS safely"
    );
    drop(g);
    // And the thread's dispatch is clean afterwards: a fresh install works.
    let g2 = single::SingleSp::install_guard(());
    g2.sequence(|q| {
        q.a(|(), _| ());
    });
    shadow_point::invoke!(single::SingleSp, a(1));
    drop(g2);
}

#[test]
fn reuses_of_the_state_address_do_not_suppress_a_live_dispatch() {
    // Re-entry suppression keys on the state's ADDRESS. Reclamation makes
    // an address free to be handed to the next install, so a stale frame
    // could suppress a fire that belongs to a different, live state. The
    // dispatch holds a strong count for its whole duration, which is what
    // prevents that; this loop is the shape that would catch it.
    for i in 0..200 {
        let g = single::SingleSp::install_guard(i);
        let hits = Arc::new(Mutex::new(0u32));
        let h = hits.clone();
        g.every(|q| {
            q.a(move |v, _| {
                *h.lock().unwrap() += 1;
                assert_eq!(*v, i, "the fire reached the state that owns it");
            });
        });
        shadow_point::invoke!(single::SingleSp, a(0));
        assert_eq!(*hits.lock().unwrap(), 1, "iteration {i} suppressed");
        drop(g);
    }
}

#[test]
fn stacked_installs_release_the_inner_and_keep_the_outer_alive() {
    // The outer state lives on through the inner guard's `old` handle: the
    // outer fires must still reach it after the inner scope closes.
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let outer = single::SingleSp::install_guard(MarksDrop(flag.clone()));
    let hits = Arc::new(Mutex::new(0u32));
    let h = hits.clone();
    outer.every(|q| {
        q.a(move |_, _| *h.lock().unwrap() += 1);
    });
    shadow_point::invoke!(single::SingleSp, a(0));
    {
        let inner = single::SingleSp::install_guard(());
        inner.sequence(|q| {
            q.b(|(), _| ());
        });
        shadow_point::invoke!(single::SingleSp, b(0));
        drop(inner);
        assert!(
            !flag.load(std::sync::atomic::Ordering::SeqCst),
            "dropping the inner guard must not release the outer state"
        );
    }
    shadow_point::invoke!(single::SingleSp, a(0));
    assert_eq!(*hits.lock().unwrap(), 2, "the outer install survived");
    drop(outer);
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst),
        "the outer value is freed when its own guard goes"
    );
}

#[test]
fn an_unwinding_guard_does_not_run_the_value_destructor() {
    // Step 3 of the drop order leaks on an unwinding thread on purpose: a
    // guarded value whose `Drop` panics would double-panic and abort. Here
    // the destructor must NOT run at all, and the run must survive.
    static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct CountsDrops;
    impl Drop for CountsDrops {
        fn drop(&mut self) {
            DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let before = DROPS.load(std::sync::atomic::Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = single::SingleSp::install_guard(CountsDrops);
        panic!("the test's own failure");
    }));
    assert!(outcome.is_err(), "the scenario failed as designed");
    assert_eq!(
        DROPS.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "the guarded value must be forgotten while unwinding, not dropped"
    );
}

#[test]
fn an_assertion_panic_does_not_run_the_value_destructor_mid_unwind() {
    // The drop assertions are OUR panic and fire on a clean thread, so
    // `SpGuard::drop` unwinds out of `assert_fires` rather than entering it
    // with `panicking()` already true. The state must be out of the implicit
    // drop path by then: a plain local `Arc` would be freed by that unwind,
    // running `T::drop` mid-unwind — a second panic if the destructor raises,
    // i.e. exactly the abort the assertion order exists to prevent.
    static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct CountsDrops;
    impl Drop for CountsDrops {
        fn drop(&mut self) {
            DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let before = DROPS.load(std::sync::atomic::Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = single::SingleSp::install_guard(CountsDrops);
        g.sequence(|q| {
            q.a(|_, _| ()); // left unconsumed: the drop assertion raises
        });
        drop(g);
    }));
    let err = outcome.expect_err("the unconsumed sequence fails the test");
    let msg = err
        .downcast_ref::<String>()
        .expect("a formatted panic message");
    assert!(
        msg.contains("sequence not fully consumed"),
        "the assertion is the failure, not the destructor: {msg}"
    );
    assert_eq!(
        DROPS.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "the state must leak when its own assertion unwinds, not free `T`"
    );
}
