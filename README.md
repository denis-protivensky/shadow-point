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
- [Debugging](#debugging)
- [Production safety](#production-safety)
- [Compatibility](#compatibility)
- [Loom](#loom)

The reference material lives in [`docs/`](docs/):

| Document | Contents |
|---|---|
| [Guide](docs/guide.md) | what `define_sp!` generates; private-mode guard API and dispatch order; fire-once, `sequence`, predicate-gated entries, `every`, `expect_calls`; hook arguments; per-worker guards; worked "what you can test" scenarios; `current_fire()` and panic-message formats |
| [Shared mode](docs/shared-mode.md) | one sync point across threads: lifecycle, the parking rendezvous, `Gate`, worked examples (ordering, parking, counting), caveats |
| [Integration](docs/integration.md) | shadow-point as a dev-dependency (the seam macro); async (tokio) consumers and `TokioAsyncGate` |
| [Alternatives](docs/alternatives.md) | comparison with `fail`, loom, shuttle, Miri/TSan, madsim, turmoil, mocks — and when to pick which |

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
| 1. Scripted interferer | private guard on the test thread | one | n/a — one thread scripts everything | guard Drop | shadow a rival operation at the linearization point (see [What you can test](docs/guide.md#what-you-can-test)) |
| 2. Per-worker guards | private guard installed **in each worker thread** | several | your own gates/atomics, in the test's code | each guard's Drop, per thread | each worker's hook behavior is a local contract; workers must merely not overlap |
| 3. Shared sync point | `install_shared` + `install()` per worker | several | the macro: `sequence` parks out-of-order arrivals | aggregated at last `Arc` drop | cross-thread order/counts *are* the assertion |

Decision rule: a private guard is a thread-local — it sees only its own
thread's fires. A thread without an install dispatches to the default
no-op and its fires disappear: the most common wrong-mode mistake is a
guard installed on the *test* thread while the hooks fire on workers.
One thread under test → pattern 1. Several threads → the trigger is
*what you assert*: per-worker local contracts, cross-thread order held
by your own gates → pattern 2; the order/counts *are* the assertion →
pattern 3, and every firing worker installs.

The same split has two useful readings:

- **Concurrency vs parallelism.** Guard mode is concurrency without
  parallelism: two logical actors — the operation under test and the
  interferer — alternate at the linearization point, deterministically,
  on one real thread; nothing actually races. Shared mode adds the
  parallelism: real threads race for real, and `sequence`/`Gate` pin
  the interleaving down so the chosen scenario reproduces run to run.
- **Interior mutability.** The guard derefs to `&T`, and every hook
  closure receives the same `&T` — never `&mut`. The operation under
  test and the interferer share one value through shared references,
  so the instrumented API must mutate through `&self`. That is exactly
  what lets a hook closure perform the rival operation at the
  linearization point — the `map.insert(*key, "rival")` pattern in the
  [guide's examples](docs/guide.md#what-you-can-test).

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
module path (`invoke!(writer_sp::WriterSp, …)`) — see the
[guide](docs/guide.md#what-define_sp-generates) for the full picture.

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
from the prefix that way; see the
[guide](docs/guide.md#what-define_sp-generates)). The arguments
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

The interesting case is a *shadow*: the hook closure runs the operation
the competing thread would have run, at the exact linearization point.
Here the map's own `insert` fires `before_insert` between "key 1 is free"
and the write — so the closure steals the slot first, and the outer insert
is made to lose the race on demand:

```rust,ignore
#[cfg(test)]
mod tests {
    // The entry point lives in the parent module — the file that ran
    // `define_sp!`. (If the declaration sits in its own `mod writer_sp`,
    // import it from there: `use super::writer_sp::WriterSp;`.)
    use super::MyModuleSp;

    #[test]
    fn insert_loses_the_race() {
        // A concurrent map: interior mutability, mutates through &self.
        let guard = MyModuleSp::install_guard(my_map());

        // before_insert fires at the linearization point: the lookup
        // already passed ("key 1 is free"), the write has not. Shadow
        // the rival thread here — take the slot it was about to take.
        guard.before_insert(|map, key| {
            map.insert(*key, "rival"); // the competing op, run in place
        }).expect(1);

        // The real insert now finds the key taken: it must report a
        // duplicate, NOT overwrite the value the hook just wrote.
        assert!(guard.insert(1, "mine").is_err());
        assert_eq!(guard.get(&1), Some(&"rival"));
    }
}
```

`MyModuleSp` is the only import: a test module nested inside the
instrumented file takes it from the parent module, and hooks need no
import at all — they are associated constants on the entry-point struct.

Beyond `install_guard` and the registration methods, nothing here is
shadow-point API: `my_map()` is your constructor, and `guard.insert(...)`
/ `guard.get(...)` are *your* `T`'s methods — the guard derefs to `&T`, so
the guarded value's own methods are callable through it. The nested
`map.insert` inside the closure does not recurse into `before_insert`:
fires of the same sync point on a thread already inside a hook are
suppressed (see the intro), which is what keeps the count at `.expect(1)`.

This is the private mode: the guard sees only fires from this thread.
For hooks fired from several threads there are two ways to go — per-worker
private guards when each thread's contract is what you assert
([guide](docs/guide.md#per-worker-guards-private-mode-on-real-threads)),
or one `install_shared` sync point when the cross-thread order/counts
*are* the assertion
([shared mode](docs/shared-mode.md#shared-mode-one-sync-point-across-threads)).
Hook declarations and `invoke!` calls are identical in all of them.

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
Inside a hook closure, `current_fire()` reports the hook name, the
fire index, and the firing thread; every drop assertion prints the
install site — formats and details are in the
[debugging reference](docs/guide.md#debugging-reference).

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

How shadow-point differs from `fail` (failpoints), loom, shuttle,
Miri/ThreadSanitizer, madsim, turmoil, and mocks — with a decision
rule for each — is in
[docs/alternatives.md](docs/alternatives.md). In one line: shadow-point
*scripts* one chosen interleaving at a named point in real code, where
the alternatives inject failures, explore or detect schedules, replace
the environment, or reshape the API.

