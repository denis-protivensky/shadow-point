# shadow-point — Single-Threaded Hooks for Concurrent Tests

Sync points let tests inject code at linearization points of concurrent
operations. In production they compile to nothing.

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

## What `define_sp!` generates

Given `prefix MyModule`, the macro generates:

| Name | What |
|---|---|
| `MyModuleSp` | Entry-point struct — `install_guard()`, `with_dyn()`, associated hook constants |
| `MyModuleSyncPoint` | Extension trait (supertrait: `SyncPoint`) |
| `MyModuleHook` | Enum with one snake_case variant per hook |
| `MyModuleSpGuard<T>` | Guard — `Deref<Target=T>`, hook registration, `Drop` |
| `MyModuleSeqBuilder<T>` | Builder for `guard.sequence(...)` |

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

## Debugging

### SP_TRACE

Set the `SP_TRACE` environment variable to print every hook fire:

```sh
SP_TRACE=1 cargo test -- --nocapture
```

Output:
```
[sp] before_insert call #1
[sp] after_commit call #1
[sp] after_commit call #2
```

Useful for discovering expected sequences and call counts.

### Panic messages

Drop assertions include the install location:

```
sync point hook `before_insert` fired 1 time(s), expected 2
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