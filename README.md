# shadow-point — Deterministic testing of concurrent Rust code

[![crates.io](https://img.shields.io/crates/v/shadow-point)](https://crates.io/crates/shadow-point)
[![docs.rs](https://img.shields.io/docsrs/shadow-point)](https://docs.rs/shadow-point)
[![MSRV](https://img.shields.io/crates/msrv/shadow-point)](#compatibility)
[![License](https://img.shields.io/crates/l/shadow-point)](#license)
[![CI](https://github.com/denis-protivensky/shadow-point/actions/workflows/ci.yml/badge.svg)](https://github.com/denis-protivensky/shadow-point/actions/workflows/ci.yml)

**No more flaky tests.** `shadow-point` lets you name one interleaving of a
concurrent operation and replay it. A hook sits at the operation's
*linearization point* — the instant it takes effect — and the hook closure
runs the competing operation right there, on the same value. One test, one
trace, the same result every run.

The private mode gets that determinism by giving up parallelism, not
concurrency: two logical actors — the operation under test and the
interferer — alternate at the hook on one real thread. That is also why the
instrumented type must use **interior mutability**: hooks receive `&T`, never
`&mut T`, and both actors mutate through shared references. The shared mode
adds real threads: workers race for real, and `sequence`/`Gate` pin the
chosen interleaving down.

In production, hooks compile to nothing.

## Quick start

```toml
# Cargo.toml
[dev-dependencies]
shadow-point = "0.3"
```

### 1. Declare the hooks

Call `define_sp!` in the module that owns the instrumented code, gated with
`#[cfg(test)]`. Production builds drop the whole declaration.

```rust,ignore
// src/register.rs

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) prefix Register {
        before_store(value: u32),
    }
}
```

`prefix Register` generates `RegisterSp` (the entry point), `RegisterHook`
(hook identifiers, also exposed as associated constants — `RegisterSp::before_store`),
the guard/shared types, and the `sequence`/`every` builders. One `define_sp!`
per module: the macro also emits prefix-free
helper names, so two invocations in one module collide. Put each sync point
in its own file, or in its own `mod { … }`.

### 2. Fire at the linearization point

```rust,ignore
impl Register {
    fn store(&self, value: u32) {
        shadow_point::invoke!(RegisterSp, before_store(value));
        self.0.store(value, Ordering::SeqCst);
    }
}
```

`invoke!` takes the entry point and a plain `hook(args…)` call, using exactly
the arguments declared in `define_sp!` — without the guarded `&T`, which
dispatch prepends. It expands to a statement (an `#[cfg(test)]`-attributed
block), so write `invoke!(…);` on its own line: not as a closure's body, not
as a block's trailing value. Outside test builds it compiles to nothing.

### 3. A guard test

```rust,ignore
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn the_hook_runs_before_the_store() {
        let guard = RegisterSp::install_guard(Register(AtomicU32::new(0)));

        // The closure runs on the first fire, with the call's arguments.
        // `.expect(1)` pins how many times the HOOK must fire.
        guard.before_store(|_register, value| {
            assert_eq!(value, 7);
        }).expect(1);

        guard.store(7);
        assert_eq!(guard.0.load(Ordering::SeqCst), 7);
    }
}
```

These snippets are excerpts: they assume `struct Register(AtomicU32);` and
`use std::sync::atomic::{AtomicU32, Ordering};`; the complete file is
`examples/register_private.rs`.

The guard derefs to `&T`, so `guard.store(…)` and `guard.0` are the guarded
value's own API. A nested fire of the same sync point on the same thread is
suppressed: calling instrumented code inside a hook does not recurse.

## The two install modes

- **`install_guard` — private.** One guard on one thread; hooks see only that
  thread's fires. The guard derefs to `&T`, and assertions run at guard drop.
  This is concurrency without parallelism: the operation under test and the
  interferer alternate on one thread at the hook. A fire from any other
  thread hits the default no-op and vanishes. An out-of-order fire panics if
  a later sequence entry still expects that hook; a fire matching no entry
  falls through.
- **`install_shared` — shared.** `install_shared(value)` returns an
  `Arc<…SharedSp<T>>`; every firing thread calls `shared.install()` first,
  so its TLS dispatch points at the shared state. Cross-thread order comes
  from `sequence`, which parks an out-of-order arrival at the sequence head
  for up to `PARK_TIMEOUT` (a fixed 10 s constant), then panics instead of
  hanging. `sequence` orders consumption, not completion: use `Gate` — a
  reusable boolean rendezvous (`Gate::new()`, shared with the workers) — to
  prove that one thread is inside dispatch before another proceeds.
  Assertions run at the last `Arc` drop, after the workers are joined.

The decision rule: one thread under test → private guard. Several threads →
if each worker's local contract is what you assert, use one private guard
**inside each worker**; if the cross-thread order or total counts are the
assertion, use one shared sync point and install it in every firing thread.
A guard installed on the test thread while the hooks fire on workers is the
most common wrong-mode mistake — those fires disappear.

## Cheat sheet

| Need | Current API |
|---|---|
| one-shot | `guard.<name>(f).expect(n)` — the closure runs on the first fire |
| every fire | `guard.every(\|e\| e.<name>(f))` |
| ordered | `guard.sequence(\|s\| { s.a(f).b(g); })` |
| conditional step | `s.a_when(pred, f)` — only inside `sequence` |
| count only | `guard.expect_calls(MySp::a, n)` |
| shared | `install_shared`, then `shared.install()` per firing thread |

`<name>` is the hook name declared in `define_sp!`; `define_sp!` generates
one registration method per hook, and the table covers the registration
surface. **`.expect(n)` asserts how many times the hook fires, not how many
times the closure runs.** A one-shot closure runs on the first fire only; an
operation that retries (the CAS loop in the private example below) fires the
hook again, so a correct trace can be two fires with one closure run — that
is what `.expect(2)` pins. Without `.expect`, the closure is still
registered, but no count is checked.

`optional()` marks the most recently pushed sequence entry as allowed to
remain unconsumed at drop — the trailing-sentinel pattern. It is a weak
tool: a non-matching predicate or a different hook firing first can leave
the sentinel in place, and the sentinel closure may never run. Prefer a
mandatory entry; use `optional` only when the branch genuinely may not
happen.

## Examples

Two runnable examples sit in `examples/`; both are test targets:

- `examples/register_private.rs` — an `AtomicU32` register with a `store`
  and a CAS-loop `increment`. The hook shadows the rival store; the CAS
  retries once, so the hook fires twice while the one-shot closure runs once
  (`.expect(2)`).
- `examples/register_shared.rs` — the same register through
  `install_shared`; two writer threads, a `sequence` and a `Gate` prove that
  one writer is inside dispatch before the other proceeds.

```sh
cargo test --example register_private
cargo test --example register_shared
```

`cargo test` alone does not run example tests; use `--examples` or
`--all-targets` in CI.

## FAQ

**What is a linearization point?** The instant a concurrent operation takes
effect — the store that publishes the value, the CAS that wins, the lookup
that decides the key is free. Put the hook there and the competing operation
runs exactly between "the decision" and "the effect", without touching the
production API or the runtime.

**Why not loom or shuttle?** They *find* an interleaving you did not choose.
`shadow-point` pins the one you already chose, in real code. Use them
together: find the bad schedule with a model checker, then write a
deterministic regression test for it here.

**Why not failpoints?** Failpoints inject a failure at a named point. A
shadow hook runs the competing operation, with the call's typed arguments,
at the linearization point. Different property, different tool.

**Why not Miri/TSan or a simulator?** Miri and TSan *detect* whatever race a
run happens to produce; simulators replace the runtime. `shadow-point` leaves
both the runtime and the production API alone and scripts one chosen
interleaving inside them.

The alternatives find, inject, detect, or replace; `shadow-point` scripts
one chosen interleaving.

**Tokio and thread-locals.** The dispatch is per-thread; every worker that
fires a hook must install the sync point on its own thread. With the
`tokio-async` feature, `TokioAsyncGate` lets an async test body wait on
fires without parking the executor; a parked thread holds production locks,
which is fatal on a `current_thread` runtime. Order `sequence` so the head
consumer never needs a parked thread's lock.

**Limitations (0.3.x).** The hook state is `Box::leak`ed: the guarded
value's `Drop` does not run, and captured values live until the process ends.
The guarded type must be `Send + Sync + 'static` and mutate through `&self`.
Hooks are side-effect only — they cannot change what the instrumented
function returns. One hook per `invoke!` site: a fire is identified by hook
name alone, so two sites sharing a name are indistinguishable to `sequence`
and `expect_calls`.

## Compatibility

MSRV is Rust 1.65 (edition 2021). The only default dependency is `paste`,
used at macro-expansion time and re-exported by the crate; consumers never
declare it themselves. Behind the non-default `tokio-async` feature, optional
`tokio` (default features off, `sync` only, ≥ 1.21) is added — default builds
never resolve tokio. On Rust 1.65–1.69 pin `tokio` to ≤ 1.38: 1.39 requires
Rust 1.70. The full public API is documented at
<https://docs.rs/shadow-point>.

## Debugging

Set `SP_TRACE` (any value, including `0`) to print every hook fire to stderr,
with the hook name, fire index, and firing thread. Inside a closure,
`current_fire()` reports the hook name and the fire index; the closure runs
on the firing thread, so take thread info from `std::thread::current()`.
`trace_enabled()` answers the same question programmatically. Every drop
assertion names the install site.

## License

Dual-licensed under your choice of either the MIT license or the Apache
License, Version 2.0. The SPDX field is in `Cargo.toml`; the texts ship as
`LICENSE-MIT` and `LICENSE-APACHE`.
