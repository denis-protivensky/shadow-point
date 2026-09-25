# Integration: dev-dependency seam and async (tokio) consumers

Dispatch semantics: [guide.md](guide.md) and [shared-mode.md](shared-mode.md).

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

## Async (tokio) consumers

On a `current_thread` tokio runtime, the single executor thread must
never park — the wake it is waiting for can never run because it *is*
the executor thread. That makes `Gate::wait` unusable inside or
alongside async choreography: parking the executor stalls all work
until a timeout panic. The TLS caveat in
[Per-worker guards](guide.md#per-worker-guards-private-mode-on-real-threads)
applies: dispatch follows the OS thread, not the task — keep the
instrumented section await-free, or move it into `spawn_blocking` and
install inside the closure.

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

The feature pulls in optional `tokio` (default-features off, `sync`
only, ≥ 1.21) — the library uses `tokio::sync::Notify` and
`tokio::pin!` and nothing else, and `pin!` is available without the
`macros` feature. The floor is deliberately conservative: `Notified::enable`,
which `wait_at_least` needs, shipped in tokio 1.19, and the manifest rounds
up to 1.21. Version and MSRV details are in the README's
[Compatibility](../README.md#compatibility) section. Enable
`tokio-async` as a **dev**-dependency in your project — production
builds keep the seam-cfg-stripped zero-cost property. If your toolchain
is below Rust 1.70, pin tokio in your lockfile so Cargo does not
resolve a newer one.

Concretely: if shadow-point is only a dev-dependency, add
`features = ["tokio-async"]` to that entry; if it is also a regular
dependency (the seam setup), keep that entry untouched and add a second
`[dev-dependencies]` entry with the feature — Cargo unifies the feature
flags of both entries for test builds.

**Example** (simplified; the full tests live in
[`tests/tokio_async_gate.rs`](../tests/tokio_async_gate.rs)):

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
