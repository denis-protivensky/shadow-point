//! Cross-thread (shared-mode) sync point tests.
//!
//! Each test installs a shared sync point (`install_shared`), drives the
//! instrumented hooks from real `std::thread` workers, and asserts on the
//! globally ordered sequence, parking diagnostics, optional-head skipping,
//! call-count aggregation, and the shared drop assertions.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shadow_point::{Gate, PARK_TIMEOUT};

mod ct {
    shadow_point::define_sp! {
        pub prefix Ct
        {
            a(value: u32),
            b(value: u32),
            c(value: u32),
        }
    }
}

type Log = Arc<Mutex<Vec<&'static str>>>;

fn mk_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

#[test]
fn sequence_order_across_threads() {
    let log = mk_log();
    let gate_a = Arc::new(Gate::new());
    let gate_b = Arc::new(Gate::new());
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.a({
            let log = log.clone();
            let gate_a = gate_a.clone();
            move |_, _| {
                log.lock().unwrap().push("a");
                gate_a.set();
            }
        });
        s.b({
            let log = log.clone();
            let gate_a = gate_a.clone();
            let gate_b = gate_b.clone();
            move |_, _| {
                // Completion is coordinated with gates: consumption order
                // (a -> b -> c) does not order step *completion*.
                gate_a.must_wait(PARK_TIMEOUT);
                log.lock().unwrap().push("b");
                gate_b.set();
            }
        });
        s.c({
            let log = log.clone();
            move |_, _| {
                log.lock().unwrap().push("c");
            }
        });
    });
    // TLS is per-thread: each worker installs its own guard from the Arc
    // clone before driving the hooks.
    let s1 = shared.clone();
    let s2 = shared.clone();
    let gate_b2 = gate_b.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();
        shadow_point::invoke!(ct::CtSp, a(0));
        // `gate_b` is set inside step `b` only after `b` is consumed, so `c`
        // always finds itself at the head and never parks (the park arm is
        // exercised by park_until_turn instead).
        gate_b2.must_wait(PARK_TIMEOUT);
        shadow_point::invoke!(ct::CtSp, c(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(ct::CtSp, b(0));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(*log.lock().unwrap(), ["a", "b", "c"]);
}

#[test]
fn park_until_turn() {
    let log = mk_log();
    let entered = Arc::new(Gate::new());
    let go = Arc::new(Gate::new());
    let gate_a = Arc::new(Gate::new());
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.a({
            let log = log.clone();
            let gate_a = gate_a.clone();
            move |_, _| {
                log.lock().unwrap().push("a");
                gate_a.set();
            }
        });
        s.b({
            let log = log.clone();
            let gate_a = gate_a.clone();
            move |_, _| {
                // `b`'s step may start as soon as `a` is consumed (the park
                // wake precedes step `a`), so completion must wait for
                // step `a` to finish.
                gate_a.must_wait(PARK_TIMEOUT);
                log.lock().unwrap().push("b");
            }
        });
    });
    // `every` closures run before the sequence head check: `entered` proves
    // T2 is inside `b`'s dispatch while `a` is still at the head, so it is
    // deterministically parked (under a per-__Sp `executing` reversion it
    // would hold the flag that suppresses T1's `a` dispatch instead).
    shared.every(|e| {
        e.b({
            let entered = entered.clone();
            move |_, _| entered.set()
        });
    });
    let s1 = shared.clone();
    let s2 = shared.clone();
    let go_t1 = go.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();
        // Block BEFORE firing: `a` must still be at the head when T2
        // dispatches, so the rendezvous is deterministic.
        go_t1.must_wait(PARK_TIMEOUT);
        shadow_point::invoke!(ct::CtSp, a(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(ct::CtSp, b(0));
    });
    // T2 is provably inside its `b` dispatch, parked at the `a` head.
    // Release T1: `a` is popped, T2 wakes, re-checks, consumes `b`.
    entered.must_wait(PARK_TIMEOUT);
    go.set();
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(*log.lock().unwrap(), ["a", "b"]);
}

#[test]
fn optional_head_skipped_cross_thread() {
    let log = mk_log();
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.a({
            let log = log.clone();
            move |_, _| {
                log.lock().unwrap().push("a");
            }
        })
        .optional()
        .b({
            let log = log.clone();
            move |_, _| {
                log.lock().unwrap().push("b");
            }
        });
    });
    let s2 = shared.clone();
    let t = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(ct::CtSp, b(0));
    });
    t.join().unwrap();
    drop(shared);
    assert_eq!(
        *log.lock().unwrap(),
        ["b"],
        "optional foreign head is skipped in shared mode; no parking"
    );
}

#[test]
fn gate_level_triggered() {
    let gate = Gate::new();
    assert!(!gate.is_set());
    gate.set();
    gate.wait(); // level-triggered: set-before-wait is not lost
    assert!(gate.is_set());
    assert!(gate.wait_timeout(Duration::from_millis(50)));
    gate.clear();
    assert!(!gate.is_set());
    assert!(!gate.wait_timeout(Duration::from_millis(50)));

    // A set() from another thread wakes a waiter.
    let gate = Arc::new(Gate::new());
    let g2 = gate.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        g2.set();
    });
    assert!(gate.wait_timeout(Duration::from_secs(5)));
    t.join().unwrap();
}

#[test]
fn expect_calls_aggregates_threads() {
    let count = Arc::new(AtomicU32::new(0));
    let shared = ct::CtSp::install_shared(());
    shared.expect_calls(ct::CtSp::a, 4);
    shared.every(|e| {
        e.a({
            let count = count.clone();
            move |_, _| {
                let _ = count.fetch_add(1, Ordering::SeqCst);
            }
        });
    });
    let s1 = shared.clone();
    let s2 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();
        shadow_point::invoke!(ct::CtSp, a(0));
        shadow_point::invoke!(ct::CtSp, a(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(ct::CtSp, a(0));
        shadow_point::invoke!(ct::CtSp, a(0));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    // The Arc is dropped after the join: worker guards are gone, so the
    // final counters are race-free (Arc release/acquire happens-before).
    drop(shared);
    assert_eq!(count.load(Ordering::SeqCst), 4, "every saw all 4 fires");
}

#[test]
#[should_panic(expected = "sequence not fully consumed")]
fn shared_drop_asserts_unconsumed() {
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.a(|_, _| {});
    });
    drop(shared);
}

#[test]
fn two_threads_same_hook_entries() {
    let log = mk_log();
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.b({
            let log = log.clone();
            move |_, _| {
                log.lock().unwrap().push("b");
            }
        });
        s.b({
            let log = log.clone();
            move |_, _| {
                log.lock().unwrap().push("b");
            }
        });
    });
    let s1 = shared.clone();
    let s2 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();
        shadow_point::invoke!(ct::CtSp, b(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(ct::CtSp, b(0));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "both same-hook entries are consumed exactly once; order is un-fixed"
    );
}
