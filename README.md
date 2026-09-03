# shadow-point — Sync Points for Concurrent Tests

Hook points let tests inject code at linearization points of concurrent
operations — single-threaded through guards, or shared across threads via
`install_shared` (a parking rendezvous at the sequence head). Nested fires
of the same sync point on the same thread are silently suppressed. In
production they compile to nothing.

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

### 3. Write tests

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

## Guard API

`install_guard(value)` returns a guard that:
- **Derefs** to `&T` — call methods on the value directly through the guard.
- **Registers closures** for hooks.
- **Auto-counts** every hook fire.
- **Asserts on drop** — unconsumed sequence entries and count mismatches
  panic. Entries marked `.optional()` are exempt (see below).

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

The `_every` closure runs before fire-once / sequence dispatch.

### expect_calls

Declare expected call counts (checked at guard drop):

```rust
guard.expect_calls(MyModuleSp::after_commit, 3);
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

## Shared sync points (cross-thread)

`install_shared(value)` returns an `Arc<SharedSp<T>>` that any thread can
drive. Register `sequence`/`every`/`expect_calls`/fire-once from the test
thread *before* spawning workers; each worker installs the sync point on
its own thread (TLS is per-thread):

```rust,ignore
#[test]
fn concurrent_writers_commit_in_order() {
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let shared = MyModuleSp::install_shared(());

    shared.sequence(|s| {
        s.before_insert(|_, _| log.lock().unwrap().push("before"));
        s.after_commit(|_, _| log.lock().unwrap().push("after"));
    });

    let s1 = shared.clone();
    let t1 = std::thread::spawn(move || {
        let _guard = s1.install();
        // ... drive real instrumented code ...
        shadow_point::invoke!(MyModuleSp, before_insert(&42));
    });
    // ... more workers ...
    t1.join().unwrap();
    // Assertions run on the final Arc drop.
    drop(shared);
}
```

A worker that fires a hook whose entry is not at the sequence head
**parks** (up to `PARK_TIMEOUT`, default 10 s) instead of panicking; it
re-checks the head after every wake. Parking is a mutual-deadlock guard,
not a synchronization mechanism: if the head can never advance, the worker
panics with a `sequence park timeout` diagnostic mentioning the head hook
and the install site — failing the test instead of hanging it.

Coordinate *completion* (not just consumption) with `Gate`, a
level-triggered boolean barrier (`set` before `wait` is not lost):

```rust,ignore
let gate = Arc::new(shadow_point::Gate::new());
shared.sequence(|s| {
    s.step_a({
        let gate = gate.clone();
        move |_, _| {
            // Append/record first, then park: the entry is visible even if
            // the gate is never released.
            log.lock().unwrap().push("a");
            gate.must_wait(shadow_point::PARK_TIMEOUT);
        }
    });
});
```

Caveats:

- A parked thread is inside the instrumented code path and holds
  production locks: order `sequence` entries so the head consumer never
  needs a lock held by a parked thread (otherwise `sequence park
  timeout`, not a hang).
- `sequence` orders *consumption* of entries, not completion of their
  closures — coordinate completion with `Gate`.
- Fire-once closures are nondeterministic in shared mode (the winner of
  the call-counter race); use `sequence` for deterministic ordering.
- Register before spawning workers; early fires fall through to
  fire-once/counter.
- Same-named entries from different threads are each consumed exactly
  once, but wake order is not FIFO.
- Join workers and drop their guards before dropping the last `Arc`.
- `install_guard`/`install_shared` leak the per-install state
  (`Box::leak`) by design; that is normal for test instantiation, but do
  not call `install` in a long-lived loop.
- Inside step closures use `Gate::must_wait`, never `Gate::wait`: a
  missed `set` fails the test with a diagnostic instead of hanging it.
- Predicates on gated entries must be pure — they run while the sequence
  lock is held (no hook invocations, no blocking).

## What you can test

Hooks turn rare thread interleavings into deterministic, reproducible
scenarios: at the exact linearization point, the closure runs the operation
that the *other* thread would have run. Each example below assumes a map
with the hook set from the Quick start (the third also declares
`before_get_search(root: *const ())`).

Note: a hook re-fired on the registering thread is silently suppressed, so
the interleaving operation inside the closure runs without recursing into
its own hooks.

### The inserting thread loses the race

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

### The key vanishes under an in-flight remove

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

### A reader pinned a node the remover detaches

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