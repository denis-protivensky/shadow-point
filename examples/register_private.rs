//! Private-mode example: one guard, one thread, two logical actors.
//!
//! The hook runs the competing operation at the linearization point — here it
//! stores a rival value just before the real CAS attempt. The CAS loop then
//! retries, so the hook fires twice while the one-shot closure runs once.
//!
//! Run with:
//!
//! ```sh
//! cargo test --example register_private
//! ```
//!
//! `cargo test` alone does not run example tests; `--examples` or
//! `--all-targets` does.

use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) prefix Register {
        before_store(value: u32),
        before_cas(expected: u32),
    }
}

struct Register(AtomicU32);

impl Register {
    fn new(value: u32) -> Self {
        Self(AtomicU32::new(value))
    }

    fn store(&self, value: u32) {
        shadow_point::invoke!(RegisterSp, before_store(value));
        self.0.store(value, Ordering::SeqCst);
    }

    /// A correct CAS loop: it fires `before_cas` on every attempt.
    fn increment(&self) -> u32 {
        let mut cur = self.0.load(Ordering::SeqCst);
        loop {
            shadow_point::invoke!(RegisterSp, before_cas(cur));
            match self
                .0
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return cur + 1,
                Err(actual) => cur = actual,
            }
        }
    }

    /// The naive counterexample: load, fire at the single publish attempt,
    /// store. It loses a rival update between the load and the store.
    fn naive_increment(&self) -> u32 {
        let cur = self.0.load(Ordering::SeqCst);
        shadow_point::invoke!(RegisterSp, before_cas(cur));
        self.0.store(cur + 1, Ordering::SeqCst);
        cur + 1
    }
}

#[cfg(not(test))]
fn main() {
    let register = Register::new(0);
    register.store(7);
    let _ = register.increment();
    let _ = register.naive_increment();
    println!("run with `cargo test --example register_private`");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_runs_before_the_store() {
        let guard = RegisterSp::install_guard(Register::new(0));
        guard
            .before_store(|_, value| assert_eq!(value, 7))
            .expect(1);

        guard.store(7);
        assert_eq!(guard.0.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn the_cas_loop_keeps_the_rival_update() {
        let guard = RegisterSp::install_guard(Register::new(0));
        // The shadow lands on the first attempt and stores 100. The CAS then
        // fails and retries: the hook fires twice, the closure runs once.
        guard
            .before_cas(|reg, _expected| {
                reg.0.store(100, Ordering::SeqCst);
            })
            .expect(2);

        assert_eq!(guard.increment(), 101);
        assert_eq!(guard.0.load(Ordering::SeqCst), 101);
    }

    #[test]
    fn the_naive_load_store_loses_it() {
        let guard = RegisterSp::install_guard(Register::new(0));
        guard
            .before_cas(|reg, _expected| {
                reg.0.store(100, Ordering::SeqCst);
            })
            .expect(1);

        // One fire: the shadow stores 100, then the naive store overwrites it
        // with 1. The increment is lost.
        assert_eq!(guard.naive_increment(), 1);
        assert_eq!(guard.0.load(Ordering::SeqCst), 1);
    }
}
