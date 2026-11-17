//! Shared-mode example: one sync point across two writer threads.
//!
//! The shared sync point observes: `every` closures record both writers'
//! fires and set a `Gate` when T1's store has happened; T2 waits on that
//! gate in its worker body, outside dispatch, before storing 2. The final
//! log and value are deterministic: 1 is stored before 2, and the gate
//! proves T1's store completed before T2 proceeded.
//!
//! Run with:
//!
//! ```sh
//! cargo test --example register_shared
//! ```
//!
//! `cargo test` alone does not run example tests; `--examples` or
//! `--all-targets` does.

use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) mod register_sp {
        before_store(value: u32),
        after_store(value: u32),
    }
}

struct Register(AtomicU32);

impl Register {
    fn new(value: u32) -> Self {
        Self(AtomicU32::new(value))
    }

    fn store(&self, value: u32) {
        shadow_point::invoke!(register_sp, before_store(value));
        self.0.store(value, Ordering::SeqCst);
        shadow_point::invoke!(register_sp, after_store(value));
    }
}

#[cfg(not(test))]
fn main() {
    let register = Register::new(0);
    register.store(7);
    println!("run with `cargo test --example register_shared`");
}

#[cfg(test)]
mod tests {
    use super::*;
    use shadow_point::Gate;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The wait budget for this example's choreography — a test-local
    /// policy, generous enough for a loaded machine.
    const GATE_TIMEOUT: Duration = Duration::from_secs(10);

    type Log = Arc<Mutex<Vec<&'static str>>>;

    #[test]
    fn a_second_writer_waits_for_the_first_to_finish() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let stored = Arc::new(Gate::new());

        let shared = register_sp::shared(Register::new(0));
        // `every` only records and sets gates — never waits: a blocked
        // dispatch would park the writer inside `store`, holding whatever
        // production locks the instrumented function holds.
        shared.every(|e| {
            let log_before = log.clone();
            e.before_store(move |_, value| {
                log_before
                    .lock()
                    .unwrap()
                    .push(if value == 1 { "before 1" } else { "before 2" });
            });
            let (log_after, stored) = (log.clone(), stored.clone());
            e.after_store(move |_, value| {
                log_after
                    .lock()
                    .unwrap()
                    .push(if value == 1 { "after 1" } else { "after 2" });
                if value == 1 {
                    // T1's store has happened; release T2.
                    stored.set();
                }
            });
        });
        shared.fires(register_sp::before_store, 2);
        shared.fires(register_sp::after_store, 2);

        let s1 = shared.clone();
        let t1 = std::thread::spawn(move || {
            let _g = s1.guard();
            s1.store(1);
        });
        let s2 = shared.clone();
        let t2 = std::thread::spawn(move || {
            let _g = s2.guard();
            // Completion, not just consumption: the gate is set from T1's
            // after_store closure, so its 1 is already in the register.
            // The wait lives here, in the worker body — not in dispatch.
            stored.must_wait(GATE_TIMEOUT);
            s2.store(2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            ["before 1", "after 1", "before 2", "after 2"]
        );
        assert_eq!(shared.0.load(Ordering::SeqCst), 2);

        // The last `Arc` drop runs the shared assertions: both hooks fired
        // exactly twice, once per writer.
        drop(shared);
    }
}
