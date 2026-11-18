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
`register_sp::guard` and `register_sp::shared`, a private-mode `Guard<T>`, a
clonable `Shared<T>` whose `guard()` returns an attach-only `WorkerGuard<T>`,
a hook-identifier constant per hook (`register_sp::before_store`), and the
`sequence`/`every` builders. Every generated item lives inside the module,
so several sync points with distinct module names can share a file directly.

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

`invoke!` takes the module and a plain `hook(args…)` call, using exactly the
arguments declared in `define_sp!` — without the guarded `&T`, which dispatch
prepends. It expands to a block expression (`{ #[cfg(test)] { … } }`), so it
works as a statement (`invoke!(…);`) and in expression position — a closure's
body, a block's trailing value, `let () = invoke!(…);` — in both test and
production builds. Outside test builds it compiles to an empty block.

Under `[dev-dependencies]` the call site carries its own `#[cfg(test)]`: in
a production build the crate name does not resolve at all (`E0433`), before
the macro ever strips its body. The quick start repeats the attribute per
call — fine for a handful of sites, but one forgotten attribute breaks
`cargo build`. Past a few call sites, hide the attribute once in a local
`macro_rules!` seam (`sp_invoke!(register_sp, hook(…))`): the FAQ below and
the `invoke!` documentation, "Dev-dependency seam", carry the full pattern.
With a regular `[dependencies]` no gating is needed; the crate is linked in
production but every `invoke!` compiles away.

### 3. A guard test

```rust,no_run
use std::sync::atomic::{AtomicU32, Ordering};

shadow_point::define_sp! {
    pub mod register_sp { before_store(value: u32), }
}

struct Register(AtomicU32);
impl Register {
    fn store(&self, value: u32) {
        #[cfg(test)]
        shadow_point::invoke!(register_sp, before_store(value));
        self.0.store(value, Ordering::SeqCst);
    }
}

fn the_hook_runs_before_the_store() {
    let guard = register_sp::guard(Register(AtomicU32::new(0)));

    // In this example the closure runs on the first fire, with the call's
    // arguments. Registration asserts exactly one HOOK fire; `.fires(n)`
    // pins a different count.
    guard.before_store(|_register, value| {
        assert_eq!(value, 7);
    });

    guard.store(7);
    assert_eq!(guard.0.load(Ordering::SeqCst), 7);
}
```

*Illustrative snippet, not intended for real code as written. The complete
runnable file is `examples/register_private.rs`.*

The guard derefs to `&T`, so `guard.store(…)` and `guard.0` are the guarded
value's own API. A nested fire of the same sync point on the same thread is
suppressed: calling instrumented code inside a hook does not recurse.

## The two install modes

- `guard` (private). `register_sp::guard(value)` returns a `Guard<T>` on
  one thread; hooks see only that thread's fires. The guard derefs to `&T`,
  and assertions run when the last owner drops. This is concurrency without
  parallelism: the operation under test and the interferer alternate on one
  thread at the hook. A fire from any other thread hits the default no-op
  and vanishes. An out-of-order fire panics if a later sequence entry still
  expects that hook; a fire matching no entry falls through.
- `shared` (shared). `register_sp::shared(value)` returns a clonable
  `Shared<T>` (`Clone`, `Send + Sync`). Every firing thread calls
  `shared.guard()` first. The returned `WorkerGuard<T>` installs dispatch
  on that thread and keeps the state alive; it offers only `Deref` and
  `Drop` and is `!Send`. Register on `Shared` with `every`, `fires`, the
  per-hook methods, and `verify`.

  Shared mode observes; it does not schedule. Use `Gate` in worker bodies
  after `invoke!` to order shared workers. `Gate::new()` gives a reusable
  boolean rendezvous shared with the workers. `Shared::sequence` was removed:
  parking an out-of-order worker inside dispatch could deadlock on the
  production locks it still held. No dispatch closure may wait in shared
  mode: `every` and fire-once callbacks both run on the worker's stack
  inside the instrumented function. They record the trace and set gates;
  worker bodies wait with `must_wait` and a test-local budget. Assertions
  run when the last owner drops, after the workers are joined.

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

Private does not mean single-test: dispatch is per-thread, so a
multithreaded test can give each worker its own private guard and order that
worker's protocol with `sequence`, while gates choreograph the workers
against each other. Fires from threads without an install are no-ops. One
worker's sequence never waits on another thread's hooks. The pattern:

- Victim protocol under contention: the victim worker installs a private
  guard and pins its own steps (`sequence`); the racers run uninstrumented
  or with their own guards. A gate freezes the victim at the chosen step.
  A blocking wait inside a private dispatch closure, including a step, is
  legitimate choreography: the private dispatcher never waits on another
  thread (out-of-order fires panic). Use `must_wait` with a budget, and only
  when the thread that releases the gate needs nothing the blocked frame
  holds. The frame still owns the instrumented function's locks, so a racer
  that needs them deadlocks; the budget bounds that to a failing test, it
  does not remove the class. The sequence catches intra-worker step
  reorderings that counters miss; the racers' effects are asserted after the
  joins.
- Freeze-frame window: generalize the single-hook freeze to a whole path.
  `sequence([step_before(freeze), step_after_recovery(assert)])` orders the
  victim's entire journey through the interference window, not just one
  hook.
- Retry/redo under a controlled racer: the victim's retry choreography
  (`a, b, a, b, …`) stays pinned by its sequence while a gated racer thread
  performs the interference between chosen retries.
- Asymmetric instrumentation: instrument only the victim; leave the racers
  as plain production code. Shared mode would force an attach in every
  firing thread; private per-worker guards do not.

In shared mode, no dispatch closure may block: `every` and fire-once
callbacks both run on the worker's stack inside the instrumented function.
A wait there holds production locks, the deadlock class the removed shared
`sequence` created.

## Cheat sheet

| Need | Current API |
|---|---|
| one-shot (exactly one) | `guard.<name>(f)` — the hook must fire exactly once; the closure runs at most once, on the fire whose per-hook counter was 0 and whose sequence fell through to fire-once |
| one-shot, other count | `guard.<name>(f).fires(n)` — `n` is the hook's fire count; the closure still runs at most once, on that same fire |
| chained registration | `guard.a(f).fires(2).b(g).fires(1)` — both `fires` forms return the guard |
| count only | `guard.fires(register_sp::a, n)` |
| assert now | `guard.verify()` — same checks as drop, at the call site |
| every fire | `guard.every(\|e\| { e.<name>(f); })` |
| ordered (private only) | `guard.sequence(\|s\| { s.a(f).b(g); })` |
| conditional step | `s.when(pred).<name>(f)` — only inside `sequence`; the step fires the first time `pred` passes. `pred` runs under the sequence lock: keep it a pure check (a blocking predicate can deadlock the test) |
| shared | `register_sp::shared(value)`, then `shared.guard()` per firing thread (attach-only `WorkerGuard`); register on the `Shared` handle |
| cross-thread order | `Gate` in the worker bodies after `invoke!`; dispatch closures (`every`, fire-once) record traces and set gates, never wait |

`<name>` is the hook name declared in `define_sp!`; `define_sp!` generates
one registration method per hook, and the table covers the registration
surface. `.fires(n)` asserts how many times the hook fires, not how many
times the closure runs. A one-shot closure runs at most once, on the fire
whose per-hook counter was 0 and whose sequence fell through to fire-once. A
first fire consumed by a sequence entry or stopped by a failing predicate
never runs the closure. An operation that retries (the CAS loop in the
private example below) fires the hook again, so a correct trace can be two
fires with one closure run — that is what `.fires(2)` pins. Without a chain,
registration asserts exactly one fire; that default catches a missing fire
*and* an extra one.

In shared mode the count is the total across the whole shared state, not
the winner's or one worker's: a hook that fires once per worker needs
`.fires(workers)`. A second fire-once registration replaces the closure but
does not reset the count slot; an explicit `.fires(n)` or `fires(hook, n)`
wins whether it is set before or after the registration.

Every sequence entry is mandatory: an entry left unconsumed at drop fails
the assertion. To assert "this branch must not fire", use `fires(hook, 0)`;
to allow "may or may not fire", split the test. There is no `optional`.

## Migrating from 0.4.x

0.5.0 is a breaking release: it changes the namespace and the types, removes
the 0.4.0 deprecations, and drops the `paste` dependency. The counting
semantics are unchanged from 0.4.0. Names, types, and ownership move, and
shared mode becomes observe-only (no cross-thread `sequence`, no parking).

- Module form: `define_sp! { pub prefix Register { … } }` becomes
  `define_sp! { pub mod register_sp { … } }`. The 0.4.0 generated names
  (`RegisterSp`, `RegisterSpGuard`, `RegisterSharedSp`, `RegisterSharedGuard`,
  `RegisterSeqBuilder`, `EveryClosures`, `SpExpect`, `RegisterHook`,
  `RegisterSyncPoint`) are gone. The user-facing surface is `guard`, `shared`,
  `Guard`, `Shared`, `WorkerGuard`, the `Hook` identifier type, and a
  hook-identifier constant per hook; the builders (`Seq`, `When`, `Every`,
  `Expect`) and the `with_dyn` dispatch seam are `#[doc(hidden)]` internals,
  and the `Hooks` dispatch trait is private to the generated submodule
  (not re-exported).
- Constructors: `RegisterSp::install_guard(v)` → `register_sp::guard(v)`;
  `RegisterSp::install_shared(v)` → `register_sp::shared(v)` (returns
  `Shared<T>`, not `Arc<…SharedSp<T>>`); `shared.install()` → `shared.guard()`.
  `Shared` is `Clone`. Hand clones to workers; there is no `Arc` in the
  signature.
- Two guard types: `Guard<T>` is private-mode only and keeps the full
  registration API including `sequence`. `shared.guard()` returns an
  attach-only `WorkerGuard<T>` (`Deref` + `Drop`). Both guard types are
  `!Send`, so moving one across threads is a compile error.
- Shared `sequence` is gone: cross-thread ordering by parking a worker
  inside dispatch could deadlock on the production locks the parked frame
  holds. Order workers with `Gate` waits in the worker bodies after
  `invoke!`; keep every dispatch closure non-blocking (record and set gates
  only). A shared test that pinned consumption order becomes: `every`
  records the trace, gates order the bodies, assert the trace after the
  joins.
- `park_timeout()` and `SP_PARK_TIMEOUT_MS` are gone along with the parking
  they budgeted. A gate-wait budget is test policy: use a local constant
  (`const GATE_TIMEOUT: Duration = Duration::from_secs(10);`) and pass it to
  `must_wait`/`wait_timeout`.
- `invoke!`: `invoke!(RegisterSp, before_store(v))` →
  `invoke!(register_sp, before_store(v))`. It is now a block expression, so
  it also works as a closure body / block tail / `let` initializer.
- Hook identifiers: `RegisterSp::before_store` → `register_sp::before_store`
  (a module constant of type `register_sp::Hook`).
- Gated step: `s.<name>_when(pred, f)` → `s.when(pred).<name>(f)`.
- Removed deprecations: `.expect(n)` → `.fires(n)`;
  `.expect_calls(hook, n)` → `fires(hook, n)`; `PARK_TIMEOUT` and
  `park_timeout()` are both gone (see the gate-budget note above).
  `SeqBuilder::optional()` is gone with no replacement:
  every entry is mandatory; use `fires(hook, 0)` for "must not fire" or split
  the test.
- Chaining: both `fires` forms return the handle, so fire-once registrations
  chain: `g.a(f).fires(2).b(g).fires(1)`.
- Hook arguments must be `Copy`: a non-`Copy` argument is rejected at the
  declaration (it already failed inside the expansion in 0.4.0); declare a
  reference (`key: &K`) or raw pointer when the call site keeps the value.
- Reserved hook names: the list is `sequence`, `every`, `fires`, `verify`,
  `guard`, `shared`, `with_dyn`, `when`, `clone`. These are the fixed public
  surface (`clone` would shadow the generated `Clone` impl on `Shared`). A
  hook with one of these names is rejected with a `compile_error!`. Any other
  identifier is a legal hook name: the fixed fields and functions live apart
  from the generated ones in a private submodule, so names like `value`,
  `state`, `new`, `hook`, `f`, `NAMES`, `__SP_NAMES`, `__sp_state` and
  `__sp_value` do not collide. (Keywords such as `loop` are not identifiers;
  a raw identifier like `r#type` works.)
- Ownership: assertions and reclamation run in one place, the last owner's
  drop, for both modes. `drop(shared)` while a worker guard is live gives no
  verdict and frees nothing; the verdict runs when the last guard leaves.
  Nested guards of one sync point restore a saved TLS install, so drop them
  in reverse order: dropping an outer guard first detaches the inner one,
  whose fires become no-ops. The raise-or-print-on-unwind rule and the
  deliberate leak on a failing thread are unchanged (see "Reclamation and
  ownership" below).

Registration calls now live on `Shared`:

| Before | Now |
|---|---|
| `shared.guard().<name>(f)` | `shared.<name>(f)` |
| `shared.guard().every(f)` | `shared.every(f)` |
| `shared.guard().fires(<name>, n)` | `shared.fires(<name>, n)` |
| `shared.guard().verify()` | `shared.verify()` |

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
the value, the CAS that wins, the lookup that decides the key is free. Put
the hook there and the competing operation runs exactly between "the
decision" and "the effect", without touching the production API or the
runtime.

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
call's arguments: the closure receives them, so no `sequence`/`when` is
involved. Under `[dev-dependencies]`, wrap the block in a seam macro (see
`invoke!`'s "Dev-dependency seam").

### Why not Miri/TSan or a simulator?

Miri and TSan *detect* whatever race a run happens to produce; simulators
replace the runtime. `shadow-point` leaves both the runtime and the
production API alone and scripts one chosen interleaving inside them.

### Tokio and thread-locals

The dispatch is per-thread; every worker that fires a hook must attach the
sync point on its own thread. With the `tokio-async` feature,
`TokioAsyncGate` lets an async test body wait on fires without parking the
executor; a parked thread holds production locks, which is fatal on a
`current_thread` runtime. Keep dispatch closures non-blocking: `every` and
fire-once callbacks both run inside the instrumented function, and
`TokioAsyncGate::fire` is non-blocking. Await milestones in the test body.
For synchronous workers, `Gate` waits belong in the worker bodies after
`invoke!`, never inside a shared-mode dispatch closure.

### Keeping shadow-point out of production builds

A regular `[dependencies]` entry needs no call-site ceremony: `invoke!`
compiles away in non-test builds on its own. The crate still stays in the
production dependency graph, resolved and compiled for every release build.
Under `[dev-dependencies]` production never sees the crate; in return each
`invoke!` call site needs a `#[cfg(test)]` gate, and a forgotten attribute
breaks `cargo build` with `E0433`. The robust wiring is a seam: one local
`macro_rules!` that carries the attribute, wired with `#[macro_use]` ahead
of the modules using it, so call sites read
`sp_invoke!(register_sp, before_store(value))` and the `invoke!` path
appears exactly once, in the seam. Check it holds with `cargo build` (must
compile while the dev-dependency does not resolve) and `cargo test` (must
dispatch). The `invoke!` documentation, "Dev-dependency seam", has the
snippet and the scope rules. The `examples/` in this crate call
`shadow_point::invoke!` ungated: example targets compile against the
library itself, so the path resolves there. The ungated form does not
transfer to a consumer's `src/` under `[dev-dependencies]`, where the call
site needs the gate or the seam.

### Reclamation and ownership

`guard`/`shared` own the state through an `Arc`; the last owner releases it,
and the guarded value's `Drop` runs there, after the sync-point assertions.
Both modes share one assertion point: `drop(shared)` while a guard is live
gives no verdict, and the last owner may be a `Guard` or `WorkerGuard`, not
the `Shared` handle. A failed `verify()` owns the diagnostic: drop then
skips the remaining assertions and the value's destructor. On a thread that
is already unwinding the state is deliberately leaked instead, so a
panicking destructor cannot abort the test binary. Captures of an `every`
closure or an unfired fire-once are freed with the state; a fire-once that
did fire is dropped when its slot is taken. A destructor assertion inside
`T::drop` runs at the last-owner drop, so a scenario that abandoned entries
can fail there. Treat that as a finding, not a regression. `Weak` and flag
shapes still control the destructor's moment.

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
