# Private mode, hook arguments, and the `define_sp!` reference

Moved out of the [README](../README.md) to keep it scannable. Shared sync
points (cross-thread sequences, parking, `Gate`) live in
[shared-mode.md](shared-mode.md); the dev-dependency seam and tokio support
in [integration.md](integration.md).

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

One `define_sp!` per module. Besides the prefixed names in the table
(derived from the prefix via `paste!`), the macro emits a set of names
that do not derive from the prefix: the
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
   [parking example](shared-mode.md#example-parking-is-the-rendezvous).
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
       re-checks after every wake (see [Parking](shared-mode.md#parking-the-sequence-head-is-a-rendezvous)) — a parked fire
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

Each `s.hook(closure)` pushes an entry. On fire, the head entry must match
the hook. A fire of a *different* hook is an ordering violation only when
some *later* entry expects the hook that fired — then private mode panics
immediately (`sync point ordering violation: expected <hook> next but
<hook> fired`) and shared mode parks the arrival at the head
([Parking](shared-mode.md#parking-the-sequence-head-is-a-rendezvous)).
With no later entry expecting that hook, the fire falls through to
fire-once instead — an unmatched head does not panic on its own (see
Registration precedence and lifetime above). A second `sequence(...)`
call replaces the whole deque, it does not append.

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
only shared (see the [shared-mode caveats](shared-mode.md#caveats)): it must be pure, with no hook
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
[Example: counting fires across threads](shared-mode.md#example-counting-fires-across-threads)).

See **Registration precedence and lifetime** above for the full dispatch
order: what `every`, `sequence`, and fire-once each see, and in what
order.

### What you can test

Hooks turn rare thread interleavings into deterministic, reproducible
scenarios: at the exact linearization point, the closure runs the operation
that the *other* thread would have run. Each example below assumes a
*concurrent* map — one with interior mutability, mutating through
`&self` — with the hook set from the [Quick start](../README.md#quick-start) (the third also declares
`before_get_search(root: *const ())`). The examples use the private
guard; in shared mode the same interleavings are scripted as `sequence`
steps, coordinated with [`Gate`](shared-mode.md#gate-coordinating-completion-not-just-consumption).

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
nothing fires nothing — its hooks silently no-op (the wrong-mode
mistake from the README's decision rule). The guarded value can
equally be an `Arc` shared between workers; what must *not* be shared
is the guard, which is per-thread by construction.

The same TLS rule bites async runtimes: dispatch follows the *thread*,
not the task — a future resumed on a different OS worker after an
`.await` fires into whatever *that* thread has installed (usually
nothing: the fires vanish silently, in both modes). Keep the
instrumented section free of awaits between install and the last
fire — or run it through `spawn_blocking` and install inside the
closure, where the OS thread is yours. On tokio, see
[Async (tokio) consumers](integration.md#async-tokio-consumers) for
the cooperative gate.

## Debugging reference

`SP_TRACE` tracing is described in the
[README](../README.md#debugging); the programmatic companions follow.

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
