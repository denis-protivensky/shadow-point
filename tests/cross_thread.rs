//! Cross-thread sync point tests.
//!
//! Every worker gets its own private guard: `every` records the trace,
//! `sequence` pins one worker's own protocol, and gates order the workers
//! against each other. The verdict runs at each guard's drop.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shadow_point::Gate;

/// The wait budget for this suite's gate choreography. A test-local
/// policy, not a crate knob: generous enough for a loaded CI machine.
const GATE_TIMEOUT: Duration = Duration::from_secs(10);

mod ct {
    shadow_point::define_sp! {
        pub mod ct_sp {
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
fn order_across_threads_with_gates() {
    let log = mk_log();
    let a_done = Arc::new(Gate::new());
    let b_done = Arc::new(Gate::new());

    let log1 = log.clone();
    let a_done1 = a_done.clone();
    let b_done1 = b_done.clone();
    let t1 = std::thread::spawn(move || {
        let g = ct::ct_sp::guard(());
        g.every(|e| {
            let log = log1.clone();
            e.a(move |(), _| log.lock().unwrap().push("a"));
            let log = log1.clone();
            e.c(move |(), _| log.lock().unwrap().push("c"));
        });
        g.fires(ct::ct_sp::a, 1);
        g.fires(ct::ct_sp::c, 1);
        shadow_point::invoke!(ct::ct_sp, a(0));
        a_done1.set();
        b_done1.must_wait(GATE_TIMEOUT);
        shadow_point::invoke!(ct::ct_sp, c(0));
    });

    let log2 = log.clone();
    let a_done2 = a_done.clone();
    let b_done2 = b_done.clone();
    let t2 = std::thread::spawn(move || {
        let g = ct::ct_sp::guard(());
        g.every(|e| {
            let log = log2.clone();
            e.b(move |(), _| log.lock().unwrap().push("b"));
        });
        g.fires(ct::ct_sp::b, 1);
        a_done2.must_wait(GATE_TIMEOUT);
        shadow_point::invoke!(ct::ct_sp, b(0));
        b_done2.set();
    });

    t1.join().unwrap();
    t2.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["a", "b", "c"]);
}

/// The private-guard-per-worker pattern: the worker pins its own protocol
/// (`a` then `b`) with `sequence`; the racer has no install and only releases
/// the blocked step. The racer's fire must be a no-op: neither a second
/// victim fire nor an out-of-order dispatch into the victim's sequence.
#[test]
fn private_guard_on_a_worker_orders_its_own_protocol() {
    let log = mk_log();
    let in_step = Arc::new(Gate::new());
    let release = Arc::new(Gate::new());

    let log_a = log.clone();
    let log_b = log.clone();
    let in_step1 = in_step.clone();
    let release1 = release.clone();
    let victim = std::thread::spawn(move || {
        let g = ct::ct_sp::guard(());
        g.fires(ct::ct_sp::a, 1);
        g.fires(ct::ct_sp::b, 1);
        g.sequence(|s| {
            s.a(move |(), _| {
                log_a.lock().unwrap().push("a");
                // A blocking wait inside a private step is legal
                // choreography: the private dispatcher never waits on
                // another thread, and the racer below needs nothing this
                // frame holds.
                in_step1.set();
                release1.must_wait(GATE_TIMEOUT);
            });
            s.b(move |(), _| log_b.lock().unwrap().push("b"));
        });
        shadow_point::invoke!(ct::ct_sp, a(0));
        shadow_point::invoke!(ct::ct_sp, b(0));
    });

    let racer = std::thread::spawn(move || {
        in_step.must_wait(GATE_TIMEOUT);
        // No install on this thread: the fire must vanish, not consume the
        // victim's head `b` while the victim is still inside `a`.
        shadow_point::invoke!(ct::ct_sp, b(0));
        release.set();
    });

    victim.join().unwrap();
    racer.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["a", "b"]);
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

    // A set() from another thread wakes a waiter — or, if the main thread
    // is descheduled past the sleep window, is absorbed by the
    // level-triggered flag when the wait starts (either way, true).
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
fn fires_are_counted_per_guard() {
    let count = Arc::new(AtomicU32::new(0));

    let t1 = {
        let count = count.clone();
        std::thread::spawn(move || {
            let g = ct::ct_sp::guard(());
            g.fires(ct::ct_sp::a, 2);
            g.every(|e| {
                let count = count.clone();
                e.a(move |(), _| {
                    let _ = count.fetch_add(1, Ordering::SeqCst);
                });
            });
            shadow_point::invoke!(ct::ct_sp, a(0));
            shadow_point::invoke!(ct::ct_sp, a(0));
        })
    };

    let t2 = {
        let count = count.clone();
        std::thread::spawn(move || {
            let g = ct::ct_sp::guard(());
            g.fires(ct::ct_sp::a, 2);
            g.every(|e| {
                let count = count.clone();
                e.a(move |(), _| {
                    let _ = count.fetch_add(1, Ordering::SeqCst);
                });
            });
            shadow_point::invoke!(ct::ct_sp, a(0));
            shadow_point::invoke!(ct::ct_sp, a(0));
        })
    };

    t1.join().unwrap();
    t2.join().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 4, "every saw all four fires");
}

#[test]
fn chained_fires_counts_each_hook() {
    let g = ct::ct_sp::guard(());
    g.a(|(), _| ()).fires(2).b(|(), _| ()).fires(1);
    shadow_point::invoke!(ct::ct_sp, a(0));
    shadow_point::invoke!(ct::ct_sp, a(0));
    shadow_point::invoke!(ct::ct_sp, b(0));
    g.verify();
}

#[test]
fn stacked_private_guards_restore_the_outer_install() {
    let outer = ct::ct_sp::guard(());
    outer.fires(ct::ct_sp::a, 1);

    let inner = ct::ct_sp::guard(());
    inner.fires(ct::ct_sp::b, 1);
    shadow_point::invoke!(ct::ct_sp, b(0));
    drop(inner);

    // The inner guard's drop restored the outer install: this fire reaches
    // the outer state, not the detached inner one.
    shadow_point::invoke!(ct::ct_sp, a(0));
    drop(outer);
}

#[test]
fn two_threads_same_hook_fires() {
    let log = mk_log();

    let t1 = {
        let log = log.clone();
        std::thread::spawn(move || {
            let g = ct::ct_sp::guard(());
            g.fires(ct::ct_sp::a, 1);
            g.every(|e| {
                let log = log.clone();
                e.a(move |(), _| log.lock().unwrap().push("t1"));
            });
            shadow_point::invoke!(ct::ct_sp, a(0));
        })
    };

    let t2 = {
        let log = log.clone();
        std::thread::spawn(move || {
            let g = ct::ct_sp::guard(());
            g.fires(ct::ct_sp::a, 1);
            g.every(|e| {
                let log = log.clone();
                e.a(move |(), _| log.lock().unwrap().push("t2"));
            });
            shadow_point::invoke!(ct::ct_sp, a(0));
        })
    };

    t1.join().unwrap();
    t2.join().unwrap();
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "both threads fire the same hook against their own private state"
    );
}

/// A value whose `Drop` sets a flag, so a test can see the release without
/// holding a strong handle to the value itself.
struct MarksDrop(Arc<AtomicBool>);

impl Drop for MarksDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn the_guarded_value_is_dropped_with_the_last_arc() {
    let flag = Arc::new(AtomicBool::new(false));
    let g = ct::ct_sp::guard(Arc::new(MarksDrop(flag.clone())));
    assert!(
        !flag.load(Ordering::SeqCst),
        "the value is alive while its Arc is"
    );
    drop(g);
    assert!(
        flag.load(Ordering::SeqCst),
        "the last Arc released the state, and the guarded value with it"
    );
}

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct CountsDrops;

impl Drop for CountsDrops {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_panicking_guard_drop_does_not_run_the_value_destructor() {
    let before = DROPS.load(Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = ct::ct_sp::guard(CountsDrops);
        g.fires(ct::ct_sp::a, 1); // never fired: the drop assertion raises
        panic!("the test's own failure");
    }));
    assert!(outcome.is_err(), "the scenario failed as designed");
    assert_eq!(
        DROPS.load(Ordering::SeqCst),
        before,
        "the guarded value must be forgotten while unwinding, not dropped"
    );
}

#[test]
fn an_assertion_panic_at_guard_drop_does_not_run_the_value_destructor() {
    let before = DROPS.load(Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = ct::ct_sp::guard(CountsDrops);
        g.fires(ct::ct_sp::a, 1); // never fired: the drop assertion raises
        drop(g);
    }));
    let err = outcome.expect_err("the missed count fails the test");
    let msg = err
        .downcast_ref::<String>()
        .expect("a formatted panic message");
    assert!(
        msg.contains("fired 0 time(s), expected 1"),
        "the assertion is the failure, not the destructor: {msg}"
    );
    assert_eq!(
        DROPS.load(Ordering::SeqCst),
        before,
        "the state must leak when its own assertion unwinds, not free the value"
    );
}

/// Lost update pinned by a private step: the victim reads, freezes inside
/// its own hook, the racer writes, and the victim's stale write clobbers it.
/// The final value is the race we chose to pin, not an accident of timing.
#[test]
fn lost_update_is_pinned_by_a_frozen_private_step() {
    let value = Arc::new(AtomicU32::new(0));
    let in_step = Arc::new(Gate::new());
    let release = Arc::new(Gate::new());

    let value_v = value.clone();
    let check_v = value_v.clone();
    let in_step_v = in_step.clone();
    let release_v = release.clone();
    let victim = std::thread::spawn(move || {
        let g = ct::ct_sp::guard(value_v.clone());
        g.a(move |_, _| {
            in_step_v.set();
            release_v.must_wait(GATE_TIMEOUT);
            // The freeze is real only if the racer's distinct write has
            // landed before the victim's stale write; a removed wait fails
            // here or at the final value.
            assert_eq!(check_v.load(Ordering::SeqCst), 100);
        });
        let read = value_v.load(Ordering::SeqCst);
        shadow_point::invoke!(ct::ct_sp, a(0)); // freeze between read and write
        value_v.store(read + 1, Ordering::SeqCst);
    });

    in_step.must_wait(GATE_TIMEOUT);
    value.store(100, Ordering::SeqCst); // the racer's write lands first
    release.set();

    victim.join().unwrap();
    assert_eq!(
        value.load(Ordering::SeqCst),
        1,
        "the victim's stale write clobbered the racer: the freeze pinned the lost update"
    );
}
