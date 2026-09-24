# Shared sync points across threads

The private-guard API, dispatch order, and hook arguments are in [guide.md](guide.md).

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
   point (usually the default no-op). Drop the guard on the thread
   that installed it — same rule as in
   [private mode](guide.md#guard-api-private-mode).
4. **Drive** — workers call the real API; its `invoke!` sites dispatch
   into the shared sequence. A worker whose hook matches only a *later*
   entry **parks** (see [Parking](#parking-the-sequence-head-is-a-rendezvous)).
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
[Registration precedence and lifetime](guide.md#registration-precedence-and-lifetime)).

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
[Predicate-gated entries](guide.md#predicate-gated-entries)).

### `sequence` vs `every` + trace: choosing your assertion

Shared mode offers two assertion shapes, and picking wrong costs you
either flakiness or a parking hazard:

- **`sequence`** asserts *global order* — the strong property. Cost: an
  out-of-order arrival parks **inside the instrumented path**, with
  production locks held (see [Caveats](#caveats)). Also unusable when legitimate
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

### Caveats

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
- Inside step closures use `Gate::must_wait`, never `Gate::wait`
  (see [`Gate`](#gate-coordinating-completion-not-just-consumption)).
- Predicates on gated entries must be pure — they run while the sequence
  lock is held (no hook invocations, no blocking).

The examples above follow [`tests/cross_thread.rs`](../tests/cross_thread.rs):
the parking and counting examples are transcriptions of `park_until_turn`
and `expect_calls_aggregates_threads` with the domain names swapped; the
first example is a simplified variant of `sequence_order_across_threads`
(two steps, gate wait moved out of the instrumented path) — run the
real tests with `cargo test --test cross_thread`.
