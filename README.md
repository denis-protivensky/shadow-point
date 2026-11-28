# shadow-point — Deterministic testing of concurrent Rust code

[![crates.io](https://img.shields.io/crates/v/shadow-point)](https://crates.io/crates/shadow-point)
[![docs.rs](https://img.shields.io/docsrs/shadow-point)](https://docs.rs/shadow-point)
[![MSRV](https://img.shields.io/crates/msrv/shadow-point)](#compatibility)
[![License](https://img.shields.io/crates/l/shadow-point)](#license)
[![CI](https://github.com/denis-protivensky/shadow-point/actions/workflows/ci.yml/badge.svg)](https://github.com/denis-protivensky/shadow-point/actions/workflows/ci.yml)

**No more flaky tests.** `shadow-point` lets you name one interleaving of a
concurrent operation and replay it. A hook sits at the operation's
linearization point: the instant it takes effect. Its closure runs the
competing operation right there, on the same value. One test, one trace,
the same result every run.

The guard is private to the thread that installs it. On one thread the
operation under test and the interferer alternate at the hook — concurrency
without parallelism; on several threads every firing worker installs its
own guard and gates in worker bodies pin the chosen order.
Hooks receive `&T`, never `&mut T`, so the instrumented type must use
interior mutability: both actors mutate through shared references.

In production, hooks compile to nothing.

## Quick start

```toml
# Cargo.toml
[dev-dependencies]
shadow-point = "0.5"
```

### 1. Declare the hooks

Call `define_sp!` in the module that owns the instrumented code, gated with
`#[cfg(test)]`. Production builds drop the whole declaration.

```rust,no_run
// src/register.rs

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) mod register_sp {
        before_store(value: u32),
    }
}
```

`mod register_sp` generates a module with fixed names: the `guard`
constructor, the private `Guard<T>`, the `Hook` type, and a hook constant
per hook (full surface: `define_sp!` rustdoc). Distinct module names can
share a file. A guard is thread-local; there is no central handle to
install, clone, or share.

### 2. Fire at the linearization point

```rust,no_run
use std::sync::atomic::{AtomicU32, Ordering};
struct Register(AtomicU32);

impl Register {
    fn store(&self, value: u32) {
        #[cfg(test)]
        shadow_point::invoke!(register_sp, before_store(value));
        self.0.store(value, Ordering::SeqCst);
    }
}
```

`invoke!` takes the module and a plain `hook(args…)` call with exactly the
declared arguments (dispatch prepends the guarded `&T`). It expands to a
block expression (statement or expression position) and compiles to an
empty block outside tests. Under `[dev-dependencies]` each call site needs
its own `#[cfg(test)]` (a production build fails with `E0433` otherwise);
past a few sites, hide the attribute in a local `macro_rules!` seam (FAQ
below; `invoke!`'s "Dev-dependency seam"). With a regular `[dependencies]`
no gating is needed; every `invoke!` still compiles away.

### 3. A guard test

```rust,ignore
use std::sync::atomic::{AtomicU32, Ordering};

fn the_hook_runs_before_the_store() {
    let guard = register_sp::guard(Register(AtomicU32::new(0)));

    // The closure runs on the first fire, with the call's arguments.
    // Registration asserts exactly one fire; `.fires(n)` pins another count.
    guard.before_store(|_register, value| {
        assert_eq!(value, 7);
    });

    guard.store(7);
    assert_eq!(guard.0.load(Ordering::SeqCst), 7);
}
```

*Illustrative snippet, not intended for real code as written: it continues
steps 1–2. The complete runnable files are in `examples/`.*

The guard derefs to `&T`: `guard.store(…)` and `guard.0` are the guarded
value's own API. A fire nested inside a hook on the same installed guard is
suppressed — instrumented code inside a hook does not recurse; a new guard
of the same module is a separate install and is not suppressed.

## Cheat sheet

| Need | Current API |
|---|---|
| one-shot (exactly one) | `guard.<name>(f)` — the hook must fire exactly once; the closure runs at most once, on the hook's first fire and only if no sequence entry expects it (counting rule below) |
| one-shot, other count | `guard.<name>(f).fires(n)` — `n` is the hook's fire count; the closure still runs at most once |
| chained registration | `guard.a(f).fires(2).b(g).fires(1)` — both `fires` forms return the guard |
| count only | `guard.fires(register_sp::a, n)` |
| assert now | `guard.verify()` — same checks as drop, at the call site |
| every fire | `guard.every(\|e\| { e.<name>(f); })` |
| ordered (per worker) | `guard.sequence(\|s\| { s.a(f).b(g); })` — a fire of a hook that a later pending step expects, while an earlier step is unfired, panics; a hook with no pending entry falls through |
| conditional step | `s.when(pred).<name>(f)` — only inside `sequence`; the step fires the first time `pred` passes. `pred` runs on the firing thread, before the step: keep it a pure check (a blocking predicate parks the frame; `sequence`/`verify` from it panic with a `shadow-point:` diagnostic; only those two are diagnosed) |
| cross-thread order | `Gate` in the worker bodies after `invoke!`; dispatch closures record and set gates — a wait there is only the deliberate freeze (see "Using guards across threads"), always with a budget |

`<name>` is a hook declared in `define_sp!`. `.fires(n)` counts hook fires,
not closure runs: the fire-once closure runs only on the hook's first
fire, and only when no sequence entry expects it. If an entry consumes
that fire, or a failing predicate leaves the entry at the head, the closure
never runs — a later fire cannot recover it. The counter still counts
every fire: a retrying operation (the CAS loop in the private example) can
fire twice with one closure run — `.fires(2)` pins that. Without a chain,
registration asserts exactly one fire: a missing fire *and* an extra one
both fail.

Counts are per guard: each worker's guard sees only its own thread's fires.
A second fire-once registration replaces the closure without resetting the
count; an explicit `.fires(n)` or `fires(hook, n)` wins in either order.
A second `sequence` replaces the whole queue; a second `every` replaces the
whole registration.

Every sequence entry is mandatory: an entry left unconsumed at drop fails
the assertion. To assert "this branch must not fire", use `fires(hook, 0)`;
to allow "may or may not fire", split the test. There is no `optional`.

## Using guards across threads

A guard is private to the thread that installs it: a guard on the test
thread does not see worker fires. For a production object shared by
threads, the recipe is one guard per firing thread:

1. Share the value as `Arc<T>`.
2. Inside the worker, install `register_sp::guard(arc.clone())`. `Guard` is
   `!Send`: it is created and dropped on the thread that fires; moving one
   across threads is a compile error, not a runtime trap.
3. Register on that guard what *this* worker's fires must do — `every`
   records the trace, `.fires(n)` counts, `sequence` pins the worker's own
   protocol. Counts are per guard: a worker's guard sees only its own fires.
4. Order the workers with `Gate` in the worker bodies: `set` after the fire
   you want observed, `must_wait(budget)` before the fire that must come
   later. The budget is test policy — a local `Duration` constant.

```rust,ignore
// `Register` and `register_sp` come from the quick start.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

let register = Arc::new(Register(AtomicU32::new(0)));
let stored_1 = Arc::new(shadow_point::Gate::new());

// Writer 1: the guard is created and dropped on its own thread.
let t1 = thread::spawn({
    let (register, stored_1) = (register.clone(), stored_1.clone());
    move || {
        let guard = register_sp::guard(register);
        guard.before_store(|_, value| assert_eq!(value, 1)); // this worker's fire
        guard.store(1);
        stored_1.set(); // after the fire you want observed
    }
});

// Writer 2: wait in the worker body, not in a dispatch closure.
let t2 = thread::spawn({
    let register = register.clone();
    move || {
        stored_1.must_wait(Duration::from_secs(10)); // a test-local budget
        let guard = register_sp::guard(register);
        guard.before_store(|_, value| assert_eq!(value, 2));
        guard.store(2);
    }
});

t1.join().unwrap();
t2.join().unwrap();
assert_eq!(register.0.load(Ordering::SeqCst), 2);
```

*`Register` and `register_sp` are the quick-start declarations;
`#[cfg(test)]` gates omitted. The full example, with a second hook, `every`
traces, and the gate set inside dispatch, is
`examples/register_writers.rs`.*

Dispatch closures (`every`, fire-once, `sequence` steps) run on the firing
stack, inside the instrumented function, holding its locks: normally they
record and set gates only. The deliberate exception is the freeze — the
lost-update pin, where the victim's own hook *waits* between its read and
its stale write (`lost_update_is_pinned_by_a_frozen_private_step` in
`tests/cross_thread.rs`). Freeze only when the thread that releases the
gate needs nothing the blocked frame holds, and only with a budget
(`Gate::must_wait`): the budget turns a mistake into a failing test; it
does not remove the deadlock class.

## Examples

Two runnable examples sit in `examples/`:

- `examples/register_private.rs` — an `AtomicU32` register with a `store`
  and a CAS-loop `increment`. The hook shadows the rival store; the CAS
  retries once, so the hook fires twice while the one-shot closure runs once
  (`.fires(2)`).
- `examples/register_writers.rs` — two writers share one `Arc<Register>`;
  each thread installs its own private guard, `every` records the trace,
  and a `Gate` proves that one writer's store completed before the other
  proceeds.

```sh
cargo test --example register_private
cargo test --example register_writers
```

`cargo test` alone does not run example tests; use `--examples` or
`--all-targets` in CI.

## Migrating from 0.4.x

0.5.0 is a breaking release: the namespace and the types change, the 0.4.0
deprecations are removed, and the `paste` dependency is dropped. For a
private guard, counting semantics are unchanged; there is one install mode,
the private `Guard`. Shared-mode counts were totals across the shared state;
they become per guard (see "Shared-mode users" below).

- Declaration: `define_sp! { pub prefix Register { … } }` →
  `define_sp! { pub mod register_sp { … } }`. The generated type names are
  gone; the surface is `guard`, `Guard`, `Hook`, and a hook constant per
  hook.
- Constructors: `RegisterSp::install_guard(v)` → `register_sp::guard(v)`.
  The shared mode is gone: `install_shared`/`shared(v)`/`Shared<T>`, the
  0.4.x attach `shared.install()` (returning `SharedGuard`), and the
  0.5.0 module name `shared.guard()` have no replacement — give every
  firing thread its own private guard (see "Using guards across threads").
  The 0.4.x guards were `Send`: moving one across threads compiled and was
  a silent runtime trap. The new `Guard<T>` is `!Send` (0.5.0): moving one
  is a compile error.
- Shared-mode users: the old `Shared` handle's `every`/`fires`/`verify`
  registrations become per-worker registrations on each guard; counts are
  per guard, not a total across the shared state; `sequence` is per guard.
  There is no central registration to move.
- Shared `sequence` and parking are gone (parking inside dispatch could
  deadlock on the production locks the parked frame holds). Gates in the
  worker bodies are the replacement — see `examples/register_writers.rs`
  and "Using guards across threads". `park_timeout()` and `SP_PARK_TIMEOUT_MS`
  are gone with it — a gate-wait budget is test policy: a local `Duration`
  constant passed to `must_wait`/`wait_timeout`.
- `invoke!(RegisterSp, before_store(v))` →
  `invoke!(register_sp, before_store(v))`; it is now a block expression
  (works as a closure body / block tail / `let` initializer). Hook
  identifiers: `RegisterSp::before_store` → `register_sp::before_store`.
- Gated step: `s.<name>_when(pred, f)` → `s.when(pred).<name>(f)`.
- Removed: `.expect(n)` → `.fires(n)`; `.expect_calls(hook, n)` →
  `fires(hook, n)`; `optional()` — every entry is mandatory, use
  `fires(hook, 0)` for "must not fire" or split the test. Both `fires`
  forms return the handle: `guard.a(f).fires(2).b(h).fires(1)`.
- Hook arguments must be `Copy` (rejected at the declaration now); declare
  a reference or raw pointer when the call site keeps the value. Reserved
  hook names are the fixed surface (`sequence`, `every`, `fires`, `verify`,
  `guard`, `with_dyn`, `when`); `shared` and `clone` are no longer
  reserved. Details: `define_sp!` rustdoc, "Hook arguments" / "Reserved
  hook names".
- Ownership: assertions and reclamation run at the last owner's drop; see
  "Reclamation and ownership" below.

## FAQ

### What is a linearization point?

The instant a concurrent operation takes effect: the store that publishes
the value, the CAS that wins, the lookup that decides the key is free.
The hook closure runs exactly between "the decision" and "the effect";
`shadow-point` leaves the production API and the runtime alone.

### Why not loom or shuttle?

They *find* an interleaving you did not choose. `shadow-point` pins the one
you already chose, in real code. Use them together: find the bad schedule
with a model checker, then write a deterministic regression test for it here.

### Why not Miri/TSan or a simulator?

Miri and TSan *detect* whatever race a run happens to produce; simulators
replace the runtime. `shadow-point` leaves both the runtime and the
production API alone and scripts one chosen interleaving inside them.

### Why not failpoints?

They overlap partly: inside a crate's own unit tests a shadow hook covers a
failpoint's `return` action, and adds what a failpoint does not script:
running the competing operation before injecting. Outside those tests
(integration tests, staging, production, runtime arming with
`FAILPOINTS`/`fail::cfg`) use `fail`; hook dispatch is compiled only under
`cfg(test)`. Give the hook a test-only flag and check it at the site:

```rust,ignore
struct Full;

fn alloc(capacity: usize) -> Result<(), Full> {
    // hook: before_alloc_node(capacity: usize, inject: &Cell<bool>)
    #[cfg(test)]
    {
        let inject = ::core::cell::Cell::new(false);
        shadow_point::invoke!(map_sp, before_alloc_node(capacity, &inject));
        if inject.get() {
            return Err(Full);
        }
    }
    Ok(())
}
```

The flag is per call; `Cell` is enough because the firing thread's guard is
private to it. The hook writes the flag, the site decides what to return.
Register with `every` when the site may fire more than once, or when the
flag should depend on the call's arguments. Under `[dev-dependencies]`,
wrap the block in a seam macro (see `invoke!`'s "Dev-dependency seam").

### Tokio and thread-locals

The dispatch is per-thread; every firing worker installs its own private
guard on its own thread. With the `tokio-async` feature, `TokioAsyncGate`
lets an async test body wait on fires without parking the executor —
parking holds production locks, fatal on a `current_thread` runtime. On an
executor, a dispatch closure may call only `TokioAsyncGate::fire`, which
does not block; await milestones in the test body. `Gate` parks the OS
thread: wait on it in a synchronous worker body after `invoke!`, or in a
`spawn_blocking` body — never on the executor. A dispatch closure may wait
only as the deliberate freeze described in "Using guards across threads",
with a budget; on an async runtime keep that body off the executor.

### Keeping shadow-point out of production builds

A regular `[dependencies]` entry needs no ceremony — `invoke!` compiles
away — but the crate stays in the production dependency graph, compiled
for every release build. `[dev-dependencies]` keeps it out entirely; in
return each call site needs the `#[cfg(test)]` gate. The robust wiring is
a seam: one local `macro_rules!` carrying the attribute, wired with
`#[macro_use]` ahead of the modules using it, so call sites read
`sp_invoke!(register_sp, before_store(value))`. Verify with `cargo build`
(compiles without the dev-dependency) and `cargo test` (dispatches); the
snippet is in `invoke!`'s "Dev-dependency seam". This crate's `examples/`
call `invoke!` ungated — they compile against the library itself; that
form does not transfer to a consumer's `src/` under `[dev-dependencies]`.

### Reclamation and ownership

`guard` owns the state through an `Rc`; the last owner releases it, and
the guarded value's `Drop` runs there, after the sync-point assertions.
Nested guards of one sync point drop in reverse order; dropping an outer
guard first detaches the inner one, whose fires become no-ops. The inner
guard's drop then restores the outer install with no guard alive, so the
outer's verdict lands outside the test's control (see the `Guard` rustdoc).
`drop(guard)` while an inner guard is live gives no verdict, and the last
owner may be the inner guard. A failed `verify()` owns the diagnostic:
drop then skips the remaining assertions and, on the unwind path, the
value's destructor. On a thread that is already unwinding the state is
deliberately leaked, so a panicking destructor cannot abort the test
binary. Captures of an `every` closure or an unfired fire-once are freed
with the state; a fire-once that did fire is dropped when its slot is
taken. A destructor assertion inside `T::drop` runs at the last-owner
drop. A scenario that abandoned entries fails at the sequence assertion
before that — a finding, not a regression.

The guarded type must be `'static` and mutate through `&self`; it need not
be `Send`/`Sync`, because the guard never leaves its thread.
Hooks are side-effect only: they cannot change what the instrumented
function returns. One hook per `invoke!` site: a fire is identified by hook
name alone, so two sites sharing a name are indistinguishable to `sequence`
and `fires`.

## Compatibility

MSRV is Rust 1.65 (edition 2021). The crate has no default dependencies.
Behind the non-default `tokio-async` feature, optional `tokio` (default
features off, `sync` only, ≥ 1.21) is added. Default builds never resolve
tokio. On Rust 1.65–1.69 pin `tokio` to ≤ 1.38: 1.39 requires Rust 1.70. The
full public API is documented at <https://docs.rs/shadow-point>.

## Debugging

Set `SP_TRACE` (any value, including `0`) to print every hook fire to stderr,
with the hook name, fire index, and firing thread. Inside a closure,
`current_fire()` reports the hook name and the fire index; both indices are
zero-based and name the same fire. The closure runs
on the firing thread, so take thread info from `std::thread::current()`.
`trace_enabled()` answers the same question programmatically. Every drop
assertion names the install site.

## Disclaimer

The initial idea and the first implementation were made by a human.
Subsequent work has been produced by an AI assistant, with human review and
edits. The README and most of the other documentation have been reviewed by
a human. The project is in active internal use.

## License

Dual-licensed under your choice of either the MIT license or the Apache
License, Version 2.0. The SPDX field is in `Cargo.toml`; the texts ship as
`LICENSE-MIT` and `LICENSE-APACHE`.
