//! Shared-mode example: one sync point across two writer threads.
//!
//! T1 stores 1 and its `after_store` closure sets a `Gate`; T2 waits on that
//! gate before storing 2. The `sequence` orders T1's before/after steps, and
//! the `every` closures record T2's fires. The final log and value are
//! deterministic: 1 is stored before 2, and the gate proves T1's `after_store`
//! closure (with the store already done) ran before T2 proceeded.
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
    use shadow_point::{park_timeout, Gate};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<&'static str>>>;

    #[test]
    fn a_second_writer_waits_for_the_first_to_finish() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let stored = Arc::new(Gate::new());

        let shared = register_sp::shared(Register::new(0));
        shared.sequence(|s| {
            s.before_store({
                let log = log.clone();
                move |_, _| log.lock().unwrap().push("before 1")
            });
            s.after_store({
                let (log, stored) = (log.clone(), stored.clone());
                move |_, _| {
                    log.lock().unwrap().push("after 1");
                    // T1's store has happened; release T2.
                    stored.set();
                }
            });
        });
        shared.every(|e| {
            let log_before = log.clone();
            // `every` runs for both writers; record only T2's fires.
            e.before_store(move |_, value| {
                if value == 2 {
                    log_before.lock().unwrap().push("before 2");
                }
            });
            let log_after = log.clone();
            e.after_store(move |_, value| {
                if value == 2 {
                    log_after.lock().unwrap().push("after 2");
                }
            });
        });

        let s1 = shared.clone();
        let t1 = std::thread::spawn(move || {
            let _g = s1.guard();
            s1.store(1);
        });
        let s2 = shared.clone();
        let t2 = std::thread::spawn(move || {
            let _g = s2.guard();
            // Completion, not just consumption: T1 sets the gate from inside
            // its after_store closure, so its 1 is already in the register.
            stored.must_wait(park_timeout());
            s2.store(2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            ["before 1", "after 1", "before 2", "after 2"]
        );
        assert_eq!(shared.0.load(Ordering::SeqCst), 2);

        // The last `Arc` drop runs the shared assertions; every sequence
        // entry was consumed.
        drop(shared);
    }
}
