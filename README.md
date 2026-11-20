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

The private mode gets that determinism by giving up parallelism, not
concurrency: two logical actors, the operation under test and the interferer,
alternate at the hook on one real thread. That is also why the instrumented
type must use *interior mutability*: hooks receive `&T`, never `&mut T`, and
both actors mutate through shared references. The shared mode adds real
threads: workers race for real while the sync point observes. `every`
records the trace and `fires` counts; `Gate` rendezvous in worker bodies pin
the chosen interleaving down.

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

`mod register_sp` generates a module with fixed names: the constructors
`guard`/`shared`, the guard types, a hook constant per hook, and the
registration builders (full surface: `define_sp!` rustdoc). Distinct module
names can share a file.

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
block expression — statement or expression position — and compiles to an
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
steps 1–2. The complete runnable file is `examples/register_private.rs`.*

The guard derefs to `&T`: `guard.store(…)` and `guard.0` are the guarded
value's own API. A nested fire of the same sync point on the same thread
is suppressed — instrumented code inside a hook does not recurse.

### 4. A shared test

```rust,ignore
use std::sync::Arc;
use std::time::Duration;

impl Register {
    fn store(&self, value: u32) {
        self.0.store(value, Ordering::SeqCst);
        #[cfg(test)]
        shadow_point::invoke!(register_sp, after_store(value));
    }
}

fn the_second_writer_waits_for_the_first() {
    let shared = register_sp::shared(Register(AtomicU32::new(0)));
    let one_stored = Arc::new(shadow_point::Gate::new());

    // Dispatch closures record and set gates — they never wait.
    shared.every(|e| {
        let gate = one_stored.clone();
        e.after_store(move |_, value| {
            if value == 1 {
                gate.set(); // writer one's store has happened
            }
        });
    });
    shared.fires(register_sp::after_store, 2); // total across workers

    let s1 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _g = s1.guard(); // attach the firing thread
        s1.store(1);
    });
    let s2 = shared.clone();
    let t2 = std::thread::spawn(move || {
        let _g = s2.guard();
        // The wait lives in the worker body, with a test-local budget.
        one_stored.must_wait(Duration::from_secs(10));
        s2.store(2);
    });
    t1.join().unwrap();
    t2.join().unwrap();
    assert_eq!(shared.0.load(Ordering::SeqCst), 2); // 1 before 2
    shared.verify(); // same checks as drop, at the call site
}
```

*Illustrative snippet, not intended for real code as written: it reuses
`Register` from step 2 and declares `after_store(value: u32)` alongside
`before_store`. The complete runnable file is `examples/register_shared.rs`.*

## The two install modes

- `guard` (private). `register_sp::guard(value)` returns a `Guard<T>` on
  one thread; hooks see only that thread's fires, and assertions run at
  the last owner's drop. Concurrency without parallelism: the operation
  under test and the interferer alternate at the hook. A fire from any
  other thread is a no-op; an out-of-order fire panics if a later sequence
  entry still expects that hook.
- `shared` (shared). `register_sp::shared(value)` returns a clonable
  `Shared<T>` (`Clone`, `Send + Sync`). Every firing thread calls
  `shared.guard()` first: the attach-only `WorkerGuard<T>` installs
  dispatch on that thread and keeps the state alive (`Deref` + `Drop`
  only). Register on `Shared` with `every`, `fires`, the per-hook methods,
  and `verify`. Shared mode observes; it does not schedule: dispatch
  closures run inside the instrumented function, so none may wait — they
  record traces and set gates, and worker bodies order themselves with
  `Gate` waits (`must_wait`, a test-local budget) after `invoke!`.
  Assertions run at the last owner's drop, after the workers are joined.

`Guard` and `WorkerGuard` are `!Send`: drop them on the thread that created
them. Moving one across threads is a compile error, not a runtime trap.

Pick the mode by what the test asserts:

| The test asserts | Use |
|---|---|
| the order of steps on one thread | a private guard on that thread |
| each worker's local contract | one private guard inside each worker |
| the cross-thread trace or total counts | one shared sync point and a `guard()` on every firing thread |

The common mistake is to install a guard on the test thread while hooks
fire on workers: those fires disappear.

### Private guards in a multithreaded test

Dispatch is per-thread: give each worker its own private guard, pin its
protocol with `sequence`, and let gates choreograph the workers; fires
from uninstalled threads are no-ops, so only the victim needs
instrumentation. A blocking wait inside a private dispatch closure is
legitimate only when the releasing thread needs nothing the blocked frame
holds (the frame owns the instrumented function's locks); `must_wait`
with a test-local budget bounds a mistake to a failing test. Pinned
pattern: `private_guard_on_a_worker_orders_its_own_protocol` in
`tests/cross_thread.rs`; wait rule: `define_sp!` rustdoc.

## Cheat sheet

| Need | Current API |
|---|---|
| one-shot (exactly one) | `guard.<name>(f)` — the hook must fire exactly once; the closure runs at most once, on the first fire the sequence does not claim (counting rule below) |
| one-shot, other count | `guard.<name>(f).fires(n)` — `n` is the hook's fire count; the closure still runs at most once |
| chained registration | `guard.a(f).fires(2).b(g).fires(1)` — both `fires` forms return the guard |
| count only | `guard.fires(register_sp::a, n)` |
| assert now | `guard.verify()` — same checks as drop, at the call site |
| every fire | `guard.every(\|e\| { e.<name>(f); })` |
| ordered (private only) | `guard.sequence(\|s\| { s.a(f).b(g); })` |
| conditional step | `s.when(pred).<name>(f)` — only inside `sequence`; the step fires the first time `pred` passes. `pred` runs under the sequence lock: keep it a pure check (a blocking predicate can deadlock the test) |
| shared | `register_sp::shared(value)`, then `shared.guard()` per firing thread (attach-only `WorkerGuard`); register on the `Shared` handle |
| cross-thread order | `Gate` in the worker bodies after `invoke!`; dispatch closures (`every`, fire-once) record traces and set gates, never wait |

`<name>` is a hook declared in `define_sp!`. `.fires(n)` counts hook fires,
not closure runs: a fire consumed by a sequence entry or stopped by a
failing predicate never runs the fire-once closure, so a retrying
operation (the CAS loop in the private example) can fire twice with one
closure run — `.fires(2)` pins that. Without a chain, registration asserts
exactly one fire: a missing fire *and* an extra one both fail.

In shared mode the count is the total across the whole shared state, not
one worker's: a hook that fires once per worker needs `.fires(workers)`. A
second fire-once registration replaces the closure without resetting the
count; an explicit `.fires(n)` or `fires(hook, n)` wins in either order.

Every sequence entry is mandatory: an entry left unconsumed at drop fails
the assertion. To assert "this branch must not fire", use `fires(hook, 0)`;
to allow "may or may not fire", split the test. There is no `optional`.

## Migrating from 0.4.x

0.5.0 is a breaking release: the namespace and the types change, the 0.4.0
deprecations are removed, and the `paste` dependency is dropped. Counting
semantics are unchanged; shared mode becomes observe-only. The full name
map is in the CHANGELOG.

- Declaration: `define_sp! { pub prefix Register { … } }` →
  `define_sp! { pub mod register_sp { … } }`. The generated type names are
  gone; the surface is `guard`, `shared`, `Guard`, `Shared`, `WorkerGuard`,
  `Hook`, and a hook constant per hook.
- Constructors: `RegisterSp::install_guard(v)` → `register_sp::guard(v)`;
  `RegisterSp::install_shared(v)` → `register_sp::shared(v)` — `Shared<T>`
  is `Clone`, hand clones to workers, no `Arc` in the signature;
  `shared.install()` → `shared.guard()` (attach-only `WorkerGuard<T>`,
  `Deref` + `Drop`). `Guard<T>` keeps `sequence`; both guard types are
  `!Send`.
- Shared `sequence` and parking are gone (parking inside dispatch could
  deadlock on the production locks the parked frame holds). The
  replacement recipe is in "The two install modes" above. `park_timeout()`
  and `SP_PARK_TIMEOUT_MS` are gone with it — a gate-wait budget is test
  policy: a local `Duration` constant passed to `must_wait`/`wait_timeout`.
- `invoke!(RegisterSp, before_store(v))` →
  `invoke!(register_sp, before_store(v))`; it is now a block expression
  (works as a closure body / block tail / `let` initializer). Hook
  identifiers: `RegisterSp::before_store` → `register_sp::before_store`.
- Gated step: `s.<name>_when(pred, f)` → `s.when(pred).<name>(f)`.
- Removed: `.expect(n)` → `.fires(n)`; `.expect_calls(hook, n)` →
  `fires(hook, n)`; `optional()` — every entry is mandatory, use
  `fires(hook, 0)` for "must not fire" or split the test. Both `fires`
  forms return the handle: `g.a(f).fires(2).b(g).fires(1)`.
- Hook arguments must be `Copy` (rejected at the declaration now); declare
  a reference or raw pointer when the call site keeps the value. Reserved
  hook names are the fixed surface (`sequence`, `every`, `fires`, `verify`,
  `guard`, `shared`, `with_dyn`, `when`, `clone`); any other identifier is
  legal. Details: `define_sp!` rustdoc, "Hook arguments" / "Reserved hook
  names".
- Ownership: assertions and reclamation run at the last owner's drop;
  nested guards of one sync point drop in reverse order (dropping an outer
  guard first detaches the inner one, whose fires become no-ops). See
  "Reclamation and ownership" below.

Registration calls now live on `Shared`: `shared.guard().<name>(f)` →
`shared.<name>(f)`, and likewise for `every`, `fires`, and `verify`.

## Examples

Two runnable examples sit in `examples/`; both are test targets:

- `examples/register_private.rs` — an `AtomicU32` register with a `store`
  and a CAS-loop `increment`. The hook shadows the rival store; the CAS
  retries once, so the hook fires twice while the one-shot closure runs once
  (`.fires(2)`).
- `examples/register_shared.rs` — a register through `shared`; two writer
  threads, `every` records the trace and a `Gate` proves that one writer's
  store completed before the other proceeds.

```sh
cargo test --example register_private
cargo test --example register_shared
```

`cargo test` alone does not run example tests; use `--examples` or
`--all-targets` in CI.

## FAQ

### What is a linearization point?

The instant a concurrent operation takes effect: the store that publishes
the value, the CAS that wins, the lookup that decides the key is free.
The hook closure runs exactly between "the decision" and "the effect",
without touching the production API or the runtime.

### Why not loom or shuttle?

They *find* an interleaving you did not choose. `shadow-point` pins the one
you already chose, in real code. Use them together: find the bad schedule
with a model checker, then write a deterministic regression test for it here.

### Why not failpoints?

They overlap partly: inside a crate's own unit tests a shadow hook covers a
failpoint's `return` action, and adds what a failpoint does not script:
running the competing operation before injecting. Outside those tests
(integration tests, staging, production, runtime arming with
`FAILPOINTS`/`fail::cfg`) use `fail`; hook dispatch is compiled only under
`cfg(test)`. Give the hook a test-only flag and check it at the site:

```rust,no_run
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

The flag is per call, so `Cell` is enough in private and shared mode; the
hook writes it, the site decides what to return. Register with `every` when
the site may fire more than once, or when the flag should depend on the
call's arguments. Under `[dev-dependencies]`, wrap the block in a seam
macro (see `invoke!`'s "Dev-dependency seam").

### Why not Miri/TSan or a simulator?

Miri and TSan *detect* whatever race a run happens to produce; simulators
replace the runtime. `shadow-point` leaves both the runtime and the
production API alone and scripts one chosen interleaving inside them.

### Tokio and thread-locals

The dispatch is per-thread; every firing worker attaches the sync point on
its own thread. With the `tokio-async` feature, `TokioAsyncGate` lets an
async test body wait on fires without parking the executor — parking holds
production locks, fatal on a `current_thread` runtime. Dispatch closures
stay non-blocking (`TokioAsyncGate::fire` is); await milestones in the test
body. Synchronous workers wait on `Gate` in the worker body after
`invoke!`, never inside a shared-mode dispatch closure.

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

`guard`/`shared` own the state through an `Arc`; the last owner releases it,
and the guarded value's `Drop` runs there, after the sync-point assertions.
Both modes share one assertion point: `drop(shared)` while a guard is live
gives no verdict, and the last owner may be a guard, not the `Shared`
handle. A failed `verify()` owns the diagnostic: drop then skips the
remaining assertions and the value's destructor. On a thread that is
already unwinding the state is deliberately leaked, so a panicking
destructor cannot abort the test binary. Captures of an `every` closure or
an unfired fire-once are freed with the state; a fire-once that did fire is
dropped when its slot is taken. A destructor assertion inside `T::drop`
runs at the last-owner drop, so a scenario that abandoned entries can fail
there — a finding, not a regression. `Weak` and flag shapes still control
the destructor's moment.

The guarded type must be `Send + Sync + 'static` and mutate through `&self`.
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
