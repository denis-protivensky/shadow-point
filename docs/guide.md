# Private mode, hook arguments, and the `define_sp!` reference

Shared sync points (cross-thread sequences, parking, `Gate`) live in
[shared-mode.md](shared-mode.md); the dev-dependency seam and tokio support
in [integration.md](integration.md).

## Two readings of the mode split

The same two modes (and the three usage patterns built on them) are worth
reading twice:

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
  linearization point — the `map.insert(*key, "rival")` pattern in
  [What you can test](#what-you-can-test).

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
| `EveryClosures<T>` | Builder for `every(...)` (per-hook setters) |
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
(derived from the prefix via `paste!`), the macro emits names that do not
derive from the prefix: module-private machinery
(`__Sp`, `__SpDefault`, `__SP_DEFAULT`, `__SP_TL`, `SeqEntry`), and the
visibility-bearing `EveryClosures` / `SpExpect` types. Two declarations
in one module collide on those names.

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
and `install_guard` / `install_shared` carry the same bounds: the state is
reference-counted (`Arc`) and may cross threads, and dispatch reaches it
through a trait object, so `T` itself has to be shareable. Registered
closures must be `Send + 'static` (`every` closures additionally `Sync`).

`MyModuleHook` derives `Debug, Clone, Copy, PartialEq, Eq`; the hook
list must contain at least one hook.

### Hook placement: one hook per call site

Give each `invoke!` site in production code its own hook method — do not
share one hook name across call sites. A fire is identified by its hook
name alone, so nothing downstream can tell the sites apart: a sequence
entry consumes whichever site fires first, `expect_calls` mixes the
sites' counts, and an ordering-violation panic cannot say which site
fired out of turn.

Repeated fires of the *same* site (loops, retries) are expected: tell
them apart by arguments with [predicate-gated
entries](#predicate-gated-entries) (`s.hook_when(pred, …)`), not by
extra hook names.

### The extension trait: generic code over a sync point

`MyModuleSyncPoint` has one method per hook (taking the declared
arguments) with a default no-op body; its supertrait `SyncPoint` is a
`Send + Sync` marker. The thread-local dispatch holds the installed state
behind an `Arc` — installed (or restored) by a guard, `None` on a thread
that never installed — and a thread without an install dispatches to the
default no-op impl, which is why firing a hook on an uninstalled thread
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
- **Asserts on drop** — an unconsumed sequence entry or a count mismatch
  panics on a thread that is not unwinding; while the thread is already
  unwinding the same message is printed instead (see
  [Keep a failure one test](#keep-a-failure-one-test)). Entries marked
  `.optional()` are exempt (see below).
- **Owns the state and releases it** — the guard holds the state, and
  dropping the guard releases it: the guarded value's own `Drop` runs, at
  guard drop, *after* the assertions above. In shared mode the last `Arc`
  is that moment (see `shared-mode.md`). Before 0.4.0 the state was
  `Box::leak`ed and none of this was observable — see
  [Reclamation and what it owns](#reclamation-and-what-it-owns) for what
  it costs and what it now lets a test assert.
- **Restores on drop** — the guard binds this thread's dispatch for its
  lifetime and, on drop, restores the previously installed sync point
  (stacked installs unwind LIFO; after the drop, fires on this thread
  hit the previous install or the default no-op). Drop the guard on the
  thread that installed it: dropping a guard that was moved to another
  thread clobbers that thread's install.

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
so other threads keep running and reporting their own diagnostics.

Guard/`Arc` drop assertions are **reported, not raised, while the thread
is already unwinding**: the same message goes to stderr with
`(while unwinding)` appended, and the process keeps running. On a thread
that is *not* unwinding the assertion is raised exactly as before: a test
that ends with an unconsumed sequence or a wrong call count fails. Why
printing was the only safe form, and the habits that keep the fix sharp:
[Keep a failure one test](#keep-a-failure-one-test).

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
sets the same counter expectation (see **Registration precedence and
lifetime**); `expect_calls` exists to declare counts independently of
closures.

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

## Keep a failure one test

A drop assertion that fires while the test is already failing is a second
panic on top of the first, and Rust turns a double panic into an abort:
the test binary dies, the harness reports nothing after it, and the real
failure — the test's own — is the only thing worth reading. The library
now prints that diagnostic instead of raising it, so an unwinding drop
cannot take the run down. Four habits keep the fix useful rather than
merely survivable:

- **Let the guard own a narrow scope, and assert outside it.** The drop
  then happens on a thread that is not unwinding, so a genuinely
  incomplete scenario still fails loudly with its own message, and the
  test's own assertion fails separately:

  ```rust,ignore
  fn observed_by_the_hook() -> Vec<u32> {
      let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
      let out = {
          let guard = MyModuleSp::install_guard(());
          // Collect on EVERY fire, and count separately. A fire-once
          // registration runs its closure on the first fire only — the slot
          // is consumed — so collecting three keys through one would
          // collect one, while `.expect(3)` still passes: the counter counts
          // FIRES, not closure runs. The mismatch would surface as a plain
          // `assert_eq!` failure in the test body, far from its cause.
          guard.expect_calls(MyModuleSp::before_insert, 3);
          let s = seen.clone();
          guard.every(|e| {
              e.before_insert(move |_, key| {
                  s.lock().unwrap().push(*key);
              });
          });
          run_the_thing(); // the instrumented call, hooks fire here
          // Read through the same lock instead of `Arc::try_unwrap`: this
          // runs while the guard is still alive, and its `every` registration
          // holds another `Arc` clone of `seen`, so `try_unwrap` would fail
          // with an opaque `Err(Arc)` even when the scenario ran.
          seen.lock().unwrap().clone()
      }; // the guard (and its drop assertions) end here
      out
  }

  #[test]
  fn one_visit_per_key() {
      assert_eq!(observed_by_the_hook(), vec![1, 2, 3]);
  }
  ```

- **Prefer `expect(n)` to `sequence(...)` when the claim is only a
  count.** `expect_calls` / `.expect(n)` assert one number and cannot
  strand a step. `sequence` is a choreography: an unfinished choreography
  is exactly the message that used to abort the binary.

- **Finish the scenario before a `#[should_panic]`.** `thread::panicking()`
  is true inside an expected panic too — that test unwinds by design, with
  no first failure behind it — so a guard dropped there prints its
  assertion instead of raising it, and an incomplete choreography **passes**.
  This is the accepted cost of the fix (the alternative aborted the whole
  binary, pinned by `should_panic_test_passes_with_an_incomplete_scenario`),
  and two shapes keep the teeth: end the scenario and `drop(guard)` before
  the expected `panic!` (pinned by
  `guard_dropped_before_the_expected_panic_still_asserts`), or keep the
  guard outside a `catch_unwind` — after the catch the thread is no longer
  unwinding and the drop raises on the clean path.

- **Do not let a hook closure panic on an assertion it wants to make.**
  A panic raised inside a closure propagates out of the `invoke!` site and
  unwinds the instrumented operation mid-flight — which is the intended
  way to stop a scenario, but the record belongs in the test body:
  collect through the closure (the `seen` vector above) and assert after
  the guard is gone.

With these, one bad scenario costs one test. The last habit is the one
that catches people first, because "assert inside the hook" reads as the
natural way to check something at a linearization point.

## Reclamation and what it owns

`install_guard`/`install_shared` own their state in an `Arc` and release it.
Up to 0.3.x the state was `Box::leak`ed, because the thread-local dispatch
held a `&'static dyn …` and in shared mode the same state is published to
worker threads — reclamation would have to prove no thread is inside a
dispatch at the moment the last handle goes away. That proof is now carried
by the reference count itself: every handle is a strong `Arc`, including the
one a dispatch clones before calling out (see `with_dyn`), so a state cannot
be freed while something is reading it, and no `unsafe` was needed for the
change (`forbid(unsafe_code)` still holds).

**Who holds a strong reference.** The guard; this thread's TLS entry; in
shared mode, one per worker that installed; and, for the duration of one
fire, the dispatch itself. `EXEC_STACK` keys re-entry suppression on the
state's *address*, which reclamation would otherwise make reusable under a
live frame — the dispatch's clone is what keeps that impossible.

**When the value goes.** The guard's drop is the release moment, in this
order:

1. restore this thread's TLS (dispatch can no longer reach the state, and
   any outer install lives on through the guard's saved handle);
2. run the drop assertions, while the state is fully readable — they read
   counters and the deque, not `T`, so freeing `T` first would make them
   true only by accident;
3. release this guard's reference: if it was the last, the state is freed
   and the guarded value's own `Drop` runs.

In shared mode the same order is the last `Arc`'s drop (a worker's guard
holds one, so the value cannot go while any worker is installed). On an
unwinding thread step 3 deliberately **leaks instead**: a guarded value
whose `Drop` panics would double-panic and abort the binary — the exact
failure 0.3.2 removed the assertions for. A leak on a path that is already
failing is the cheaper half of that trade.

| What holds what | Since 0.4.0 | Consequence |
|---|---|---|
| `T` is the value the test passes in | **dropped at guard drop** | the destructor is observable — see the migration note below |
| `T` is an `Arc<_>` the test cloned | dropped when the *last* clone goes | usually the test's own `drop`, if it does not leak clones into hooks |
| `T` is a `Weak<_>` | never by us | the test keeps full control of the timing |
| an `every` closure, or a fire-once that never fired | **freed with the state** | a captured `TempDir`/`Child`/channel handle goes back with it |
| any of the above, on an unwinding thread | **leaked, on purpose** | no abort, no destructor running mid-unwind |

**What the change costs, measured** (min of 11 runs per fire, pinned to two
cores, on a machine already loaded — treat the numbers as orders of
magnitude, not as a bench): a fire through a live guard is ~60 ns either
way, with reclaim costing ~5 ns more for the one extra `Arc` clone/drop;
`install`+`drop` per cycle went from ~290 ns to ~95 ns, because the leak
built the state once and never gave it back; an uninstalled fire is test-only
code (`invoke!` compiles away in a `#[cfg(test)]`-gated path) and went from
1.7 to ~2.5 ns, the `RefCell` borrow and the count.

**Migration note.** `T::drop` becoming observable is the point, and it is a
semantic change: a test that asserts *inside* the value's `Drop` — a debug
assert on internal invariants, for instance — used to pass because that
assertion never ran, and now runs at guard drop, *after* the sync-point
assertions. A scenario that abandoned entries or mis-ordered fires can now
fail there for the first time. That is a real finding, not a regression, but
it does not arrive silently-green.

Three shapes still matter for destructor *timing*, now as choices rather
than workarounds:

- **Guard a `Weak`** when the test must decide the moment. Pass
  `Arc::downgrade(&v)` as `T`; the hook closures are `'static`, so they take
  their own `Weak` clone to upgrade, never the `Arc` (an `Arc` in a
  registration would keep the value alive until the state is released):

  ```rust,ignore
  let v = std::sync::Arc::new(Resource::open());
  let w = std::sync::Arc::downgrade(&v);
  let guard = MyModuleSp::install_guard(std::sync::Arc::downgrade(&v));
  guard.before_close({
      let w = w.clone(); // a Weak, never the Arc
      move |_, _| {
          if let Some(res) = w.upgrade() {
              res.note_closed();
          }
      }
  }).expect(1);
  run_the_thing(std::sync::Arc::clone(&v)); // the code under test owns a strong Arc
  drop(v);
  assert!(w.upgrade().is_none(), "the destructor ran and nothing kept it alive");
  ```

  With `T = Weak<_>` the state holds no strong reference, so that last
  assertion is the test's own timing, not ours. This costs the hook body one
  `upgrade()` per fire.
- **Assert on a flag the destructor sets** when the destructor lives inside
  the instrumented type. Name all four roles: the object implementing `Drop`
  is a small `Dropped(Arc<AtomicBool>)` wrapper, owned by the *instrumented
  value*; it is dropped when the *production code* releases it; the flag
  lives in the test's own clone.

  ```rust,ignore
  struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
  impl Drop for Dropped {
      fn drop(&mut self) {
          self.0.store(true, std::sync::atomic::Ordering::SeqCst);
      }
  }
  struct Resource {
      _watchdog: Dropped, // the destructor that runs is THIS one: inside the
                          // production value, NOT inside T
      // ... real fields ...
  }

  let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
  let resource = Resource { _watchdog: Dropped(flag.clone()) };
  let guard = MyModuleSp::install_guard(()); // T is the unit: `Resource` is not T
  // ... drive the instrumented code so the production value releases `resource` ...
  assert!(flag.load(std::sync::atomic::Ordering::SeqCst), "the watchdog ran");
  ```

  Use this when you want the destructor's *moment to be the production
  code's*, not the guard's; with reclaim the second option is simply `T`
  itself, which now drops.
- **Test the destructor outside a sync point.** The type's own `Drop` is
  ordinary Rust: `drop(map); assert!(file.is_closed())` needs no hook at
  all. Reserve the sync points for what only they can pin — the
  interleaving.

Captures follow the same rule in the other direction: an `every(...)`
closure that captures a `tempfile::TempDir` or a `Child` used to keep it
alive for the process; with the state released, the capture goes with it.

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
    // Cross-thread ORDER is enforced by the test's own gates, not by a
    // shared sequence. `first_done` proves T1's before_insert completed.
    let first_done = Arc::new(Gate::new());

    let done_t1 = first_done.clone();
    let t1 = std::thread::spawn(move || {
        // Each worker binds its OWN private guard: hooks registered
        // here fire only on this thread, and Drop asserts this thread's
        // counts.
        let guard = MyModuleSp::install_guard(());
        guard
            .before_insert(move |_, key| {
                assert_eq!(key, &1, "T1 inserts first");
                done_t1.set(); // completion signal for T2's turn
            })
            .expect(1); // T1's own hook count, checked at this guard's Drop
        real_insert(1); // the instrumented API fires this thread's hooks
    });

    let wait_t2 = first_done.clone();
    let t2 = std::thread::spawn(move || {
        let guard = MyModuleSp::install_guard(());
        guard
            .before_insert(move |_, key| {
                assert_eq!(key, &2, "T2 inserts second");
            })
            .expect(1);
        // Ordered *after* T1's hook completes — the gate does what a
        // shared sequence's parking would, without shared state.
        wait_t2.must_wait(PARK_TIMEOUT); // fails loudly if T1 never fired
        real_insert(2);
    });

    t1.join().unwrap(); // each guard — and its expect(1) — drops in-thread
    t2.join().unwrap();
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
dispatch running on this thread: the `hook` name and the zero-based
`index` of this fire within its hook's count — a per-sync-point
counter shared by all threads, the same number `SP_TRACE` prints
(1-based). The closure runs on the firing thread, so thread info is
just `std::thread::current()`.
Outside a dispatch it returns `None`. Handy for helpers that must know
which hook fired without being told:

```rust
let f = shadow_point::current_fire().expect("inside a hook");
eprintln!("{} #{} on {:?}", f.hook, f.index, std::thread::current().id());
```

### Panic messages

Drop assertions include the install location. Each diagnostic is one
line; the samples below are wrapped for readability:

```
sync point hook `before_insert` fired 1 time(s), expected 2
(installed at src/my_module.rs:142:21)
```

(In shared mode the location is marked `(shared, installed at ...)`.)

The unconsumed-sequence diagnostic names each remaining entry (hook
names, comma-separated):

```
sync point sequence not fully consumed: 2 entries remaining [before_insert,
after_commit] (installed at src/my_module.rs:142:21)
```

The same message printed rather than raised, because the thread was
already unwinding when the guard (or the shared state's last `Arc`)
dropped — stderr, prefixed, and the run
continues:

```
shadow-point: sync point hook `before_insert` fired 1 time(s), expected 2
(installed at src/my_module.rs:142:21) (while unwinding)
```

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
