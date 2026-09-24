# Comparison with similar crates

Moved out of the [README](../README.md). shadow-point's own loom status is
in the README's [Loom](../README.md#loom) section.

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

## Failpoints (the `fail` crate)

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
change what the call returns ([Hook arguments](guide.md#hook-arguments)).

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
  `invoke!` ([Using as a dev-dependency](integration.md#using-as-a-dev-dependency));
  and the thread-local guards keep the async caveat
  ([Per-worker guards](guide.md#per-worker-guards-private-mode-on-real-threads)) —
  on tokio, `spawn_blocking` + `TokioAsyncGate` ([Async (tokio)
  consumers](integration.md#async-tokio-consumers); fire/count/await, no parking)
  rather than parking a guard across an `.await`.

An integration shadow-point seam is therefore a deliberate exception, not
a default: land it with its justification. The two tools coexist happily —
failpoints for the environment misbehaving, shadow-points for the rival
thread behaving in a scripted way.

## Loom and shuttle: exploring schedulers

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

## Detectors: Miri and ThreadSanitizer

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

## Deterministic simulators

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

## Hand-rolled mocks and barriers

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
