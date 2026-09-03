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
        s.a(|_, _| {}).optional();
        s.b(|_, _| {}).optional();
    });
    let r = std::panic::catch_unwind(|| fire_b(1));
    assert!(
        r.is_err(),
        "firing `b` with `a` at the sequence head must panic"
    );
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
            move |_, _| steps.inc()
        });
    });
    g.a({
        let fire_once = fire_once.clone();
        move |_, _| fire_once.inc()
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
        s.a(|_, _| {}).optional();
        s.b(|_, _| {}).optional();
    });
    // Private mode never skips an `optional` head on a mismatched fire: a
    // later matching entry still means ordering violation.
    let r = std::panic::catch_unwind(|| fire_b(1));
    assert!(r.is_err(), "optional head is not skipped in private mode");
    drop(g);
}

#[test]
fn trailing_optional_entry_forgiven_at_drop() {
    let g = single::SingleSp::install_guard(());
    g.sequence(|s| {
        s.a(|_, _| {}).b(|_, _| {}).c(|_, _| {}).optional();
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
        move |_, _| runs.inc()
    })
    .expect(3);
    g.every(|e| {
        e.a({
            let every = every.clone();
            move |_, _| every.inc()
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

#[test]
fn nested_invoke_of_same_sp_is_suppressed() {
    let g = single::SingleSp::install_guard(());
    let outer = C::new();
    g.a({
        let outer = outer.clone();
        move |_, _| {
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
            move |_, _| {
                push(&log, "A-");
                shadow_point::invoke!(second::SecondSp, ping(0));
                push(&log, "-a");
            }
        });
    });
    second.ping({
        let log = log.clone();
        move |_, _| {
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
        assert_eq!(f.thread_id, std::thread::current().id());
        assert_eq!(f.thread_name.as_deref(), std::thread::current().name());
    });
    shadow_point::invoke!(single::SingleSp, a(7));
    assert!(
        shadow_point::current_fire().is_none(),
        "fire info restored after the dispatch"
    );
    drop(g);
}
