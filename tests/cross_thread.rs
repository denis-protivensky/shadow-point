//! Cross-thread (shared-mode) sync point tests.
//!
//! Each test creates a shared sync point (`shared`), drives the
//! instrumented hooks from real `std::thread` workers, and asserts on the
//! gate-ordered event trace, call-count aggregation, and the shared drop
//! assertions. Shared mode observes: `every` records, gates order the
//! worker bodies, the verdict runs after the joins.

use std::sync::atomic::{AtomicU32, Ordering};
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
    let shared = ct::ct_sp::shared(());
    // `every` only records the trace and sets gates — never waits: a
    // blocked dispatch would park the worker inside the instrumented
    // function, holding its production locks.
    shared.every(|e| {
        e.a({
            let log = log.clone();
            move |(), _| log.lock().unwrap().push("a")
        });
        e.b({
            let log = log.clone();
            move |(), _| log.lock().unwrap().push("b")
        });
        e.c({
            let log = log.clone();
            move |(), _| log.lock().unwrap().push("c")
        });
    });
    shared.fires(ct::ct_sp::a, 1);
    shared.fires(ct::ct_sp::b, 1);
    shared.fires(ct::ct_sp::c, 1);
    // TLS is per-thread: each worker attaches its own guard from the Arc
    // clone before driving the hooks. The order a -> b -> c is the workers'
    // choreography: each waits in its body, after `invoke!`, on the gate
    // the previous step's closure set.
    let s1 = shared.clone();
    let s2 = shared.clone();
    let a_done1 = a_done.clone();
    let b_done2 = b_done.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
        a_done1.set();
        b_done2.must_wait(GATE_TIMEOUT);
        shadow_point::invoke!(ct::ct_sp, c(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        a_done.must_wait(GATE_TIMEOUT);
        shadow_point::invoke!(ct::ct_sp, b(0));
        b_done.set();
    });
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(*log.lock().unwrap(), ["a", "b", "c"]);
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
fn fires_aggregates_threads() {
    let count = Arc::new(AtomicU32::new(0));
    let shared = ct::ct_sp::shared(());
    shared.fires(ct::ct_sp::a, 4);
    shared.every(|e| {
        e.a({
            let count = count.clone();
            move |(), _| {
                let _ = count.fetch_add(1, Ordering::SeqCst);
            }
        });
    });
    let s1 = shared.clone();
    let s2 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
        shadow_point::invoke!(ct::ct_sp, a(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
        shadow_point::invoke!(ct::ct_sp, a(0));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    // The Arc is dropped after the join: worker guards are gone, so the
    // final counters are race-free (Arc release/acquire happens-before).
    drop(shared);
    assert_eq!(count.load(Ordering::SeqCst), 4, "every saw all 4 fires");
}

#[test]
fn chained_fires_counts_each_worker() {
    let shared = ct::ct_sp::shared(());
    shared.a(|(), _| ()).fires(2);
    let s1 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
    });
    let s2 = shared.clone();
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        shadow_point::invoke!(ct::ct_sp, a(1));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
}

#[test]
#[should_panic(expected = "fired 0 time(s), expected 1")]
fn shared_drop_asserts_a_missed_count() {
    let shared = ct::ct_sp::shared(());
    shared.fires(ct::ct_sp::a, 1); // never fired: the last-Arc verdict fails
    drop(shared);
}

#[test]
fn two_threads_same_hook_fires() {
    let log = mk_log();
    let shared = ct::ct_sp::shared(());
    shared.every(|e| {
        e.b({
            let log = log.clone();
            move |(), _| {
                log.lock().unwrap().push("b");
            }
        });
    });
    shared.fires(ct::ct_sp::b, 2);
    let s1 = shared.clone();
    let s2 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard();
        shadow_point::invoke!(ct::ct_sp, b(0));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        shadow_point::invoke!(ct::ct_sp, b(0));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "both workers' fires are observed exactly once each; order is un-fixed"
    );
}

// ── Shared-mode reclamation ──────────────────────────────────────────
//
// The shared path had the same blind spot as the guard. Reaching the last
// `Arc` now frees the state — and the guarded value's `Drop` runs there,
// which is why the ORDER inside `Owner::drop` is load-bearing: without it,
// the last `Arc` on an unwinding thread would run a consumer destructor
// mid-unwind, the exact abort 0.3.2 removed the crate's own raise for.

/// A shared value whose `Drop` sets a flag, so a test can see the release
/// without holding a strong handle to the value itself.
struct MarksDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for MarksDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn the_shared_value_is_dropped_with_the_last_arc() {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = ct::ct_sp::shared(MarksDrop(flag.clone()));
    assert!(
        !flag.load(Ordering::SeqCst),
        "the value is alive while its Arc is"
    );
    drop(shared);
    assert!(
        flag.load(Ordering::SeqCst),
        "the last Arc released the state, and the guarded value with it"
    );
}

#[test]
fn a_worker_guard_keeps_the_shared_value_alive_until_it_leaves() {
    // The docs already promise this for the ASSERTIONS ("they run only after
    // every worker has dropped its guard"); reclamation extends it to the
    // value. A worker's TLS entry is a strong handle, so a thread inside a
    // dispatch cannot be reading state that the main test just freed.
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = ct::ct_sp::shared(MarksDrop(flag.clone()));
    // A handshake, not sleeps: the assertion below is only about "the worker
    // holds a guard", and wall-clock guesses made this test fail on a loaded
    // machine whenever the main thread lost its slot for longer than the
    // worker's nap — the state then died before the drop it was measuring.
    let (installed_tx, installed_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = {
        let s = shared.clone();
        std::thread::spawn(move || {
            let _inst = s.guard(); // holds a strong handle for its scope
            installed_tx.send(()).expect("the test is still listening");
            release_rx
                .recv()
                .expect("the test releases the worker, not a dropped sender");
        })
    };
    installed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the worker installed its guard");
    drop(shared); // the test's own last reference — a worker still holds one
    assert!(
        !flag.load(Ordering::SeqCst),
        "the value was freed while a worker's install still held the state"
    );
    release_tx.send(()).expect("the worker is still waiting");
    worker.join().unwrap();
    assert!(
        flag.load(Ordering::SeqCst),
        "the value went when the last handle — the worker's guard — left"
    );
}

#[test]
fn dropping_the_shared_handle_with_a_live_guard_gives_no_verdict() {
    // D19/D24: the assertion lives in Owner::drop on the LAST Arc<Owner>,
    // not in Shared::drop. A live WorkerGuard holds an Arc<Owner>, so
    // dropping the Shared handle must neither assert nor free the value —
    // even with a missed count that would fail if the verdict ran here.
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = ct::ct_sp::shared(MarksDrop(flag.clone()));
    shared.fires(ct::ct_sp::a, 1); // never fired: a premature verdict would panic
    let g = shared.guard();
    drop(shared); // NOT the last owner — the guard still holds one
    assert!(
        !flag.load(Ordering::SeqCst),
        "drop(s) with a live guard must not free the value"
    );
    // The guard is the last owner now; its drop runs the verdict. The
    // missed count fails, so catch it rather than let it abort the test.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(g)));
    assert!(
        outcome.is_err(),
        "the last-owner WorkerGuard drop must run the count assertion"
    );
}

#[test]
fn a_panicking_last_arc_does_not_run_the_shared_value_destructor() {
    // The release step is the last thing `Owner::drop` does, and on an
    // unwinding thread it is skipped: `T::drop` is consumer code, and one
    // that panics here would double-panic and abort the run. The state
    // field is an `Option` precisely so this step can be withheld — an
    // implicitly dropped `Arc` field could not be.
    static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct CountsDrops;
    impl Drop for CountsDrops {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    let before = DROPS.load(Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shared = ct::ct_sp::shared(CountsDrops);
        shared.fires(ct::ct_sp::a, 1); // never fired: asserts fire at the end
        let _inst = shared.guard();
        // Unwinding drops `_inst` (one handle) and then `shared` — the last
        // one — so both the assertions and the release run mid-unwind.
        panic!("the test's own failure");
    }));
    assert!(outcome.is_err(), "the scenario failed as designed");
    assert_eq!(
        DROPS.load(Ordering::SeqCst),
        before,
        "the shared value must be forgotten while unwinding, not dropped"
    );
}

#[test]
fn an_assertion_panic_at_the_last_arc_does_not_run_the_shared_value_destructor() {
    // Same hole, shared side: `Owner::drop` raises its assertions on a
    // clean thread, and the state has to be out of the implicit drop path
    // before that, or the unwind frees it and runs `T::drop` mid-unwind.
    static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct CountsDrops;
    impl Drop for CountsDrops {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    let before = DROPS.load(Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shared = ct::ct_sp::shared(CountsDrops);
        shared.fires(ct::ct_sp::a, 1); // never fired: the drop assertion raises
        drop(shared); // the last `Arc` — the assertions run here
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
        "the state must leak when its own assertion unwinds, not free `T`"
    );
}

#[test]
fn an_explicit_count_races_registration_without_losing() {
    // Whichever side wins the `expected` lock, the explicit 2 must be the
    // final count: registration fills only an empty slot.
    let shared = ct::ct_sp::shared(());
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let s1 = shared.clone();
    let b1 = barrier.clone();
    let t1 = std::thread::spawn(move || {
        b1.wait();
        s1.fires(ct::ct_sp::a, 2);
    });

    let s2 = shared.clone();
    let b2 = barrier.clone();
    let t2 = std::thread::spawn(move || {
        b2.wait();
        s2.a(|(), _| ());
    });

    t1.join().unwrap();
    t2.join().unwrap();

    let g = shared.guard();
    shadow_point::invoke!(ct::ct_sp, a(0));
    shadow_point::invoke!(ct::ct_sp, a(1));
    drop(g);
    drop(shared); // last Arc: the shared assertion sees the explicit count
}

#[test]
fn verify_on_the_shared_value_checks_the_total_after_joins() {
    let shared = ct::ct_sp::shared(());
    shared.fires(ct::ct_sp::a, 2);

    let s1 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
    });
    let s2 = shared.clone();
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        shadow_point::invoke!(ct::ct_sp, a(1));
    });

    t1.join().unwrap();
    t2.join().unwrap();
    shared.verify(); // the same 2 fires that the last-Arc drop will assert
}

#[test]
#[should_panic(expected = "fired 1 time(s), expected 2")]
fn verify_on_the_shared_value_reports_a_mismatch() {
    let shared = ct::ct_sp::shared(());
    shared.fires(ct::ct_sp::a, 2);

    let s = shared.clone();
    let t = std::thread::spawn(move || {
        let _g = s.guard();
        shadow_point::invoke!(ct::ct_sp, a(0));
    });
    t.join().unwrap();
    shared.verify();
}
