# shadow-point — Sync Points for Concurrent Tests

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
| Out-of-order sequence fire | panics immediately | parks the early thread at the head, panics on `PARK_TIMEOUT` |
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

### 1. Declare hooks

At module level, gate with `#[cfg(test)]` and call `define_sp!`:

```rust
// src/my_module.rs

#[cfg(test)]
shadow_point::define_sp! {
    pub(crate) prefix MyModule
    {
        before_insert(key: &K),
        before_remove(id: usize),
        after_commit(),
    }
}
```

In production builds `#[cfg(test)]` removes the call entirely.

### 2. Insert `invoke!` calls

At the points in your production code where tests need to hook in:

```rust
shadow_point::invoke!(MyModuleSp, before_insert(&key));
// ... do the insert ...
shadow_point::invoke!(MyModuleSp, after_commit());
```

`invoke!` takes the entry-point struct (`MyModuleSp`, generated from the
prefix) and a method call. In production `invoke!` compiles to `{}`.

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

Associated constants on the entry-point struct let you reference hooks
without importing the enum:

```rust
MyModuleSp::before_insert   // type: MyModuleHook
MyModuleSp::after_commit    // type: MyModuleHook
```

All names derive from the prefix via `paste!`.

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

### Fire-once

Run a closure on the first call of a hook:

```rust
guard.before_insert(|data, key| {
    data.clear();
}).expect(1);
```

`.expect(N)` asserts the hook fires exactly N times. Without `.expect()`,
the closure runs once but no count is checked (on_first semantics).

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
the hook — otherwise panic with an ordering violation message.

Extra fires (after the sequence is consumed) are silently ignored.

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
for a later fire; they also do not trigger fire-once closures for that hook.
Ordering between different hooks stays strict — the gate only filters fires
of its own hook by arguments.

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

### What you can test

Hooks turn rare thread interleavings into deterministic, reproducible
scenarios: at the exact linearization point, the closure runs the operation
that the *other* thread would have run. Each example below assumes a
*concurrent* map — one with interior mutability, mutating through
`&self` — with the hook set from the Quick start (the third also declares
`before_get_search(root: *const ())`). The examples use the private
guard; in shared mode the same interleavings are scripted as `sequence`
steps, coordinated with `Gate`.

Note: a hook re-fired on the registering thread is silently suppressed, so
the interleaving operation inside the closure runs without recursing into
its own hooks.

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

Hook arguments appear in the trait signature. The closure receives `&T`
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
is the guard, which is per-thread by construction. (Inside a tokio
runtime, `spawn_blocking` workers are separate OS threads with their own
TLS — install per worker there too, or use shared mode.)

## Shared mode: one sync point across threads

A private guard sees only the installing thread's fires. When the
assertion is *cross-thread* — the global order of interleaving steps, or
total counts across workers — install one sync point instead:
`install_shared(value)` returns an `Arc<SharedSp<T>>` whose state every
bound thread drives. This mode adds real parallelism: workers race for
real, and `sequence` + `Gate` pin the chosen interleaving down so it
reproduces run to run.

### Lifecycle — six steps, in this order

Each step exists to make the next one deterministic; do not reorder:

1. **Install** — `install_shared(value)` on the test thread →
   `Arc<SharedSp<T>>`.
2. **Register** — `sequence` / `every` / fire-once / `expect_calls` on
   the `Arc`, *before spawning workers*. A fire that beats registration
   falls through to the fire-once/counter path, not to your entry.
3. **Bind each worker** — clone the `Arc` into the thread; its first act
   is `let _g = shared.install();`. TLS is per-thread: this points the
   *current* thread's dispatch at the shared state. Keep the guard alive
   for as long as the worker touches instrumented code.
4. **Drive** — workers call the real API; its `invoke!` sites dispatch
   into the shared sequence. A worker whose hook is not at the sequence
   head **parks** (see Parking).
5. **Join every worker.**
6. **Drop the last `Arc`** — the shared assertions (unconsumed sequence
   entries, `expect_calls` mismatches) run there. After the joins, so
   they cannot race an in-flight fire.

A worker that never calls `install()` dispatches to the default no-op
and its fires vanish silently — that is the shared-mode version of the
wrong-mode mistake. Threads that only wait on gates and never fire hooks
do not need to bind.

### Parking: the sequence head is a rendezvous

A worker that fires a hook whose entry is not at the sequence head parks
(up to `PARK_TIMEOUT`, default 10 s) instead of panicking, and re-checks
the head after every wake. This is what makes shared mode composable:
*any* thread may arrive out of order, and the sequence sorts arrivals —
each same-hook entry is consumed exactly once, the thread that fires the
head hook pops it and wakes the next in line.

Parking is a mutual-deadlock guard, **not** a synchronization
mechanism: if the head can never advance (no live thread will fire it),
the parked worker panics at `PARK_TIMEOUT` with a `sequence park
timeout` diagnostic naming the head hook and the install site. The test
fails loudly; it does not hang.

### `Gate`: coordinating completion, not just consumption

`sequence` orders *consumption* of entries — a thread is released the
moment its entry pops, while its closure may still be running, and its
step may still hold production locks. To wait for a step to *finish*,
coordinate with `Gate`, a level-triggered boolean (`set` before `wait`
is not lost; `clear` re-arms it):

| Call | Semantics |
|---|---|
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

## Debugging

### SP_TRACE

Set the `SP_TRACE` environment variable to print every hook fire (with the
firing thread's name):

```sh
SP_TRACE=1 cargo test -- --nocapture
```

Output:
```
[sp] before_insert call #1 thread=worker-1
[sp] after_commit call #1 thread=worker-1
[sp] after_commit call #2 thread=main
```

Useful for discovering expected sequences and call counts.

### Panic messages

Drop assertions include the install location:

```
hook hook `before_insert` fired 1 time(s), expected 2
(installed at src/my_module.rs:142:21)
```

## Production safety

- `invoke!` compiles to `{}` outside `#[cfg(test)]`.
- `define_sp!` is gated with `#[cfg(test)]` by the caller.
- Zero cost — verified with `cargo build --release`.

## Loom

Under `loom` (`cfg(test)` + `cfg(loom)`), `define_sp!` still generates
infrastructure and `invoke!` dispatches through `with_dyn`. However, the
thread_local holds the default no-op impl, so all hooks are silent no-ops.
This requires no `loom` references in `shadow-point` — the crate is
cfg-gated solely on `cfg(test)`.

Full loom integration (replacing `Cell`/`RefCell` with loom analogs) is
future work.