# shadow-point — Deterministic testing of concurrent Rust code

Hook points let tests inject code at linearization points of concurrent
operations and *shadow* the competing one: the closure runs in place of
what the other thread would have done. The same `invoke!` call sites
serve two install modes — a private guard drives hooks on one thread, a
shared sync point drives them from any thread (a parking rendezvous at
the sequence head). Under the two modes sit three usage patterns: one
guard on the test thread, one guard per worker thread, and one shared
sync point bound by every worker. Nested fires of the same sync point
on the same thread are silently suppressed. In production they compile
to nothing.

## Contents

- [Install modes and usage patterns](#install-modes-and-usage-patterns)
- [Quick start](#quick-start)
- [Using as a dev-dependency](#using-as-a-dev-dependency)
- [What `define_sp!` generates](#what-define_sp-generates)
- [Guard API (private mode)](#guard-api-private-mode)
- [Hook arguments](#hook-arguments)
- [Per-worker guards: private mode on real threads](#per-worker-guards-private-mode-on-real-threads)
- [Shared mode: one sync point across threads](#shared-mode-one-sync-point-across-threads)
- [Async (tokio) consumers](#async-tokio-consumers)
- [Debugging](#debugging)
- [Production safety](#production-safety)
- [Compatibility](#compatibility)
- [Loom](#loom)
- [Comparison with similar crates](#comparison-with-similar-crates)

## Install modes and usage patterns

There are two install *modes* — how the `invoke!` dispatch is bound to
threads — but three *usage patterns* people actually write, and the
mapping is not one-to-one: the private mode serves both the
single-thread test and the per-worker-guard choreography. Both modes
dispatch the same `invoke!` call sites — you choose per test by how you
install, not by how you declare hooks or fire them.

The two modes:

| | `install_guard` — private | `install_shared` — shared |
|---|---|---|
| Whose fires are seen | only the installing thread | every worker that called `install()` on its thread |
| Fires from other threads | silently dropped (they hit the default no-op) | counted, sequenced, aggregated |
| Out-of-order sequence fire | panics immediately if a later entry expects the hook (otherwise the fire falls through) | parks the early thread at the head, panics on `PARK_TIMEOUT` |
| Expected counts | checked at guard drop | aggregated across threads, checked at last `Arc` drop |

The three patterns:

| Pattern | Mode | Threads | Where cross-thread order is asserted | Count checks | Typical scenario |
|---|---|---|---|---|---|
| 1. Scripted interferer | private guard on the test thread | one | n/a — one thread scripts everything | guard Drop | shadow a rival operation at the linearization point (see [What you can test](#what-you-can-test)) |
| 2. Per-worker guards | private guard installed **in each worker thread** | several | your own gates/atomics, in the test's code | each guard's Drop, per thread | each worker's hook behavior is a local contract; workers must merely not overlap |
| 3. Shared sync point | `install_shared` + `install()` per worker | several | the macro: `sequence` parks out-of-order arrivals | aggregated at last `Arc` drop | cross-thread order/counts *are* the assertion |

Decision rule: hooks fire on the thread that runs the instrumented
code, and a private guard is a thread-local — it sees only its own
thread. A guard installed on the *test* thread will not see worker
fires: a thread without an install dispatches to the default no-op and
its fires disappear — the most common wrong-mode mistake. One thread
under test → pattern 1. Several threads → the trigger is *what you
assert*: if the cross-thread ordering lives in your own gates and each
worker's counts are local, use pattern 2 (shared state and its parking
mechanics buy nothing there); if the ordering *is* the assertion, use
pattern 3 and every worker installs.


The same split has two useful readings:

- **Concurrency vs parallelism.** Guard mode is concurrency without
  parallelism: two logical actors — the operation under test and the
  interferer — alternate at the linearization point, deterministically,
  on one real thread; nothing actually races. Shared mode adds the
  parallelism: real threads race for real, and `sequence`/`Gate` pin
  the interleaving down so the chosen scenario reproduces run to run.
- **Interior mutability.** The guard derefs to `&T`, and every hook
  closure receives the same `&T` — never `&mut`. The operation under
  test and the interferer therefore share one value through shared
  references, and the instrumented API must mutate through `&self`
  (interior mutability). That is exactly what lets a hook closure
  perform the rival operation at the linearization point — the
  `map.insert(*key, "rival")` pattern in the examples below — and it
  makes such types the primary use case of the single-threaded mode.

## Quick start

```toml
# Cargo.toml
[dependencies]
shadow-point = "0.3"
```

### 1. Declare hooks

At module level, gate with `#[cfg(test)]` and call `define_sp!`:

```rust
// src/my_module.rs

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) prefix MyModule
    {
        // `K` is this module's key type — use yours or a concrete type.
        before_insert(key: &K),
        before_remove(id: usize),
        after_commit(),
    }
}
```

In production builds `#[cfg(test)]` removes the call entirely.

One `define_sp!` per module: the macro emits prefix-free names
(`__Sp`, `EveryBuilder`, …) that collide when two invocations share a
module — keep sync points in separate files, or give each its own
`mod { … }` inside one file. Scoping the declaration also scopes its
entry point, so `invoke!` call sites outside that module must name the
module path (`invoke!(writer_sp::WriterSp, …)`) — see
[What `define_sp!` generates](#what-define_sp-generates) for the full
picture.

### 2. Insert `invoke!` calls

At the points in your production code where tests need to hook in:

```rust
shadow_point::invoke!(MyModuleSp, before_insert(&key));
// ... do the insert ...
shadow_point::invoke!(MyModuleSp, after_commit());
```

`invoke!` takes the entry-point struct and a plain `hook(args…)` call.
The struct's name is the `define_sp!` `prefix` with `Sp` appended —
`prefix MyModule` generates `MyModuleSp` (every generated name derives
from the prefix that way; see
[What `define_sp!` generates](#what-define_sp-generates)). The arguments
in the hook call are exactly the *declared* ones — without the guarded
`&T`: dispatch prepends it to the closure parameters, so
`invoke!(MyModuleSp, before_insert(&key))` fires the hook declared
`before_insert(key: &K)` as the closure `|data, key|`. It expands to a
*statement*
(an `#[cfg(test)]`-attributed block), so it cannot be used in
expression position — not as a closure's expression body
(`|| invoke!(…)`), not as a block's trailing value; write `invoke!(…);`
as its own statement or hoist such fires into a function.
In production `invoke!` compiles to nothing — the `#[cfg(test)]`
block is stripped.

### 3. Write a test (single thread)

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn insert_triggers_hook() {
        let data = setup();
        let guard = MyModuleSp::install_guard(data);

        guard.before_insert(|data, key| {
            assert_eq!(key, &42);
        }).expect(1);

        guard.insert(42, "hello");
    }
}
```

No `use` imports needed — hook names are associated constants on the
entry-point struct.

Nothing in the example beyond `install_guard` and the registration
methods is shadow-point API: `setup()` is yours, and
`guard.insert(42, "hello")` is *your* `T::insert` — the guard derefs to
`&T`, so the guarded value's own methods are callable through it.

This is the private mode: the guard sees only fires from this thread.
For hooks fired from several threads there are two ways to go — per-worker
private guards when each thread's contract is what you assert, or one
`install_shared` sync point when the cross-thread order/counts *are* the
assertion (see the pattern table and
[Shared mode](#shared-mode-one-sync-point-across-threads)). Hook
declarations and `invoke!` calls are identical in all of them.

## Using as a dev-dependency

As a *regular* dependency, `invoke!` compiles away in production but the
crate is still linked. To drop that coupling too, declare shadow-point under
`[dev-dependencies]`. The catch: a dev-dependency is only in the extern
prelude when building tests, so any plain `shadow_point::...` path in a
non-test build fails to resolve (`E0433`) — even inside `invoke!`, whose own
body is `#[cfg(test)]`-gated, because the *invocation path* is resolved
before the macro expands and cfg-strips.

The fix is a *seam*: a locally-defined `macro_rules!` that hides the crate
path behind a `#[cfg(test)]` statement. In production builds the seam
expands to a statement that is stripped before `::shadow_point` is ever
resolved — the crate is not referenced at all:

```rust
// src/sp.rs — plain macro_rules; nothing here depends on the crate
macro_rules! sp_invoke {
    ($($t:tt)*) => {
        #[cfg(test)]
        ::shadow_point::invoke!($($t)*);
    };
}
```

```rust
// src/lib.rs — textual macro scope: the seam module must precede every
// module that calls sp_invoke! (an explicit `use` in each module works too)
#[macro_use]
mod sp;
```

Call sites then drop the crate path entirely:
`sp_invoke!(MyModuleSp, before_insert(&key));`. `define_sp!` needs no seam —
it is already `#[cfg(test)]`-gated by the caller and only ever expands in
test builds.

Check the seam holds with `cargo build` (must compile while the dev-dep is
unresolvable) and `cargo test` (must dispatch to the real hooks).

## What `define_sp!` generates

Given `prefix MyModule`, the macro generates:

| Name | What |
|---|---|
| `MyModuleSp` | Entry-point struct — `install_guard()`, `install_shared()`, `with_dyn()`, associated hook constants |
| `MyModuleSyncPoint` | Extension trait (supertrait: `SyncPoint`) |
| `MyModuleHook` | Enum with one snake_case variant per hook |
| `MyModuleSpGuard<T>` | Guard — `Deref<Target=T>`, hook registration, `Drop` |
| `MyModuleSharedSp<T>` | Shared state — same registration API, `install()` per worker |
| `MyModuleSharedGuard<T>` | Per-thread TLS install of a shared sync point |
| `MyModuleSeqBuilder<T>` | Builder for `sequence(...)` |
| `EveryBuilder<T>` | Builder for `every(...)` |
| `SpExpect<'_, T>` | Return value of a fire-once registration — chain `.expect(n)` |

Associated constants on the entry-point struct let you reference hooks
without importing the enum:

```rust
MyModuleSp::before_insert   // type: MyModuleHook
MyModuleSp::after_commit    // type: MyModuleHook
```

The visibility token before `prefix` applies to every generated item
listed in the table above — the trait, the enum, the structs, and the
associated hook constants; the internal machinery (`__Sp`,
`SeqEntry`, the thread-local, …) stays private.

One `define_sp!` per module. Besides the prefixed names in the table,
the macro emits a set of names that do not derive from the prefix: the
`use HookId` import, the module-private machinery (`__Sp`, `__SpDefault`,
`__SP_DEFAULT`, `__SP_TL`, `SeqEntry`, `EveryClosures`), and two
un-prefixed types that carry the visibility token — `EveryBuilder` and
`SpExpect` (both already in the table above). Two sync points generated
side by side in one module collide on each of those names — E0252 for
the duplicate `HookId` import, E0428 for each redefined name, then a
cascade as the second invocation binds to the first one's types:
E0034 (multiple applicable items), E0592 (duplicate impls),
E0119/E0308 (conflicting impls, mismatched types).

The usual layout avoids the issue by construction — one sync point per
file, declared at the top of the production module it instruments. When
two must share a file, separate modules give separate scopes, and that
is the fix — each `define_sp!` in its own `mod { … }`:

```rust
#[cfg(test)]
mod writer_sp {
    shadow_point::define_sp! { pub(crate) prefix Writer { commit(), } }
}

#[cfg(test)]
mod reader_sp {
    shadow_point::define_sp! { pub(crate) prefix Reader { read(), } }
}
```

Each namespace exports its own entry point (`WriterSp`, `ReaderSp`);
dispatch, guards, and `.expect(n)` behave exactly as with a single
declaration — the prefix already keeps every public API name distinct;
only the prefix-free ones collide.

Because the entry struct now lives inside its submodule, `invoke!` call
sites *outside* it must name the scoped path — `invoke!(writer_sp::WriterSp,
commit())` (a `use writer_sp::WriterSp;` works too); inside the module the
bare name still resolves. The path is written in production code but only
resolves in test builds, where the `#[cfg(test)]` submodule exists — in
production the `invoke!` body is cfg-stripped before the path is ever
resolved.

Every generated type carrying `T` (`MyModuleSpGuard<T>`,
`MyModuleSharedSp<T>`, the builders) requires `T: Send + Sync + 'static`,
and `install_guard` / `install_shared` carry the same bounds: the state
is leaked (`Box::leak`) and may cross threads. Registered closures must
be `Send + 'static` (`every` closures additionally `Sync`).

`MyModuleHook` derives `Debug, Clone, Copy, PartialEq, Eq`; the hook
list must contain at least one hook.

### The extension trait: generic code over a sync point

`MyModuleSyncPoint` has one method per hook (taking the declared
arguments) with a default no-op body; its supertrait `SyncPoint` is a
`Send + Sync` marker. The thread-local dispatch holds
`&'static dyn MyModuleSyncPoint`, and a thread without an install is the
default no-op impl — which is why firing a hook on an uninstalled thread
is always safe. Concrete impls are generated internally; you interact
with the trait through `&dyn MyModuleSyncPoint`, which lets you write
test helpers generic over the sync point:

```rust
/// Works on the installed state, on the default no-op impl, or on any
/// `&dyn MyModuleSyncPoint` your own code passes around.
fn probe_insert(sp: &dyn MyModuleSyncPoint, key: &K) {
    sp.before_insert(key);
}
```

### `with_dyn`: manual dispatch

`MyModuleSp::with_dyn(f)` runs `f(&dyn MyModuleSyncPoint)` with the sync
point currently installed **on the calling thread** — `invoke!` is
exactly this:

```rust
MyModuleSp::with_dyn(|sp| sp.before_insert(&key));
// identical to: invoke!(MyModuleSp, before_insert(&key));
```

With no install on the thread, `f` receives the default no-op impl, so
the call is safe and does nothing. Like `invoke!`, `with_dyn`
dispatches to the calling thread's install — a private guard is seen
only by its own thread — and the method carries the `define_sp!`
visibility token. It exists only where `define_sp!` exists — in test
builds.

The `MyModule*` names above derive from the prefix via `paste!`;
`EveryBuilder` and `SpExpect` are shared machinery — unlike the `MyModule*`
items their names do not derive from the prefix, which is why they (like
the private names above) limit each module to a single `define_sp!`
invocation.

## Guard API (private mode)

`install_guard(value)` returns a guard that:
- **Derefs** to `&T` — call methods on the value directly through the guard.
- **Registers closures** for hooks.
- **Auto-counts** every hook fire.
- **Asserts on drop** — unconsumed sequence entries and count mismatches
  panic. Entries marked `.optional()` are exempt (see below).
- **Leaks per-install state** (`Box::leak` by design — normal for test
  instantiation, but do not call `install` in a long-lived loop). Applies
  to `install_shared` too.
- **Requires `T: Send + Sync + 'static`** — and registered closures must
  be `Send + 'static` (`every` closures additionally `Sync`): the state
  is leaked and may cross threads.
- **Restores on drop** — the guard binds this thread's dispatch for its
  lifetime and, on drop, restores the previously installed sync point
  (stacked installs unwind LIFO; after the drop, fires on this thread
  hit the previous install or the default no-op). Drop the guard on the
  thread that installed it: dropping a guard that was moved to another
  thread clobbers that thread's dispatch pointer.

### Registration precedence and lifetime

One `__Sp` state machine sits behind both install modes, so one
dispatch order governs every fire in private and shared mode alike. A
fire that survives re-entry suppression (a hook of the sync point
already executing on this thread is dropped before it counts) runs
through it:

1. **Count + trace.** The fire takes its zero-based per-hook index
   first. The counter lives on the sync point itself — one counter
   per hook per install, shared by all threads bound to it, not
   per-thread; this is the number `SP_TRACE` prints (1-based).
2. **`every`.** If registered, its closure runs now — before the
   sequence head is consulted — on every non-suppressed fire, consumed
   or not, and even on a fire that goes on to panic (ordering
   violation) or to park. That pre-head ordering is what lets an
   `every` closure prove "inside dispatch, about to park" in the
   parking example below.
3. **Sequence.** The deque is checked next:
   - empty deque (none registered, or fully consumed): fall through
     to fire-once;
   - head expects this hook and its predicate passes (or there is
     none): the entry is popped, waiters are notified, and the step
     closure runs — the fire is done;
   - head expects this hook but its predicate fails: the entry stays
     at the head for a later fire; this fire ends here and does not
     arm fire-once;
   - head is a *different* hook:
     - shared mode and the head is marked `optional`: the head is
       skipped (its closure never runs) and the check continues with
       the next entry;
     - no later entry matches this hook: fall through to fire-once;
     - a later entry matches: private mode panics with the ordering
       violation; shared mode parks on the sequence condvar and
       re-checks after every wake (see Parking) — a parked fire
       never arms fire-once while the head still blocks it.
4. **Fire-once** runs at most once per hook, on the fire where this
   hook's counter was 0 *and* the sequence fell through to it (the
   fall-through cases above). If the hook's first fire was consumed
   by a `sequence` entry or gated off by a failing predicate, the
   fire-once closure never runs.

Panic behavior: a panic inside a hook closure propagates from the
`invoke!` site like any other panic and unwinds the instrumented
operation. The state is unwind-safe — the consumed entry stays
consumed, the fire stays counted, and every lock recovers from poison,
so other threads keep running and reporting their own diagnostics. On
the test thread, guard/`Arc` drop assertions run during unwinding and
may panic on top of the first one — a double panic aborts the process;
the first message is the real failure.

- One fire-once slot per hook: a second registration for the same hook
  replaces the first (last wins).
- `.expect(n)` and `expect_calls(hook, n)` write the same counter
  expectation; for the same hook the last call wins, whatever form it
  takes.
- A second `sequence(...)` call replaces the whole deque — it does not
  append; likewise a second `every(...)` replaces all closures,
  including hooks it does not mention.

### Fire-once

Run a closure on the first call of a hook:

```rust
guard.before_insert(|data, key| {
    data.clear();
}).expect(1);
```

`.expect(N)` asserts the hook fires exactly N times — it checks the
counter for *all* fires, not only for closure runs. Without `.expect()`,
the closure runs once but no count is checked (on_first semantics).
Fire-once and its interaction with `sequence` registrations follow the
dispatch order above.

### Sequence

Enforce a strict ordering of hook fires. Builder methods return `&mut Self`
for chaining:

```rust
guard.sequence(|s| {
    s.before_insert(|data, key| { data.clear(); })
     .after_commit(|data| { assert!(data.is_empty()); });
});
guard.expect_calls(MyModuleSp::after_commit, 1);
```

Each `s.hook(closure)` pushes an entry. On fire, the front entry must match
the hook — otherwise panic with an ordering violation message. A second
`sequence(...)` call replaces the whole deque, it does not append (see
Registration precedence and lifetime above).

Extra fires (after the sequence is consumed) are silently ignored — an
empty deque makes the fire fall through to fire-once, and with no
fire-once pending it just counts.

### Predicate-gated entries

`s.hook_when(pred, closure)` pushes an entry that consumes only when the hook
fires with arguments that pass `pred`. The predicate receives the hook
arguments (without the guarded `&T`):

```rust
guard.sequence(|s| {
    s.before_insert_when(
        |key| is_significant(key),
        |data, key| { /* runs only on significant fires */ },
    );
});
```

Fires that fail `pred` leave the entry at the front of the sequence waiting
for a later fire; they also do not trigger fire-once closures for that hook
(the pending head already expects it — see Registration precedence and lifetime).
Ordering between different hooks stays strict — the gate only filters fires
of its own hook by arguments.

The predicate runs while the sequence lock is held — in *both* modes, not
only shared (see the shared-mode caveats): it must be pure, with no hook
invocations and no blocking. A blocking predicate deadlocks the fire even
in private mode.

A gated entry must still be consumed: if it remains at guard drop, the
"sequence not fully consumed" assertion fails, so a scenario whose gate
never passes cannot complete silently. If an entry may legitimately never
fire, mark it optional:

```rust
guard.sequence(|s| {
    s.before_insert(|data, key| { /* expected step */ });
    // Trailing sentinel: panics only if an unexpected second attempt
    // happens; may stay unconsumed in the normal scenario.
    s.before_insert_when(
        |key| is_significant(key),
        |_data, _key| panic!("unexpected second attempt"),
    ).optional();
});
```

`optional()` marks the most recently pushed entry (plain or gated); it may
remain unconsumed at drop without failing the assertion. It panics if no
entries have been pushed yet.

### Every

Run a closure on every fire of a hook (pre-condition, invariant). Builder
methods return `&mut Self` for chaining:

```rust
guard.every(|e| {
    e.before_insert(|data, _key| {
        assert!(!data.is_full(), "insert into full data");
    })
    .after_commit(|data| {
        assert!(data.is_consistent(), "invariant violated");
    });
});
```

### Call counts (`expect_calls`)

Every fire is counted per hook — registered or not, consumed by a
sequence or not (re-entrant suppressed fires are the only exception:
they are dropped before counting). `guard.expect_calls(hook, n)`
declares a hook's total count without registering any closure; it works
for hooks you never registered too — `expect_calls(hook, 0)` asserts
the hook never fired. `.expect(n)` chained on a fire-once registration
sets the same counter expectation; `expect_calls` exists to declare
counts independently of closures (see the `expect_calls` line in the
Sequence example above). A later call for the same hook replaces the
earlier expectation — including a `.expect()` chained onto a fresh
fire-once registration (last write wins).

In shared mode the count is aggregated across all bound threads and
asserted at the last `Arc` drop instead (see
[Example: counting fires across threads](#example-counting-fires-across-threads)).

See **Registration precedence and lifetime** above for the full dispatch
order: what `every`, `sequence`, and fire-once each see, and in what
order.

### What you can test

Hooks turn rare thread interleavings into deterministic, reproducible
scenarios: at the exact linearization point, the closure runs the operation
that the *other* thread would have run. Each example below assumes a
*concurrent* map — one with interior mutability, mutating through
`&self` — with the hook set from the Quick start (the third also declares
`before_get_search(root: *const ())`). The examples use the private
guard; in shared mode the same interleavings are scripted as `sequence`
steps, coordinated with `Gate`.

Note: while a hook closure is executing on a thread, any further fire of
the *same sync point* on that thread is silently suppressed (suppression
is per-thread, per-sync-point) — so the interleaving operation inside the
closure runs without recursing into its own hooks.

#### The inserting thread loses the race

Between the map's lookup ("key 1 is free") and its write, sneak in the
*same* key. The outer insert must now report a duplicate instead of
committing on top of the winner:

```rust,ignore
#[test]
fn insert_race_loser() {
    let guard = MyModuleSp::install_guard(my_map());

    guard.before_insert(|map, key| {
        // Lookup already passed; the commit has not. Steal the slot.
        map.insert(*key, "rival");
    }).expect(1);

    // The outer insert now hits a taken key — it must return Err,
    // not overwrite the value the hook just wrote.
    assert!(guard.insert(1, "mine").is_err());
    assert_eq!(guard.get(&1), Some(&"rival"));
}
```

#### The key vanishes under an in-flight remove

The remove has resolved the bucket and is about to unlink the entry. Clear
the map at that instant: the outer remove must return `false` — and, if it
retires the entry it no longer owns, that is the use-after-free the test
exists to catch.

```rust,ignore
#[test]
fn remove_after_clear() {
    let guard = MyModuleSp::install_guard(my_map_with(&[(1, "a"), (2, "b")]));

    guard.before_remove(|map, _key| {
        // The entry the outer remove is about to unlink no longer exists.
        map.clear();
    }).expect(1);

    assert!(!guard.remove(1));    // nothing left to remove
    assert!(guard.is_empty());
}
```

#### A reader pinned a node the remover detaches

The get has pinned the root and is mid-search when the node it holds is
removed. If the implementation trusts the pinned root without revalidating,
it returns a value from retired memory — this is exactly the bug class hook
tests exist for:

```rust,ignore
#[test]
fn get_survives_detach_underneath() {
    let guard = MyModuleSp::install_guard(my_map_with(&[(1, "a")]));

    guard.before_get_search(|map, _root| {
        // Fires while the get holds a pinned root: detach and retire it.
        // The nested remove's own hooks are suppressed on this thread.
        map.remove(1);
    }).expect(1);

    // The get must revalidate against the new root and report absence —
    // never resurrect the value from the retired node.
    assert_eq!(guard.get(&1), None);
}
```

## Hook arguments

Hooks return nothing: the generated trait methods have no return type,
and `invoke!` expands to a statement. A hook closure can observe and
mutate through `&T` (and assert), but cannot change what the
instrumented operation returns.

Hook arguments appear in the trait signature and are passed *by value*
through the dispatch: for a non-`Copy` type, `invoke!` moves the
argument, so declare the hook with a reference (`key: &K`) or a raw
pointer (`root: *const ()`) when the call site must keep the value.
The closure receives `&T`
(the guarded value) as the first parameter, followed by the hook arguments:

```rust
// Hook declaration: before_insert(key: &K)
// Closure signature: |data: &T, key: &K|
guard.before_insert(|data, key| { ... });
```

For `*const ()` arguments (used when the hook receives a raw pointer),
cast inside the closure:

```rust
guard.before_get_search(|_map, root| {
    let node = root as *mut Node<K, V>;
    // ... use node ...
});
```

## Per-worker guards: private mode on real threads

Pattern 2 runs the private mode across several real threads: *each*
worker installs its own `install_guard` as its first act, registers its
hooks there, and lets the guard's Drop assert *that thread's* counts.
No shared state, no parking. Cross-thread ordering is not the macro's
job — you hold it in your own `Gate`s (or atomics/channels) in the
test body, exactly as you would without shadow-point at all.

Choose it when:

- each worker's hook behavior is a **local contract** (what fires,
  with what arguments, how many times, on this thread);
- workers must not overlap at specific points, and you can express
  that with plain gate handoffs;
- you want Drop-time `expect` checks per thread without coordinating
  them through the shared last-`Arc` drop;
- a shared `sequence` would park a worker **inside** the instrumented
  path while it holds locks the head consumer needs (see shared-mode
  caveats) — per-worker guards never park in the macro.

The trade: the macro never sees across threads. A wrong global order
your own gates failed to enforce will not be caught by a sequence
assertion; per-thread hooks can use `expect` precisely because each
thread's log is complete on its own.

```rust,ignore
use shadow_point::{Gate, PARK_TIMEOUT};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

/// The instrumented API (stands in for the real one under test): its
/// `invoke!` sites dispatch into whichever guard is bound on the
/// calling thread.
fn real_insert(key: i32) {
    shadow_point::invoke!(MyModuleSp, before_insert(&key));
    // ... the actual insert happens here ...
    shadow_point::invoke!(MyModuleSp, after_commit());
}

#[test]
fn t1_inserts_before_t2_under_gate_choreography() {
    let seen = Arc::new(AtomicUsize::new(0));
    // Cross-thread ORDER is enforced by the test's own gates, not by a
    // shared sequence. `first_done` proves T1's before_insert completed.
    let first_done = Arc::new(Gate::new());

    let seen_t1 = seen.clone();
    let done_t1 = first_done.clone();
    let t1 = std::thread::spawn(move || {
        // Each worker binds its OWN private guard: hooks registered
        // here fire only on this thread, and Drop asserts this thread's
        // counts.
        let guard = MyModuleSp::install_guard(());
        guard
            .before_insert(move |_, key| {
                assert_eq!(key, &1, "T1 inserts first");
                seen_t1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                done_t1.set(); // completion signal for T2's turn
            })
            .expect(1); // T1's own hook count, checked at this guard's Drop
        real_insert(1); // the instrumented API fires this thread's hooks
    });

    let seen_t2 = seen.clone();
    let wait_t2 = first_done.clone();
    let t2 = std::thread::spawn(move || {
        let guard = MyModuleSp::install_guard(());
        guard
            .before_insert(move |_, key| {
                assert_eq!(key, &2, "T2 inserts second");
                seen_t2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .expect(1);
        // Ordered *after* T1's hook completes — the gate does what a
        // shared sequence's parking would, without shared state.
        wait_t2.must_wait(PARK_TIMEOUT); // fails loudly if T1 never fired
        real_insert(2);
    });

    t1.join().unwrap(); // each guard — and its expect(1) — drops in-thread
    t2.join().unwrap();
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
}
```

One trap is unique to this pattern: a worker thread that installs
nothing fires nothing — its hooks silently no-op. The guarded value can
equally be an `Arc` shared between workers; what must *not* be shared
is the guard, which is per-thread by construction.

The same TLS rule bites async runtimes: dispatch follows the *thread*,
not the task — a future resumed on a different OS worker after an
`.await` fires into whatever *that* thread has installed (usually
nothing: the fires vanish silently, in both modes). Keep the
instrumented section free of awaits between install and the last
fire — or run it through `spawn_blocking` and install inside the
closure, where the OS thread is yours.

## Shared mode: one sync point across threads

A private guard sees only the installing thread's fires. When the
assertion is *cross-thread* — the global order of interleaving steps, or
total counts across workers — install one sync point instead:
`install_shared(value)` returns an `Arc<MyModuleSharedSp<T>>` whose state every
bound thread drives. The `Arc` derefs to `&T`, so the shared value is
readable through it everywhere. This mode adds real parallelism:
workers race for real, and `sequence` + `Gate` pin the chosen
interleaving down so it reproduces run to run.

### Lifecycle — six steps, in this order

Each step exists to make the next one deterministic; do not reorder:

1. **Install** — `install_shared(value)` on the test thread →
   `Arc<MyModuleSharedSp<T>>` (`T: Send + Sync + 'static`, as with
   `install_guard`).
2. **Register** — `sequence` / `every` / fire-once / `expect_calls` on
   the `Arc`, *before spawning workers*. Registration is lock-guarded
   and may safely happen from any thread at any time, but a fire that
   beats it falls through to the fire-once/counter path instead of
   your entry — silently — so the discipline is what keeps the
   scenario deterministic.
3. **Bind each firing thread** — clone the `Arc` into the thread; its
   first act is `let _g = shared.install();`. TLS is per-thread: this
   points the *current* thread's dispatch at the shared state — and
   *every* thread that fires must bind this way, the test thread
   included. The returned `MyModuleSharedGuard<T>` owns an `Arc` clone
   of the shared state, so the step-6 assertions cannot run while any
   worker guard is alive. Keep the guard alive
   for as long as the worker touches instrumented code; once it drops,
   the thread's dispatch falls back to the previously installed sync
   point (usually the default no-op). Drop the guard on the thread that
   installed it — moving it across threads clobbers the destination
   thread's dispatch pointer, exactly as in private mode.
4. **Drive** — workers call the real API; its `invoke!` sites dispatch
   into the shared sequence. A worker whose hook matches only a *later*
   entry **parks** (see Parking).
5. **Join every worker.**
6. **Drop the last `Arc`** — the shared assertions (unconsumed sequence
   entries, `expect_calls` mismatches) run there. After the joins, so
   they cannot race an in-flight fire.

A worker that never calls `install()` dispatches to the default no-op
and its fires vanish silently — that is the shared-mode version of the
wrong-mode mistake. Threads that only wait on gates and never fire hooks
do not need to bind.

### Parking: the sequence head is a rendezvous

A worker that fires a hook the sequence expects only at a *later*
position parks instead of panicking, and re-checks the head after
every wake. The panic fires only when a full `PARK_TIMEOUT` (a
crate-root `Duration` constant of 10 s, not per-install configurable)
elapses with no wake and the head still blocks the fire — a thread
that keeps getting woken can park longer than 10 s in total. This is what makes shared mode
composable: *any* thread may arrive out of order, and the sequence
sorts arrivals — each same-hook entry is consumed exactly once, the
thread that fires the head hook pops it and wakes the next in line.
Fires matching no entry at all never park; they fall through (see
Registration precedence and lifetime).

Parking is a mutual-deadlock guard, **not** a synchronization
mechanism: if the head can never advance (no live thread will fire it),
the parked worker panics at `PARK_TIMEOUT` with a `sequence park
timeout` diagnostic naming the waiting hook, the head hook, and the
install site. The test fails loudly; it does not hang.

`Gate::must_wait` / `wait_timeout` take any `Duration` — the examples
reuse `PARK_TIMEOUT` merely as a convenient budget.

### `Gate`: coordinating completion, not just consumption

`sequence` orders *consumption* of entries — a thread is released the
moment its entry pops, while its closure may still be running, and its
step may still hold production locks. To wait for a step to *finish*,
coordinate with `Gate`, a level-triggered boolean (`set` before `wait`
is not lost; `clear` re-arms it):

| Call | Semantics |
|---|---|
| `new()` / `Default` | Fresh unset gate |
| `set()` / `clear()` | Raise / lower the flag; wakes all waiters |
| `must_wait(timeout)` | Block until set; **panic** on timeout — the in-hook default |
| `wait_timeout(timeout)` -> `bool` | Block until set; report whether it happened |
| `wait()` / `is_set()` | Unbounded block (test body only) / read the flag |

Inside a step closure, `must_wait` — never `wait`: a missed `set` must
fail with a diagnostic, not hang the suite.

### Example: scripting a cross-thread order

A writer inserts, a committer commits — *after* the insert, never
before. T2's gate wait sits outside the instrumented path (it parks
before firing, so it holds no production locks):

```rust,ignore
use shadow_point::{Gate, PARK_TIMEOUT};
use std::sync::{Arc, Mutex};

#[test]
fn commit_never_overtakes_insert() {
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let inserted = Arc::new(Gate::new());

    let shared = MyModuleSp::install_shared(()); // step 1
    shared.sequence(|s| {                        // step 2: register first
        s.before_insert({
            let (log, inserted) = (log.clone(), inserted.clone());
            move |_, key| {
                log.lock().unwrap().push("insert");
                assert_eq!(key, &42);
                inserted.set(); // release the committer once consumed
            }
        });
        s.after_commit({
            let log = log.clone();
            move |_| log.lock().unwrap().push("commit")
        });
    });

    let (s1, s2) = (shared.clone(), shared.clone());
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();                   // step 3: bind this thread
        // ... the real op that fires the hook internally ...
        shadow_point::invoke!(MyModuleSp, before_insert(&42));
    });
    let t2 = std::thread::spawn({
        let inserted = inserted.clone();
        move || {
            let _g = s2.install();
            inserted.must_wait(PARK_TIMEOUT);    // ordered *behind* insert
            shadow_point::invoke!(MyModuleSp, after_commit());
        }
    });

    t1.join().unwrap();                          // step 5
    t2.join().unwrap();
    drop(shared);                                // step 6: asserts run here
    assert_eq!(*log.lock().unwrap(), ["insert", "commit"]);
}
```

### Example: parking is the rendezvous

The same machinery works when you cannot gate the fire site. Here T2
fires `after_commit` *out of order, on purpose*: it parks at the
sequence head (`before_insert`), and that hook's eventual fire wakes
it. One twist the parking itself does not give you: consumption order
is not *completion* order — the woken thread can run its closure while
the popped entry's closure is still executing. So step `a` ends by
setting `insert_done` and step `b` waits on it; the log order becomes
deterministic. The `every` closure — which runs **before** the head
check — records that T2 is inside its dispatch, so the test releases
T1 only once the park is determinate:

```rust,ignore
use shadow_point::{Gate, PARK_TIMEOUT};
use std::sync::{Arc, Mutex};

#[test]
fn out_of_order_fire_waits_its_turn() {
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let parked = Arc::new(Gate::new());      // T2 proved to be in dispatch
    let go = Arc::new(Gate::new());          // release for T1
    let insert_done = Arc::new(Gate::new()); // completion, not consumption

    let shared = MyModuleSp::install_shared(());
    shared.sequence(|s| {
        s.before_insert({
            let (log, insert_done) = (log.clone(), insert_done.clone());
            move |_, _| {
                log.lock().unwrap().push("insert");
                insert_done.set(); // after the push: `b` may now complete
            }
        });
        s.after_commit({
            let (log, insert_done) = (log.clone(), insert_done.clone());
            move |_| {
                insert_done.must_wait(PARK_TIMEOUT);
                log.lock().unwrap().push("commit");
            }
        });
    });
    shared.every(|e| {
        let parked = parked.clone();
        // `every` runs before the head check: set = "inside dispatch,
        // about to park at the head" — not "asleep".
        e.after_commit(move |_| parked.set());
    });

    let (s1, s2) = (shared.clone(), shared.clone());
    let t1 = std::thread::spawn({
        let go = go.clone();
        move || {
            let _g = s1.install();
            // Block BEFORE firing: the head must still be `before_insert`
            // when T2 dispatches, or the rendezvous is racy.
            go.must_wait(PARK_TIMEOUT);
            shadow_point::invoke!(MyModuleSp, before_insert(&42));
        }
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(MyModuleSp, after_commit()); // early: parks
    });

    parked.must_wait(PARK_TIMEOUT); // T2 is provably at the head...
    go.set();                       // ...now let `before_insert` pop it
    t1.join().unwrap();
    t2.join().unwrap();
    drop(shared);
    assert_eq!(*log.lock().unwrap(), ["insert", "commit"]);
}
```

The `parked` gate is what makes this run the *park* path: without it
(releasing T1 right away) the test still passes — T2 may simply consume
`after_commit` in order and never park — so the choreography would
silently stop exercising the rendezvous. The failure mode the gate
guards against is a head that never advances: if `before_insert` never
fires, T2's park ends in a `sequence park timeout` panic at
`PARK_TIMEOUT`. A wrong choreography fails the test; it never hangs it.

### Example: counting fires across threads

When order is not the assertion — only "every thread reached the point"
— `expect_calls` aggregates across threads and checks at the last `Arc`
drop. Pair it with `every` to observe each fire without sequencing:

```rust,ignore
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

#[test]
fn all_workers_reach_the_hook() {
    let seen = Arc::new(AtomicUsize::new(0));
    let shared = MyModuleSp::install_shared(());
    shared.expect_calls(MyModuleSp::before_insert, 4); // 2 workers x 2
    shared.every(|e| {
        let seen = seen.clone();
        e.before_insert(move |_, _| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
    });

    let (s1, s2) = (shared.clone(), shared.clone());
    let t1 = std::thread::spawn(move || {
        let _g = s1.install();
        shadow_point::invoke!(MyModuleSp, before_insert(&1));
        shadow_point::invoke!(MyModuleSp, before_insert(&2));
    });
    let t2 = std::thread::spawn(move || {
        let _g = s2.install();
        shadow_point::invoke!(MyModuleSp, before_insert(&3));
        shadow_point::invoke!(MyModuleSp, before_insert(&4));
    });
    t1.join().unwrap();
    t2.join().unwrap();
    // Drop last — the count assertion runs there; reading `seen` before
    // the joins would race the workers.
    drop(shared);
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 4);
}
```

Two same-hook entries in a `sequence` behave the same way — each is
consumed exactly once, but *which* thread consumes *which* is not fixed;
if the test must tell the fires apart, carry an id in the hook argument
and match it with a `s.<hook>_when(pred, closure)` entry (see
Predicate-gated entries).

### `sequence` vs `every` + trace: choosing your assertion

Shared mode offers two assertion shapes, and picking wrong costs you
either flakiness or a parking hazard:

- **`sequence`** asserts *global order* — the strong property. Cost: an
  out-of-order arrival parks **inside the instrumented path**, with
  production locks held (see Caveats). Also unusable when legitimate
  fire counts are unbounded by the scenario (retry loops, background
  churn): every extra in-order fire still consumes an entry.
- **`every` + trace**: `every` closures append to a
  `Mutex<Vec<Event>>` (the log above is a one-word trace); assert on the
  trace **after joining every worker**, while it is immutable — never
  with `expect`-style drop assertions. Cost: no strict order from the
  macro (append order is arrival order under the mutex); you assert
  membership, counts, and *causal pairs* — "`after` appears at least
  once after the first `before`" is assertable and survives any scheduling —
  and coordination still needs `Gate` (`every` proves "reached", the
  gate proves "now").

Default to `sequence` when the order IS the bug class (the "parking is
the rendezvous" example above). Reach for the trace when the scenario
churns unboundedly, when parking inside the instrumented path would hold
locks the head consumer needs, or when you only need reachability
("this decision site was reached, and the state was X when it was").

Caveats:

- A parked thread is inside the instrumented code path and holds
  production locks: order `sequence` entries so the head consumer never
  needs a lock held by a parked thread (otherwise `sequence park
  timeout`, not a hang).
- Fire-once closures are nondeterministic in shared mode (the winner of
  the call-counter race); use `sequence` for deterministic ordering.
- Register before spawning workers; early fires fall through to
  fire-once/counter.
- Same-named entries from different threads are each consumed exactly
  once, but wake order is not FIFO.
- In shared mode, an `optional` entry at the head is skipped — its
  closure never runs — when any *other* hook fires while it is at the
  head; in private mode the mismatch leaves the entry in place, forgiven
  at drop.
- Join workers and drop their guards before dropping the last `Arc`.
- Inside step closures use `Gate::must_wait`, never `Gate::wait`: a
  missed `set` fails the test with a diagnostic instead of hanging it.
- Predicates on gated entries must be pure — they run while the sequence
  lock is held (no hook invocations, no blocking).

The examples above follow [`tests/cross_thread.rs`](tests/cross_thread.rs):
the parking and counting examples are transcriptions of `park_until_turn`
and `expect_calls_aggregates_threads` with the domain names swapped; the
first example is a simplified variant of `sequence_order_across_threads`
(two steps, gate wait moved out of the instrumented path) — run the
real tests with `cargo test --test cross_thread`.

## Async (tokio) consumers

On a `current_thread` tokio runtime, the single executor thread must
never park — the wake it is waiting for can never run because it *is*
the executor thread. That makes `Gate::wait` unusable inside or
alongside async choreography: parking the executor stalls all work
until a timeout panic. (The existing async / TLS caveat in
[Per-worker guards](#per-worker-guards-private-mode-on-real-threads) already warns that the
instrumented section under a guard on the tokio runtime must stay
await-free — or the worker must use `spawn_blocking` + install inside
the closure.)

`TokioAsyncGate` — behind the non-default `tokio-async` feature — is the async
complement to `Gate`: a monotonic fire-counter that the test body
waits on cooperatively via `.await`. Call `fire()` (non-blocking, safe
from hook closures on the executor thread), `count()`, or
`wait_at_least(n).await` from the async test body. `wait_at_least` has
**no timeout by design**: a threshold the scenario never reaches hangs
until the CI job timeout — unlike `Gate::must_wait` and sequence
parking, which fail loudly; await only a count the scenario guarantees
to fire. The type is
`Clone + Default`. Default builds never resolve tokio — the feature
is off by default:

```toml
# Cargo.toml
[dependencies]
shadow-point = { version = "0.3", features = ["tokio-async"] }
```

The feature depends on tokio (specified as ≥ 1.21 in Cargo.toml; `Notified::enable` needed by `wait_at_least` shipped in 1.19, so the manifest floor is conservative). Tokio ≤ 1.38 requires Rust ≥ 1.63, which is satisfied by the crate's own MSRV 1.65. Newer tokio (≥ 1.39) requires Rust ≥ 1.70 — if Cargo resolves a version past that boundary while the toolchain is below it, the build fails. Pin tokio in your lockfile or bump the toolchain. Enable `tokio-async` as a **dev**-dependency in your project — production builds keep the seam-cfg-stripped zero-cost property.

Concretely: if shadow-point is only a dev-dependency, add
`features = ["tokio-async"]` to that entry; if it is also a regular
dependency (the seam setup), keep that entry untouched and add a second
`[dev-dependencies]` entry with the feature — Cargo unifies the feature
flags of both entries for test builds.

**Example** (simplified; the full tests live in
[`tests/tokio_async_gate.rs`](tests/tokio_async_gate.rs)):

```rust,ignore
use shadow_point::TokioAsyncGate;
use tokio::runtime::Builder;

#[test]
fn async_milestone_gate() {
    // A shared sync point drives hook closures; the gate coordinates
    // milestones without parking the executor.
    let gate = TokioAsyncGate::new();
    let g = gate.clone();
    let shared = MyModuleSp::install_shared(());

    // Register the hook closure: fire the gate on every invocation.
    shared.every(|e| {
        e.before_remove(move |_, _| g.fire());
    });

    let bound = shared.clone();
    let rt = Builder::new_current_thread().build().unwrap();
    rt.block_on(async move {
        let _guard = bound.install();

        // Await the threshold while firing milestones concurrently.
        let waiting = gate.wait_at_least(2);
        let firing = async {
            shadow_point::invoke!(MyModuleSp, before_remove(0));
            shadow_point::invoke!(MyModuleSp, before_remove(1));
        };
        tokio::join!(waiting, firing);
        assert_eq!(gate.count(), 2);
    });
}
```

For work running on blocking threads (e.g. `tokio::task::spawn_blocking`),
each worker installs the sync point on its own thread (the reference test
uses a shared install; a per-worker private guard works the same) and
fires the gate inside the closure — the async body awaits the threshold.

## Debugging

### SP_TRACE

Set the `SP_TRACE` environment variable to print every hook fire to
stderr (with the firing thread's name — `<unnamed>` for unnamed
threads). Presence is what matters, not the value: any
setting — including `SP_TRACE=0` — enables tracing.

```sh
SP_TRACE=1 cargo test -- --nocapture
```

Output:
```
[sp] before_insert call #1 thread=worker-1
[sp] after_commit call #1 thread=worker-1
[sp] after_commit call #2 thread=main
```

Useful for discovering expected sequences and call counts. The same
check is available programmatically as `shadow_point::trace_enabled()`.

### `current_fire()`

Inside any hook closure — fire-once, a sequence step, or an `every`
closure — `shadow_point::current_fire()` returns the `FireInfo` of the
dispatch running on this thread: the `hook` name, the zero-based
`index` of this fire within its hook's count — a per-sync-point
counter shared by all threads, the same number `SP_TRACE` prints
(1-based) — and the firing thread's `thread_id`/`thread_name`.
Outside a dispatch it returns `None`. Handy for helpers that must know
which hook fired without being told:

```rust
let f = shadow_point::current_fire().expect("inside a hook");
eprintln!("{} #{} on {:?}", f.hook, f.index, f.thread_id);
```

### Panic messages

Drop assertions include the install location:

```
sync point hook `before_insert` fired 1 time(s), expected 2
(installed at src/my_module.rs:142:21)
```

(In shared mode the location is marked `(shared, installed at ...)`.)

A private-mode ordering violation names the expected head and the fire
that arrived:

```
sync point ordering violation: expected `before_insert` next but `after_commit` fired
```

The shared-mode park timeout names the waiting hook, the head hook, and
the install site:

```
shadow-point: sequence park timeout (10s): thread waiting for `after_commit`,
head is `before_insert` (installed at src/my_module.rs:95:35)
```

## Production safety

- `invoke!` compiles to `{}` outside `#[cfg(test)]`.
- `define_sp!` is gated with `#[cfg(test)]` by the caller.
- Zero cost — verified with `cargo build --release`.

## Compatibility

MSRV is Rust 1.65 (edition 2021). The only default dependency is `paste`, used
at macro-expansion time and re-exported by the crate — `define_sp!` reaches it
via `$crate::paste`, so consumers never declare it themselves. Behind the
non-default `tokio-async` feature, optional `tokio` (default-features off,
`rt` + `sync` + `macros` features, ≥ 1.21) is added
— default builds never resolve tokio; tokio ≤ 1.38 is within the crate's
MSRV (tokio 1.38 requires Rust ≥ 1.63), while tokio ≥ 1.39 requires
Rust ≥ 1.70. In test builds a hook fire costs a TLS read plus a few mutex
operations; uninstalled threads dispatch to a no-op impl.

## Loom

There is no loom integration: nothing in `shadow-point` is gated on
`cfg(loom)`, and dispatch runs on std `Mutex`/`Condvar`/`Cell`
throughout. Under `--cfg loom` the macros compile and behave exactly as
in normal test builds — `define_sp!` generates the full infrastructure
and `invoke!` dispatches through `with_dyn`, with guards installing for
real — but loom does not model std synchronization, so the hook-dispatch
path contributes nothing to loom's race detection. What loom still
checks is the instrumented code itself; the sync-point machinery is
invisible to its model.

Modeling the dispatch state with loom analogs is future work.

## Comparison with similar crates

One axis separates the tools: shadow-point **scripts** one chosen
interleaving at a named sync point in real code. The alternatives below
either **inject failures**, **explore schedules**, **replace the
environment** (simulators), **detect whatever race the run happens to
produce**, or **rebuild the API** (mocks, ad-hoc barriers) to force the
interleaving. None of them lets a test say
"run this closure *in place of* the rival operation, on this thread, with
these call arguments".

| tool | what it does | pick it when |
|---|---|---|
| [failpoints (`fail`)](#failpoints-the-fail-crate) | named points with a per-name runtime action: panic, early `return(value)`, sleep, probabilistic `p%` and repeat-limited `cnt*` triggers; per-hit conditionals via `cfg_callback` or a fixed call-site predicate | an injected failure (EIO, crash, sleep) on a composed path is the whole property |
| [loom](#loom-and-shuttle-exploring-schedulers) | exhaustive interleaving exploration of instrumented primitives | you want to *find* which order breaks the code |
| [shuttle](#loom-and-shuttle-exploring-schedulers) | randomized scheduler over replacement thread/sync primitives, with deterministic replay | same, at schedules too large to explore exhaustively |
| [Miri / ThreadSanitizer](#detectors-miri-and-threadsanitizer) | run real (Miri: interpreted) schedules and report the data race or UB they hit | you want to *catch* an unscripted race: Miri for small all-Rust units, TSan for real threads |
| [madsim](#deterministic-simulators) | swaps tokio for a simulated deterministic runtime (tasks, timers, RNG, network) | reproducibility of an entire distributed run is the property, and the runtime swap is acceptable |
| [turmoil](#deterministic-simulators) | deterministic network hardship, every host on one simulated thread | same property, without swapping the runtime |
| [mockall and hand-rolled barriers](#hand-rolled-mocks-and-barriers) | trait doubles, ad-hoc channels at call sites | you accept shipping the test seam in the permanent API (a trait already exists there) |

### Failpoints (the `fail` crate)

A fail point is an *unnamed-in-code, named-by-string* hook from the
[`fail`](https://docs.rs/fail) crate: `fail_point!("wal-fsync")` consults
a global registry configured at runtime (`fail::cfg("wal-fsync", "sleep(10)")`
or the `FAILPOINTS` env var). The action is blind to call arguments; under
plain `fail::cfg` every caller of the name takes it. Narrowing to one
participant takes either a `cfg_callback` — evaluated per fire, but
still global: the callback gets neither the call's arguments nor any
per-thread context for free, so per-participant behavior means
rebuilding that state inside it by hand — or a call-site predicate
(`fail_point!(name, cond, |_| {})`), fixed in source and only usable
where the participant identity is already in scope.
Under the `failpoints` feature the macro is live; with the feature off it
generates nothing, so the instrumentation costs the public API nothing.
That combination — zero API surface, env-controllable, works from
integration tests and released binaries — makes failpoints the right tool
for failure injection: fsync that returns EIO, a crash between two writes,
a sleep that widens a race window.

The global registry is also the tax: fail points are process-wide, and
cargo runs test threads in parallel, so one test's configured action can
fire inside another test's code path — hence the `fail` crate's own
guidance to hold a `FailScenario` lock and move failpoint tests into a
dedicated test binary. shadow-point's state is thread-local and
per-`define_sp!` type, so parallel tests in one binary never see each
other's fires.

A shadow point is the other half of that expressiveness. Hooks are typed
closures that receive the call's arguments, are installed per-thread
(`install_guard` intercepts only the installing thread; `install_shared`
only threads that called `install()`), and can be *conditional*: a
predicate gate, fire-once, a `sequence` that parks an out-of-order arrival
at the rendezvous. Properties like "T1 blocks on its first acquire and T2
does not", "the third call sees the rival already inserted", "count fires
per key" are not writable against a global name-keyed action without
rebuilding that state inside a `cfg_callback`.

The asymmetry is not only in shadow-point's favor: a failpoint acts on
control flow — `panic`, an early `return(value)` from the instrumented
function, `sleep` — where a shadow hook is side-effect only and cannot
change what the call returns ([Hook arguments](#hook-arguments)).

Decision rule:

- The property decomposes into local module invariants → **shadow-point
  unit test**: `#[cfg(test)] define_sp!` in the module's own `mod tests`.
  This is the default and costs nothing — no feature, no public items.
- A global per-name action on the public composed path is enough
  (fsync fails, panic injection on a deterministic single-thread path;
  a `cfg_callback` conditional at most) → **failpoint**; prefer it, the
  API price is zero.
- A *conditional per-thread* stop is needed on a path only reachable
  through the public API → shadow-point, and you pay a feature tax for it
  (a test in `tests/` compiles the lib **without** `cfg(test)`, and
  `invoke!` is `#[cfg(test)]`-gated at the use site): the Sp type and its
  guards go `pub` with `missing_docs` allowances; the declaration moves
  behind your own feature and dispatches through `with_dyn` instead of
  `invoke!` ([Using as a dev-dependency](#using-as-a-dev-dependency));
  and the thread-local guards keep the async caveat
  ([Per-worker guards](#per-worker-guards-private-mode-on-real-threads)) —
  on tokio, `spawn_blocking` + `TokioAsyncGate` ([Async (tokio)
  consumers](#async-tokio-consumers); fire/count/await, no parking)
  rather than parking a guard across an `.await`.

An integration shadow-point seam is therefore a deliberate exception, not
a default: land it with its justification. The two tools coexist happily —
failpoints for the environment misbehaving, shadow-points for the rival
thread behaving in a scripted way.

### Loom and shuttle: exploring schedulers

[loom](https://docs.rs/loom) exhaustively model-checks interleavings of
its own synchronization primitives;
[shuttle](https://github.com/awslabs/shuttle) explores the schedule
space of std-style threads with a randomized (PCT, or bounded-DFS)
scheduler, replaying a failing run deterministically from its recorded
schedule string. Both run the exploration
over their own primitives (`loom::sync`, `shuttle::thread`,
`shuttle::sync`): the code under test must be ported away from the std
types, and anything left uninstrumented is invisible to the exploration.
Both answer "does *some* order break this code?" and find the order for you.
shadow-point answers "does the code survive *this* order?" and the test
*is* the order — named, reviewed, reproducible run to run without a seed.
The workflows compose: loom/shuttle discover an interleaving that fails,
you script the same scenario once as a shadow-point test to pin the fix.
shadow-point's own loom status (no `cfg(loom)` integration; std primitives
are invisible to loom's model) is in the [Loom](#loom) section above.

### Detectors: Miri and ThreadSanitizer

[Miri](https://github.com/rust-lang/miri) (`cargo +nightly miri test`)
interprets the MIR of an all-Rust dependency tree and flags undefined
behavior and data races; `-Zmiri-many-seeds` reschedules the run per
seed. [ThreadSanitizer](https://doc.rust-lang.org/unstable-book/compiler-flags/sanitizer.html)
(`-Zsanitizer=thread`) instruments real threads and reports the races the
actual schedule happened to produce. Both observe a run after the
fact — "did this execution contain a race?" — where a sync point
prescribes one: "run the rival operation *now*, on this thread". They
compose the same way loom/shuttle do: a detector finds a race, a
shadow-point test pins the interleaving that provokes it on demand. The
cost asymmetry keeps both useful: Miri is orders of magnitude slower
than real threads and cannot run FFI, so it fits small all-Rust units;
TSan pays a several-x slowdown on real threads, but reports only what
that one run produced.

### Deterministic simulators

[`madsim`](https://docs.rs/madsim) and
[`turmoil`](https://docs.rs/turmoil) achieve reproducibility by
*replacing the environment*, at different depths. `madsim` swaps tokio
itself for a drop-in simulated runtime (patched `madsim-tokio` plus
`RUSTFLAGS="--cfg madsim"`): tasks, timers, RNG, the network and process
crashes all run under the simulator. `turmoil` keeps tokio and puts
every host on a single simulated thread, introducing hardship through a
deterministic network and, behind a feature flag, a simulated
filesystem. That is a different layer than a sync point: simulators
give you a repeatable whole-system run, but you steer time and
messages, not "what the competing thread does at line N of `insert`"
inside your real runtime. The nearest approach to a sync point in that
world is turmoil's unstable `barriers` feature — a source-level
`trigger(event)` a test-side `Barrier` suspends execution at, filtered
by an event predicate — and it still cannot run a rival closure *in
place of* the operation with its call arguments, install per-thread,
or reach code outside the simulated single-thread runtime. For a
crash-safe storage engine or lock-free
structure tested against real threads and a real tokio runtime, the
simulator's control surface is the wrong shape — the code under test
runs through the simulator's runtime and shims, not through production
I/O.

### Hand-rolled mocks and barriers

The most common alternative is not purpose-built for concurrency:
[`mockall`](https://docs.rs/mockall) and friends generate trait doubles,
or ad-hoc channels/barriers are woven into the code so tests can force
an interleaving. Both move test structure into the production API — a
trait object, a hook slot, a `Debug` seam that ships — and hand-rolled
barriers re-implement, worse, what `sequence`
parking and `expect_calls` already provide: loud failure with a
diagnostic instead of a hang, counts checked at drop, fire-once and
predicates as data. A sync point costs one `invoke!` line at a chosen
linearization point and compiles to nothing in release. Use mocks for
*substituting a collaborator's whole behavior*; use a shadow point to
*shadow one operation at one point* without reshaping the API.
