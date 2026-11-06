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
instrumented type must use *interior mutability*: hooks receive `&T`, never
`&mut T`, and both actors mutate through shared references. The shared mode
adds real threads: workers race for real, and `sequence`/`Gate` pin the
chosen interleaving down.

In production, hooks compile to nothing.

## Quick start

```toml
# Cargo.toml
[dev-dependencies]
shadow-point = "0.4"
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
        #[cfg(test)]
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

Under `[dev-dependencies]` the call site carries its own `#[cfg(test)]`: in
a production build the crate name does not resolve at all (`E0433`), before
the macro ever strips its body. The quick start repeats the attribute per
call — fine for a handful of sites, but one forgotten attribute breaks
`cargo build`. Past a few call sites, hide the attribute once in a local
`macro_rules!` seam (`sp_invoke!(Sp, hook(…))`): the FAQ below and the
`invoke!` documentation, "Dev-dependency seam", carry the full pattern.
With a regular `[dependencies]` no gating is needed; the crate is linked in
production but every `invoke!` compiles away.

### 3. A guard test

```rust,ignore
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn the_hook_runs_before_the_store() {
        let guard = RegisterSp::install_guard(Register(AtomicU32::new(0)));

        // In this example the closure runs on the first fire, with the call's
        // arguments. Registration asserts exactly one HOOK fire; `.fires(n)`
        // pins a different count.
        guard.before_store(|_register, value| {
            assert_eq!(value, 7);
        });

        guard.store(7);
        assert_eq!(guard.0.load(Ordering::SeqCst), 7);
    }
}
```

These snippets are excerpts: they assume `struct Register(AtomicU32);` and
`use std::sync::atomic::{AtomicU32, Ordering};`. The complete file is
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
  for up to `park_timeout()` (10 s by default; `SP_PARK_TIMEOUT_MS`
  overrides it), then panics instead of hanging. Lower the budget only as a
  deadlock guard: a short value on a busy machine can fail a correct test.
  `sequence` orders consumption, not completion: use `Gate` — a
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
| one-shot (exactly one) | `guard.<name>(f)` — the hook must fire exactly once; the closure runs at most once, on the fire whose per-hook counter was 0 and whose sequence fell through to fire-once |
| one-shot, other count | `guard.<name>(f).fires(n)` — `n` is the hook's fire count; the closure still runs at most once, on that same fire |
| count only | `guard.fires(MySp::a, n)` |
| assert now | `guard.verify()` — same checks as drop, at the call site |
| every fire | `guard.every(\|e\| e.<name>(f))` |
| ordered | `guard.sequence(\|s\| { s.a(f).b(g); })` |
| conditional step | `s.<name>_when(pred, f)` — only inside `sequence`; the step fires on the first fire that passes `pred` |
| shared | `install_shared`, then `shared.install()` per firing thread |

`<name>` is the hook name declared in `define_sp!`; `define_sp!` generates
one registration method per hook, and the table covers the registration
surface. **`.fires(n)` asserts how many times the hook fires, not how many
times the closure runs.** A one-shot closure runs at most once, on the fire
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

The deprecated (0.4.0; to be removed in 0.5.0) aliases are `expect(n)` for
`.fires(n)`, `expect_calls(hook, n)` for `fires(hook, n)`, and `optional()`
for a sequence entry. `PARK_TIMEOUT` is deprecated in favor of
`park_timeout()`. `optional` misbehaves in shared mode — it can skip an
entry depending on which thread fires first — so assert "may not fire" with
`fires(hook, 0)`, or split the test.

## Migrating from 0.3.x

0.4.0 is source-compatible with 0.3.x for code that does not declare a
hook named `fires`, `verify`, `__SP_NAMES`, `__SP_COUNT`, `__sp_state`,
`__sp_held`, or `__sp_reported`, but it changes one default, reclaims the
sync-point state, and deprecates four names:

- A fire-once registration without a count now asserts **exactly one** hook
  fire. Add `.fires(n)` to a trace that fires the hook a different number of
  times. The count is hook fires, not closure runs.
- The sync-point state is reclaimed: the guarded value's `Drop` now runs at
  guard drop (shared mode: at the last `Arc`), after the sync-point
  assertions. A test that asserted inside `T::drop` used to have that
  assertion silently skipped; it now runs there. On a thread that is already
  unwinding the state is deliberately leaked. See "Reclamation and ownership
  (0.4.0)" below.
- A hook named `fires`, `verify`, `__SP_NAMES`, `__SP_COUNT`, `__sp_state`,
  `__sp_held`, or `__sp_reported` is now rejected by `define_sp!` with a
  `compile_error!`; rename the hook. The other reserved names already
  collided with 0.3.x's generated surface. A hook named `<other>_when`
  collides with the gated step for `<other>`; avoid declaring that pair.
- `.expect(n)` → `.fires(n)`; `.expect_calls(hook, n)` → `fires(hook, n)`.
  The old names still compile and behave identically, with a deprecation
  warning; they are to be removed in 0.5.0.
- `SeqBuilder::optional()` is deprecated. In shared mode it can make the
  trace depend on which thread fires first. Write `fires(hook, 0)` for "may
  not fire", or split the test; the method is to be removed in 0.5.0.
- `PARK_TIMEOUT` is deprecated in favor of `park_timeout()`, which honors
  `SP_PARK_TIMEOUT_MS`. The constant keeps its 10 s value.
  It is to be removed in 0.5.0.
- `verify()` is new: it runs the drop assertions immediately at the call
  site (useful before an expected panic). It does not replace drop. A failed
  `verify()` unwinds before the guarded value's destructor, so that
  destructor does not run on the failure path.

## Examples

Two runnable examples sit in `examples/`; both are test targets:

- `examples/register_private.rs` — an `AtomicU32` register with a `store`
  and a CAS-loop `increment`. The hook shadows the rival store; the CAS
  retries once, so the hook fires twice while the one-shot closure runs once
  (`.fires(2)`).
- `examples/register_shared.rs` — a register through
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

**Why not failpoints?** They overlap partly: inside a crate's own unit tests
a shadow hook covers a failpoint's `return` action, and adds what a
failpoint cannot — running the competing operation before injecting.
Outside those tests (integration tests, staging, production, runtime arming
with `FAILPOINTS`/`fail::cfg`) use `fail`; hook dispatch is compiled only
under `cfg(test)`. Give the hook a test-only flag and check it at the site:

```rust,ignore
// hook: before_alloc_node(capacity: usize, inject: &Cell<bool>)
#[cfg(test)]
{
    let inject = ::core::cell::Cell::new(false);
    shadow_point::invoke!(MapSp, before_alloc_node(capacity, &inject));
    if inject.get() {
        return Err(Full);
    }
}
```

The flag is per call, so `Cell` is enough in private and shared mode; the
hook writes it, the site decides what to return. Register with `every` when
the site may fire more than once, or when the flag should depend on the
call's arguments: the closure receives them, so no `sequence`/`_when` is
involved. Under `[dev-dependencies]`, wrap the block in a seam macro (see
`invoke!`'s "Dev-dependency seam").

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

**Keeping shadow-point out of production builds.** A regular
`[dependencies]` entry needs no call-site ceremony — `invoke!` compiles
away in non-test builds on its own — but the crate stays in the production
dependency graph: resolved and compiled for every release build. Under
`[dev-dependencies]` production never sees the crate; in return each
`invoke!` call site needs a `#[cfg(test)]` gate, and a forgotten attribute
breaks `cargo build` with `E0433`. The robust wiring is a seam: one local
`macro_rules!` that carries the attribute, wired with `#[macro_use]` ahead
of the modules using it, so call sites read
`sp_invoke!(RegisterSp, before_store(value))` and the `invoke!` path
appears exactly once, in the seam. Check it holds with `cargo build` (must
compile while the dev-dependency does not resolve) and `cargo test` (must
dispatch). The `invoke!` documentation, "Dev-dependency seam", has the
snippet and the scope rules. The `examples/` in this crate call
`shadow_point::invoke!` ungated: example targets compile against the
library itself, so the path resolves there — the ungated form does not
transfer to a consumer's `src/` under `[dev-dependencies]`, where the call
site needs the gate or the seam.

**Reclamation and ownership (0.4.0).** `install_guard`/`install_shared`
own the state in an `Arc`; the last handle releases it, and the guarded
value's `Drop` runs at guard drop (shared mode: at the last `Arc`), after
the sync-point assertions. On a thread that is already unwinding the state
is deliberately leaked instead, so a panicking destructor cannot abort the
test binary. Captures of an `every` closure or an unfired fire-once are
freed with the state; a fire-once that did fire is dropped when its slot is
taken. A destructor assertion inside `T::drop` that used to be skipped now
runs at guard drop, so a scenario that abandoned entries can fail there for
the first time — treat that as a finding, not a regression. `Weak` and flag
shapes still control the destructor's moment; they are no longer workarounds
for a leak.
The guarded type must be `Send + Sync + 'static` and mutate through `&self`.
Hooks are side-effect only — they cannot change what the instrumented
function returns. One hook per `invoke!` site: a fire is identified by hook
name alone, so two sites sharing a name are indistinguishable to `sequence`
and `fires`.

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
