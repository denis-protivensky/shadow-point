#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
// The crate dogfoods the lint its consumers are subject to. `define_sp!`
// emits public items (trait, hook methods, entry struct, hook enum,
// builders), and a consumer that keeps `#![deny(missing_docs)]` fails its
// *test* build on any of them that carries no doc comment — production
// builds are clean because the caller gates the macro with `#[cfg(test)]`,
// so only a test build sees the generated items. `missing_docs` is inert
// outside a library crate and for `#[doc(hidden)]` items, so the pin lives
// here, in the test build of this crate, on the `sp_docs` probe module.
#![deny(missing_docs)]

// --- invoke! macro ---

/// Invoke the currently installed sync-point hook.
///
/// Takes the generated entry point and a plain `hook(args…)` call; dispatch
/// prepends the guarded `&T`, so it is not written at the call site. It
/// expands to a block expression (`{ #[cfg(test)] { … } }`): a real dispatch
/// under `#[cfg(test)]`, an empty block otherwise. It works as a statement
/// (`invoke!(…);`) and in expression position — a closure body, a block's
/// trailing value, `let () = invoke!(…);` — in both test and production
/// builds. The outer brace pair is what makes the expression form legal: a
/// bare `#[cfg(test)]`-attributed block is a statement, not an expression.
///
/// Expands to a real dispatch under `#[cfg(test)]` (evaluated at the call
/// site's crate) and to a no-op otherwise — production builds have no
/// dispatch code and do not reference the crate.
///
/// # Dev-dependency seam
///
/// As a regular dependency the crate is still linked in production. To drop
/// that coupling too, put it under `[dev-dependencies]`. A dev-dependency
/// is in the extern prelude only when building tests, so a plain
/// `shadow_point::invoke!` path in a production build fails to resolve
/// (`E0433`) — the invocation path is resolved before the macro expands and
/// strips its `cfg(test)` body. Hide the path in a local `macro_rules!`
/// seam:
///
/// ```no_run
/// // src/seam.rs — plain macro_rules; nothing here depends on the crate
/// macro_rules! sp_invoke {
///     ($($t:tt)*) => {
///         #[cfg(test)]
///         ::shadow_point::invoke!($($t)*);
///     };
/// }
/// ```
///
/// ```text
/// // src/lib.rs — textual macro scope: the seam module must precede every
/// // module that calls sp_invoke!; an explicit `use` per module works too
/// #[macro_use]
/// mod seam;
/// ```
///
/// Call sites then drop the crate path entirely:
/// `sp_invoke!(MyModuleSp, before_insert(&key))`. In production the seam
/// expands to a stripped statement, so the crate is never resolved; in
/// tests it dispatches. Gating each call site with its own `#[cfg(test)]`
/// also works and needs no seam file, but the attribute is then repeated
/// per call. `define_sp!` needs no seam: the caller already gates it with
/// `#[cfg(test)]`. Check the seam holds with `cargo build` (must compile
/// while the dev-dependency is unresolvable) and `cargo test` (must
/// dispatch to the real hooks).
///
/// A seam can carry the whole fault-injection block, not just the crate
/// path, keeping its `#[cfg(test)]` out of production call sites. The hook
/// declares the flag as its last argument; the macro creates it, dispatches,
/// and runs the block the call site supplies:
///
/// ```text
/// macro_rules! sp_inject {
///     ($sp:path, $hook:ident($($arg:expr),* $(,)?), $body:block) => {
///         #[cfg(test)]
///         {
///             let __sp_inject = ::core::cell::Cell::new(false);
///             ::shadow_point::invoke!($sp, $hook($($arg,)* &__sp_inject));
///             if __sp_inject.get() {
///                 let _: () = $body; // rejects a value block; write `return`
///             }
///         }
///     };
/// }
///
/// sp_inject!(MapSp, before_alloc_node(capacity), { return Err(Full); });
/// ```
///
/// A block that produces a value is rejected: the injection path must
/// return, panic, abort, or fall through on purpose.
///
/// # Threads and async
///
/// Dispatch is per-thread: every worker that fires a hook must install the
/// sync point on its own thread first. A worker that installs nothing
/// silently no-ops. TLS follows the OS thread, not the task: a future resumed
/// on another worker after an `.await` fires into that thread's install
/// (usually none). Keep the instrumented section await-free, or run it under
/// `spawn_blocking` and install inside that closure. `TokioAsyncGate` waits
/// on fire milestones without parking a `current_thread` executor.
/// `TokioAsyncGate::wait_at_least` has no timeout by design: await only a
/// count the scenario is guaranteed to reach; a missed threshold hangs until
/// the CI job timeout, unlike `Gate::must_wait` and sequence parking.
#[macro_export]
macro_rules! invoke {
    ($module:path, $func:ident ( $($args:expr),* $(,)? )) => {
        {
            #[cfg(test)]
            {
                use $module as __sp;
                __sp::with_dyn(|__sp| __sp.$func($($args),*));
            }
        }
    };
}

// Reject hook names that collide with the fixed public surface: the
// registration methods (sequence/every/fires/verify), the module constructors
// (guard/shared), the dispatch fn (with_dyn), the gated-step constructor
// (when), and `clone` (shadows the generated `Clone` impl on `Shared`). The
// fixed machinery lives in a private submodule, so no other plain name can
// collide (D26). The 13 `__sp_`/`__SP_`-prefixed internal names of that
// machinery's fields and methods are also rejected, with a friendly
// diagnostic instead of a raw duplicate-definition error (9 public + 13
// private arms).
#[doc(hidden)]
#[macro_export]
macro_rules! __sp_reject_reserved {
    (sequence) => {
        ::core::compile_error!(
            "`sequence` is a reserved hook name: it collides with the generated \
             registration method `sequence`"
        );
    };
    (every) => {
        ::core::compile_error!(
            "`every` is a reserved hook name: it collides with the generated \
             registration method `every`"
        );
    };
    (fires) => {
        ::core::compile_error!(
            "`fires` is a reserved hook name: it collides with the generated \
             `fires` method and hook constant"
        );
    };
    (verify) => {
        ::core::compile_error!(
            "`verify` is a reserved hook name: it collides with the generated \
             `verify` method"
        );
    };
    (guard) => {
        ::core::compile_error!(
            "`guard` is a reserved hook name: it collides with the module \
             constructor `guard` in the value namespace"
        );
    };
    (shared) => {
        ::core::compile_error!(
            "`shared` is a reserved hook name: it collides with the module \
             constructor `shared` in the value namespace"
        );
    };
    (with_dyn) => {
        ::core::compile_error!(
            "`with_dyn` is a reserved hook name: it collides with the module \
             dispatch fn `with_dyn` in the value namespace"
        );
    };
    (when) => {
        ::core::compile_error!(
            "`when` is a reserved hook name: it collides with the gated-step \
             constructor `Seq::when`"
        );
    };
    (clone) => {
        ::core::compile_error!(
            "`clone` is a reserved hook name: it collides with the generated \
             `Clone` impl on `Shared` (a hook named `clone` would shadow \
             `Shared::clone`)"
        );
    };
    (__sp_new) => {
        ::core::compile_error!(
            "`__sp_new` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_state) => {
        ::core::compile_error!(
            "`__sp_state` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_from_owner) => {
        ::core::compile_error!(
            "`__sp_from_owner` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_value) => {
        ::core::compile_error!(
            "`__sp_value` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_calls) => {
        ::core::compile_error!(
            "`__sp_calls` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_every) => {
        ::core::compile_error!(
            "`__sp_every` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_expected) => {
        ::core::compile_error!(
            "`__sp_expected` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_installed_at) => {
        ::core::compile_error!(
            "`__sp_installed_at` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_reported) => {
        ::core::compile_error!(
            "`__sp_reported` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_sequence) => {
        ::core::compile_error!(
            "`__sp_sequence` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_sequence_cv) => {
        ::core::compile_error!(
            "`__sp_sequence_cv` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__sp_shared) => {
        ::core::compile_error!(
            "`__sp_shared` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    (__SP_NAMES) => {
        ::core::compile_error!(
            "`__SP_NAMES` is a reserved hook name: it is an internal name of the generated \
             machinery's private fields and methods"
        );
    };
    ($other:ident) => {};
}

// --- trace_enabled ---
/// Check whether sync-point tracing is enabled via the `SP_TRACE` environment
/// variable. When set, every hook fire prints its name and call index to
/// stderr — useful for discovering expected sequences and call counts.
pub fn trace_enabled() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    // The environment is process-global, so the answer is computed once and
    // cached.
    static ON: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| ON.store(std::env::var_os("SP_TRACE").is_some(), Ordering::Relaxed));
    ON.load(Ordering::Relaxed)
}

// --- Gate ---

/// A simple boolean gate for coordinating threads in concurrent tests.
///
/// Threads block in [`wait`](Self::wait) (or
/// [`must_wait`](Self::must_wait)) until another thread calls
/// [`set`](Self::set). Reusable: [`clear`](Self::clear) arms it for the
/// next wait.
///
/// # Completion, not just consumption
///
/// `sequence` releases a waiter when its entry is popped; the step closure
/// may still be running and may still hold production locks. A gate waits
/// for a step to actually finish: set it at the end of the step and wait on
/// it where the next action must not start.
///
/// Inside a hook or step closure use [`must_wait`](Self::must_wait), never
/// [`wait`](Self::wait): a missed [`set`](Self::set) must fail with a
/// diagnostic rather than hang the suite. [`wait`](Self::wait) belongs in
/// the test body. `wait_timeout`/`must_wait` accept any duration, zero
/// included: zero inspects the flag without waiting, and
/// `must_wait(Duration::ZERO)` fails the test at once when the gate is
/// unset. Shared-mode parking uses [`park_timeout()`], which honors
/// `SP_PARK_TIMEOUT_MS` and otherwise defaults to ten seconds.
///
/// The gate is panic-safe: a poisoned lock (a panicking waiter) is
/// transparently recovered via [`into_inner`](std::sync::PoisonError::into_inner).
#[derive(Debug, Default)]
pub struct Gate {
    flag: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl Gate {
    /// Create a new, unset gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.flag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the gate, waking every thread blocked in
    /// [`wait`](Self::wait) / [`wait_timeout`](Self::wait_timeout).
    pub fn set(&self) {
        let mut flag = self.lock();
        *flag = true;
        self.cv.notify_all();
    }

    /// Clear the gate so a subsequent [`wait`](Self::wait) blocks again.
    pub fn clear(&self) {
        *self.lock() = false;
    }

    /// Whether the gate is currently set.
    #[must_use]
    pub fn is_set(&self) -> bool {
        *self.lock()
    }

    /// Block until the gate is set. May hang forever if the gate is never
    /// set — prefer [`must_wait`](Self::must_wait) where a hang would be
    /// indistinguishable from a test bug.
    pub fn wait(&self) {
        let flag = self.lock();
        drop(
            self.cv
                .wait_while(flag, |set| !*set)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// Block until the gate is set or `timeout` elapses. Returns `true` if
    /// the gate was set. Spurious condvar wakeups do not terminate the wait
    /// early: the flag is re-checked by the predicate. The budget starts
    /// once the flag lock is held (`Condvar::wait_timeout_while`
    /// semantics), so the call may outlast `timeout` by the lock-acquisition
    /// time.
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        let flag = self.lock();
        let (flag, _) = self
            .cv
            .wait_timeout_while(flag, timeout, |set| !*set)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *flag
    }

    /// Block until the gate is set, panicking with a timeout diagnostic if
    /// `timeout` elapses first. Prefer over [`wait`](Self::wait) inside
    /// sequence-step closures: a missed [`set`](Self::set) fails the test
    /// with a message instead of hanging it.
    ///
    /// # Panics
    ///
    /// Panics with `shadow-point: gate wait timed out after {timeout:?}` if
    /// the gate is not set within `timeout`.
    pub fn must_wait(&self, timeout: std::time::Duration) {
        assert!(
            self.wait_timeout(timeout),
            "shadow-point: gate wait timed out after {timeout:?}"
        );
    }
}

// --- TokioAsyncGate (feature = "tokio-async") ---

/// Non-parking async milestone counter for tests whose instrumented code
/// runs on a tokio `current_thread` runtime.
///
/// This is the async half of the [`Gate`] contract and complements it, it
/// does not replace it: [`Gate`] is a level-triggered boolean rendezvous
/// for *synchronous* hook choreography (it parks the OS thread — fatal on
/// a single-thread executor, which must not lose its only thread), while
/// `TokioAsyncGate` is a monotonic fire-counter that the async test body waits
/// on cooperatively.
///
/// Contract: call [`fire`](Self::fire) from hook closures (non-blocking,
/// safe mid-dispatch on the executor thread); await
/// [`wait_at_least`](Self::wait_at_least) from the test body. Only pulls
/// `tokio` in when the `tokio-async` feature is enabled — default builds resolve
/// neither tokio nor this type.
#[cfg(feature = "tokio-async")]
#[derive(Clone, Debug, Default)]
pub struct TokioAsyncGate(std::sync::Arc<Inner>);

#[cfg(feature = "tokio-async")]
#[derive(Default, Debug)]
struct Inner {
    count: std::sync::atomic::AtomicUsize,
    woke: tokio::sync::Notify,
}

#[cfg(feature = "tokio-async")]
impl TokioAsyncGate {
    /// Create a new gate at count zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the milestone count and wake all registered waiters.
    /// Non-blocking — designed to be called from a hook closure running on
    /// the executor thread.
    pub fn fire(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.count.fetch_add(1, SeqCst);
        self.0.woke.notify_waiters();
    }

    /// Current fire count.
    #[must_use]
    pub fn count(&self) -> usize {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.count.load(SeqCst)
    }

    /// Await until `count >= n`.
    ///
    /// Registers the `Notify` wait slot (`enable()`) BEFORE reading the
    /// counter: a `fire()` racing into that window still wakes this waiter
    /// instead of losing the wakeup (the loop re-checks after waking).
    ///
    /// Each fire wakes every registered waiter, including ones whose
    /// threshold is not reached yet: they re-check and sleep again.
    ///
    /// Hangs if fewer than `n` fires ever occur (no timeout by design —
    /// CI job timeouts own that case); `count` wraps on usize overflow.
    pub async fn wait_at_least(&self, n: usize) {
        loop {
            let w = self.0.woke.notified();
            tokio::pin!(w);
            w.as_mut().enable();
            if self.count() >= n {
                return;
            }
            w.await;
        }
    }
}

// --- Fire info ---

/// Metadata about the hook fire currently being dispatched on this thread.
///
/// Returned by [`current_fire`] while a hook closure runs (fire-once, a
/// sequence step, or an `every` closure); `None` outside of any dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FireInfo {
    /// Name of the hook being dispatched.
    pub hook: &'static str,
    /// Zero-based index of this fire on this hook (global across
    /// threads). The closure runs on the firing thread, so
    /// `std::thread::current()` is the thread info — no fields for it.
    pub index: usize,
}

// `const {}` would be the idiomatic modern form, but inline const blocks
// are experimental until 1.79 and the crate targets MSRV 1.65.
thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_FIRE: std::cell::Cell<Option<FireInfo>> =
        std::cell::Cell::new(None);
}

/// Metadata about the hook fire currently being dispatched on this thread,
/// or `None` outside of a hook dispatch (e.g. in the main thread before any
/// hook fired, or after a dispatch finished).
#[must_use]
pub fn current_fire() -> Option<FireInfo> {
    CURRENT_FIRE.with(std::cell::Cell::get)
}

/// RAII guard that installs [`FireInfo`] as the current fire on this thread
/// and restores the previous value on drop (panic-safe and nesting-safe).
/// Hidden: used only from macro expansions generated by [`define_sp!`].
#[doc(hidden)]
#[must_use]
pub struct CurrentFire {
    prev: Option<FireInfo>,
}

impl CurrentFire {
    #[doc(hidden)]
    pub fn install(info: FireInfo) -> Self {
        let prev = CURRENT_FIRE.with(|c| c.replace(Some(info)));
        Self { prev }
    }
}

impl Drop for CurrentFire {
    fn drop(&mut self) {
        CURRENT_FIRE.with(|c| c.set(self.prev.take()));
    }
}

// --- Timeouts ---

/// The default park budget behind [`park_timeout()`]: ten seconds. How long
/// a thread parks in shared mode while the sequence head is a different hook.
/// Parking is a mutual-deadlock guard, not a synchronization mechanism:
/// threads re-check the sequence after every wake and park only as long as
/// the head still blocks them. A timeout therefore means the test's sequence
/// can never advance (no thread will ever fire the head hook) — failing is
/// better than hanging.
const DEFAULT_PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The "not resolved yet" marker in [`park_timeout`]'s cache. A millisecond
/// count equal to or above it can never be published, so the resolver rejects
/// it instead of returning an implausibly large timeout. `usize`, not `u64`:
/// the cache must compile on targets without 64-bit atomics (e.g. `armv5te`),
/// and on 32-bit the ceiling is ~49 days.
const UNRESOLVED_PARK_TIMEOUT: usize = usize::MAX;

/// The park timeout the shared-mode sequence actually uses: ten seconds by
/// default unless `SP_PARK_TIMEOUT_MS` names a number of milliseconds.
///
/// A library that silently failed to match the environment
/// would be a worse neighbor than one that reads it once. The env is read on
/// the first call and cached in an atomic, so the park path pays a relaxed
/// load per wake, not a `getenv` — and `unsafe` is not needed (the crate
/// forbids it, and MSRV 1.65 has no `OnceLock`).
///
/// A value that does not parse, is empty, or does not fit the internal
/// cache (`usize::MAX` milliseconds or more — ~49 days on a 32-bit target)
/// is ignored with one printed note: a typo in a CI env should not
/// silently change what the suite proves, and should not fail a run
/// either. `0` is legal and means "do not wait": the zero wait returns
/// already timed out, the head is re-checked, and the timeout diagnostic
/// is raised at once — so a wrong choreography fails instantly instead of
/// parking.
///
/// ```text
/// SP_PARK_TIMEOUT_MS=200 cargo test
/// ```
pub fn park_timeout() -> std::time::Duration {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // The resolved value in milliseconds, `UNRESOLVED_PARK_TIMEOUT` while
    // unresolved. A sentinel in one atomic rather than `Once` + a cell: the
    // cache is a single integer, two racers computing the same answer is
    // benign, and MSRV 1.65 has neither `OnceLock` nor a safe way to need
    // one here. The resolver never produces the sentinel, which is what
    // makes the fixed point below sound. `usize` keeps the cache compilable
    // on targets without 64-bit atomics; the resolver rejects a value that
    // would not fit.
    static CACHED: AtomicUsize = AtomicUsize::new(UNRESOLVED_PARK_TIMEOUT);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != UNRESOLVED_PARK_TIMEOUT {
        return std::time::Duration::from_millis(cached as u64);
    }
    let resolved = usize::try_from(resolve_park_timeout(std::env::var("SP_PARK_TIMEOUT_MS")))
        .expect("the resolver keeps the value below the sentinel");
    // Publish the winner, or adopt the value the first writer settled on, so
    // every thread in the process waits on one and the same timeout. Note the
    // shape of `compare_exchange`'s Ok arm: it yields the PREVIOUS value,
    // which on success is the unresolved sentinel — `unwrap_or_else` over it
    // would hand back the sentinel as if it were a timeout in milliseconds
    // (an absurdly long park, caught by the end-to-end test in
    // `tests/park_timeout_env.rs`, not by the resolver's unit tests).
    let winner = match CACHED.compare_exchange(
        UNRESOLVED_PARK_TIMEOUT,
        resolved,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => resolved,
        Err(current) => current,
    };
    std::time::Duration::from_millis(winner as u64)
}

/// The resolution rule, as a pure function of the env lookup: one value for
/// the whole process (see `park_timeout`'s cache), which is also why the
/// environment can only be tested in a separate binary.
fn resolve_park_timeout(raw: Result<String, std::env::VarError>) -> u64 {
    match raw {
        Err(std::env::VarError::NotPresent) => default_park_timeout_ms(),
        Err(_) => {
            note(format_args!(
                "SP_PARK_TIMEOUT_MS is not valid UTF-8, using the default {DEFAULT_PARK_TIMEOUT:?}"
            ));
            default_park_timeout_ms()
        }
        Ok(raw) => match raw.trim().parse::<u64>() {
            // The sentinel is the cache's own marker: publishing it would
            // both hand out an implausibly large timeout and leave the cache
            // permanently unresolved. On 32-bit a value at or above
            // `usize::MAX` would also fail to fit the cache, so the same
            // guard refuses it on every target.
            Ok(ms) if ms >= UNRESOLVED_PARK_TIMEOUT as u64 => {
                note(format_args!(
                    "SP_PARK_TIMEOUT_MS={ms} is too large to be a \
timeout, using the default {DEFAULT_PARK_TIMEOUT:?}"
                ));
                default_park_timeout_ms()
            }
            Ok(ms) => ms,
            Err(_) => {
                note(format_args!(
                    "SP_PARK_TIMEOUT_MS={raw:?} is not a number of milliseconds, \
using the default {DEFAULT_PARK_TIMEOUT:?}"
                ));
                default_park_timeout_ms()
            }
        },
    }
}

/// The default park budget in the milliseconds the cache stores. The constant
/// is ten seconds, so the conversion cannot fail; `try_from` keeps the cast
/// out of the code (and off clippy's truncation lint).
fn default_park_timeout_ms() -> u64 {
    u64::try_from(DEFAULT_PARK_TIMEOUT.as_millis())
        .expect("the default park budget, ten seconds in ms, fits in u64")
}

/// One diagnostic line on stderr, best-effort. `eprintln!` panics when the
/// write fails, and a warning about a misconfigured env must not fail the
/// run it is warning about — the same reason the unwind path writes through
/// `writeln!` and ignores the result.
fn note(msg: std::fmt::Arguments<'_>) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "shadow-point: {msg}");
}

// See CURRENT_FIRE: inline `const {}` blocks are experimental until 1.79.
thread_local! {
    // Addresses of `__Sp` instances currently dispatching a hook on this
    // thread. Per-thread: a genuinely concurrent fire from another thread is
    // never suppressed, and `contains` still gives per-sync-point re-entry
    // suppression that survives transitive nesting (A -> B -> A).
    #[allow(clippy::missing_const_for_thread_local)]
    static EXEC_STACK: std::cell::RefCell<Vec<*const ()>> =
        std::cell::RefCell::new(Vec::new());
}

/// RAII guard that marks a sync point as executing on this thread for the
/// duration of a hook dispatch (panic-safe). Hidden: used only from macro
/// expansions generated by [`define_sp!`].
///
/// Re-entry suppression is per-thread and per-sync-point: a nested
/// dispatch of any hook of this sync point from the same thread is
/// suppressed, while a genuinely concurrent fire from another thread
/// always proceeds.
#[doc(hidden)]
#[must_use]
pub struct ExecSink(*const ());

impl ExecSink {
    #[doc(hidden)]
    #[must_use]
    pub fn try_enter(sp: *const ()) -> Option<Self> {
        EXEC_STACK.with(|s| {
            let mut stack = s.borrow_mut();
            if stack.contains(&sp) {
                None
            } else {
                stack.push(sp);
                Some(Self(sp))
            }
        })
    }
}

impl Drop for ExecSink {
    fn drop(&mut self) {
        EXEC_STACK.with(|s| {
            let popped = s.borrow_mut().pop();
            // Dispatches on one thread are strictly nested (RAII), so the
            // top of the stack must be this dispatch's own entry.
            debug_assert_eq!(popped, Some(self.0), "ExecSink stack discipline violated");
        });
    }
}
// --- define_sp! macro ---

/// Generate a sync-point module from a hook list.
///
/// `define_sp! { $vis mod $name { hook(args), ... } }` emits a module with
/// fixed names: two constructors (`guard`, `shared`), one `Guard` type for
/// both modes, a clonable `Shared`, a hook-identifier constant per hook,
/// and the hidden dispatch machinery. Several sync points with distinct
/// module names can share a file directly.
///
/// # Surface
///
/// | Item | Role |
/// |---|---|
/// | `guard(value)` | private mode: install on this thread, returns `Guard<T>` |
/// | `shared(value)` | shared mode: returns `Shared<T>` (`Clone`, `Send + Sync`) |
/// | `Guard<T>` | `Deref`, fire-once/`sequence`/`every`/`fires`/`verify`, `!Send` |
/// | `Shared<T>` | `Deref`, `guard()` to attach a worker thread, same registration |
/// | `Hook` | hook-identifier type; one constant per hook (`<hook>: Hook`) |
///
/// # Example
///
/// `no_run`: the example type-checks but is not executed — `invoke!` is
/// `#[cfg(test)]`-only and expands to nothing in a doctest, so the fires
/// would not happen and `verify()` would fail. See
/// `examples/register_private.rs` for a runnable version.
///
/// ```no_run
/// shadow_point::define_sp! {
///     pub mod array_sp {
///         entry_before_latch(path: &str, found_idx: Option<usize>),
///         before_drain_latch(),
///     }
/// }
///
/// let g = array_sp::guard(vec![1, 2, 3]);
/// g.entry_before_latch(|a, path, idx| {
///     assert_eq!(a.len(), 3);
///     assert_eq!(path, "root");
///     assert_eq!(idx, Some(0));
/// }).fires(2);
/// g.before_drain_latch(|a| { assert_eq!(a.len(), 3); });
///
/// shadow_point::invoke!(array_sp, entry_before_latch("root", Some(0)));
/// shadow_point::invoke!(array_sp, entry_before_latch("root", Some(0)));
/// shadow_point::invoke!(array_sp, before_drain_latch());
/// g.verify();
/// ```
///
/// # Registration
///
/// - one-shot: `g.<hook>(f)` — the hook must fire exactly once; the closure
///   runs at most once, on the fire whose per-hook counter was 0 and whose
///   sequence fell through to fire-once. Chain `.fires(n)` for a different
///   count; both `fires` forms return the registration handle (`&Guard` or
///   `&Shared`) so registrations chain.
/// - ordered: `g.sequence(|s| { s.a(f).b(g); })` — entries consumed in order;
///   a gated step is `s.when(pred).<hook>(f)`.
/// - every fire: `g.every(|e| e.a(f))` — runs on every non-suppressed fire.
/// - count only: `g.fires(<hook>, n)` — no closure.
///
/// # Reserved hook names
///
/// `sequence`, `every`, `fires`, `verify`, `guard`, `shared`, `with_dyn`,
/// `when`, and `clone` collide with the fixed public surface (`clone` would
/// shadow the generated `Clone` impl on `Shared`), and the 13
/// `__sp_`/`__SP_` internal names belong to the generated machinery's private
/// fields and methods; `define_sp!` rejects all of them with a
/// `compile_error!` naming the hook. All other names — including `value`,
/// `calls`, `expected`, `state`, `new`, `hook`, `f`, `NAMES` — are legal: the
/// fixed machinery lives in a private submodule and cannot collide with a
/// hook name.
///
/// # Hook arguments
///
/// Arguments are passed by value through dispatch and must be `Copy`:
/// declare a reference (`key: &K`) or a raw pointer (`root: *const ()`) when
/// the call site must keep the value. A non-`Copy` argument is rejected at
/// the declaration. The closure signature is `|&T, args…|`.
///
/// # Threads
///
/// Dispatch is per-thread: every worker that fires a hook must attach with
/// `shared.guard()` on its own thread first. `Guard` is `!Send` — drop it on
/// the thread that created it. See the README for the full ownership and
/// panic/drop contract.
///
/// Moving a guard across threads is a compile error, not a documented trap:
///
/// ```compile_fail
/// shadow_point::define_sp! {
///     mod send_probe { ping(v: u32), }
/// }
/// let g = send_probe::guard(0u32);
/// std::thread::spawn(move || {
///     drop(g); // ERROR: `Guard<u32>` cannot be sent between threads
/// });
/// ```
#[macro_export]
macro_rules! define_sp {
    (
        $vis:vis mod $name:ident
        {
            $( $method:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? ) ),+ $(,)?
        }
    ) => {
        #[doc = concat!(
            "Sync point `", stringify!($name), "`: generated by `define_sp!`. \
             Constructors `guard`/`shared`, one `Guard` type, a hook constant \
             per declared hook. See the crate README for the full contract."
        )]
        #[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
        $vis mod $name {
            // ── Reserved hook names (the fixed public surface) ───────
            const _: () = {
                $( $crate::__sp_reject_reserved!($method); )+
            };

            // ── Hook identifiers ─────────────────────────────────────

            /// Identifiers for this sync point's hooks, one variant per
            /// declared hook. The type of the module's hook constants and
            /// the argument of `fires`.
            #[repr(usize)]
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            #[allow(non_camel_case_types)]
            pub enum Hook {
                $(
                    #[doc = concat!("The `", stringify!($method), "` hook.")]
                    $method,
                )+
            }

            impl Hook {
                /// Hook names indexed by discriminant, for diagnostics.
                /// `__sp_`/`__SP_`-prefixed: a hook named `NAMES` would otherwise
                /// shadow this associated constant (`Hook::NAMES[i]` would
                /// index the hook variant).
                const __SP_NAMES: &'static [&'static str] = &[$(stringify!($method)),+];
                /// The hook's own name, for diagnostics. `__sp_`/`__SP_`-prefixed:
                /// a hook named `name` would otherwise collide with this
                /// method in the method namespace.
                fn __sp_name(&self) -> &'static str {
                    Self::__SP_NAMES[*self as usize]
                }
            }

            $(
                #[doc = concat!("The `", stringify!($method), "` hook identifier for `fires`.")]
                #[allow(non_upper_case_globals)]
                pub const $method: Hook = Hook::$method;
            )+

            // ── Fixed machinery (private submodule) ──────────────────
            //
            // Everything with a fixed name lives here, so a hook name can
            // never collide with it in the value or method namespace. The
            // per-hook operations are free functions; dispatch lives on
            // Owner, which receives no generated hook methods at this scope.

            #[doc(hidden)]
            #[allow(dead_code, non_camel_case_types, non_snake_case)]
            mod __sp_impl {
                use super::Hook;

                // Dispatch trait: erases the guarded value type so one
                // thread-local slot serves any `T`.
                pub trait Hooks: Send + Sync {
                    $(
                        fn $method(&self $( , _: $arg_ty)*) {
                            // D29: reject non-Copy hook arguments at the
                            // declaration. A nested fn per argument gives
                            // each its own scope; the const item's E0277
                            // spans the declared type token.
                            $(
                                fn $arg() {
                                    fn needs_copy<T: Copy>() {}
                                    const _CHECK: fn() = needs_copy::<$arg_ty>;
                                }
                            )*
                        }
                    )+
                }

                struct Default;
                impl Hooks for Default {}
                static DEFAULT: Default = Default;

                thread_local! {
                    static TL: ::std::cell::RefCell<
                        Option<::std::sync::Arc<dyn Hooks>>
                    > = ::std::cell::RefCell::new(None);
                }

                /// Dispatch a closure with the sync point installed on this
                /// thread. Used by `invoke!`.
                #[inline]
                pub fn with_dyn<R>(
                    f: impl ::std::ops::FnOnce(&dyn Hooks) -> R,
                ) -> R {
                    // Clone out and release the borrow BEFORE the call: a
                    // hook that installs/drops a guard takes this same slot
                    // mutably, and the clone keeps the state alive for the
                    // whole fire.
                    let held = TL.with(|c| c.borrow().clone());
                    match held {
                        Some(sp) => f(&*sp),
                        None => f(&DEFAULT),
                    }
                }

                // ── Sequence entries ─────────────────────────────────

                pub enum SeqAction<T: Send + Sync + 'static> {
                    $( $method(
                        Option<Box<dyn Fn($($arg_ty),*) -> bool + Send>>,
                        Box<dyn FnOnce(&T $( , $arg_ty)*) + Send>,
                    ), )+
                }

                impl<T: Send + Sync + 'static> SeqAction<T> {
                    fn hook(&self) -> Hook {
                        match self {
                            $( Self::$method(..) => Hook::$method, )+
                        }
                    }
                }

                pub struct SeqEntry<T: Send + Sync + 'static> {
                    action: SeqAction<T>,
                }

                impl<T: Send + Sync + 'static> SeqEntry<T> {
                    fn matches(&self, h: Hook) -> bool {
                        self.action.hook() == h
                    }
                    fn name(&self) -> &'static str {
                        self.action.hook().__sp_name()
                    }
                }

                // ── Every-fire closures ──────────────────────────────

                /// The `every` registration builder.
                pub struct Every<T: Send + Sync + 'static> {
                    $( $method: Option<
                        ::std::sync::Arc<dyn Fn(&T $( , $arg_ty)*) + Send + Sync>
                    >, )+
                }

                impl<T: Send + Sync + 'static> Every<T> {
                    $(
                        /// Register a closure that runs on every fire of this hook.
                        #[allow(clippy::type_complexity)]
                        pub fn $method(
                            &mut self,
                            f: impl Fn(&T $( , $arg_ty)*) + Send + Sync + 'static,
                        ) -> &mut Self {
                            self.$method = Some(::std::sync::Arc::new(f));
                            self
                        }
                    )+
                }

                // ── Sequence builder ─────────────────────────────────

                /// The `sequence` registration builder.
                pub struct Seq<T: Send + Sync + 'static> {
                    seq: ::std::collections::VecDeque<SeqEntry<T>>,
                }

                impl<T: Send + Sync + 'static> Seq<T> {
                    $(
                        /// Push a sequence entry for this hook.
                        pub fn $method(
                            &mut self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + Send + 'static,
                        ) -> &mut Self {
                            self.seq.push_back(SeqEntry {
                                action: SeqAction::$method(None, Box::new(f)),
                            });
                            self
                        }
                    )+

                    /// Start a predicate-gated step: `s.when(pred).<hook>(f)`.
                    /// The predicate is stored untyped here; each `.<hook>`
                    /// on the returned handle adds the bound for that hook's
                    /// arguments, so a single-hook predicate need only
                    /// satisfy its own signature.
                    pub fn when<P>(&mut self, pred: P) -> When<'_, T, P> {
                        When { seq: self, pred }
                    }
                }

                /// The gated-step sub-builder returned by `Seq::when`.
                pub struct When<'a, T: Send + Sync + 'static, P> {
                    seq: &'a mut Seq<T>,
                    pred: P,
                }

                impl<'a, T: Send + Sync + 'static, P> When<'a, T, P> {
                    $(
                        /// Push a predicate-gated sequence entry for this hook.
                        /// The bound on `P` is applied here, at hook
                        /// selection, so it covers only this hook's argument
                        /// signature.
                        pub fn $method(
                            self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + Send + 'static,
                        ) -> &'a mut Seq<T>
                        where
                            P: Fn($($arg_ty),*) -> bool + Send + 'static,
                        {
                            self.seq.seq.push_back(SeqEntry {
                                action: SeqAction::$method(Some(Box::new(self.pred)), Box::new(f)),
                            });
                            self.seq
                        }
                    )+
                }

                // ── State ────────────────────────────────────────────

                pub struct State<T: Send + Sync + 'static> {
                    pub __sp_value: T,
                    $(
                        pub $method: ::std::sync::Mutex<
                            Option<Box<dyn FnOnce(&T $( , $arg_ty)*) + Send>>
                        >,
                    )+
                    __sp_every: ::std::sync::Mutex<Every<T>>,
                    __sp_sequence: ::std::sync::Mutex<
                        ::std::collections::VecDeque<SeqEntry<T>>
                    >,
                    __sp_sequence_cv: ::std::sync::Condvar,
                    __sp_shared: bool,
                    __sp_installed_at: &'static std::panic::Location<'static>,
                    __sp_calls: ::std::vec::Vec<::std::sync::atomic::AtomicUsize>,
                    __sp_expected: ::std::sync::Mutex<::std::vec::Vec<Option<usize>>>,
                    __sp_reported: ::std::sync::atomic::AtomicBool,
                }

                impl<T: Send + Sync + 'static> State<T> {
                    pub fn __sp_new(
                        value: T,
                        shared: bool,
                        installed_at: &'static std::panic::Location<'static>,
                    ) -> Self {
                        let n = [$( stringify!($method) ),+].len();
                        Self {
                            __sp_value: value,
                            $( $method: ::std::sync::Mutex::new(None), )+
                            __sp_every: ::std::sync::Mutex::new(Every {
                                $( $method: None, )+
                            }),
                            __sp_sequence: ::std::sync::Mutex::new(
                                ::std::collections::VecDeque::new()
                            ),
                            __sp_sequence_cv: ::std::sync::Condvar::new(),
                            __sp_shared: shared,
                            __sp_installed_at: installed_at,
                            __sp_calls: (0..n)
                                .map(|_| ::std::sync::atomic::AtomicUsize::new(0))
                                .collect(),
                            __sp_expected: ::std::sync::Mutex::new(
                                (0..n).map(|_| None).collect()
                            ),
                            __sp_reported: ::std::sync::atomic::AtomicBool::new(false),
                        }
                    }

                    $(
                        pub fn $method(
                            &self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + Send + 'static,
                        ) {
                            *self.$method.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(Box::new(f));
                            set_expected_if_unset(self, Hook::$method, 1);
                        }
                    )+
                }

                // ── Per-hook operations (free functions) ─────────────

                pub fn bump_call<T: Send + Sync + 'static>(
                    st: &State<T>, h: Hook,
                ) -> usize {
                    st.__sp_calls[h as usize].fetch_add(
                        1, ::std::sync::atomic::Ordering::SeqCst,
                    )
                }

                pub fn set_expected<T: Send + Sync + 'static>(
                    st: &State<T>, h: Hook, n: usize,
                ) {
                    st.__sp_expected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        [h as usize] = Some(n);
                }

                fn set_expected_if_unset<T: Send + Sync + 'static>(
                    st: &State<T>, h: Hook, n: usize,
                ) {
                    let mut expected = st
                        .__sp_expected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let slot = &mut expected[h as usize];
                    if slot.is_none() {
                        *slot = Some(n);
                    }
                }

                pub fn build_sequence<T: Send + Sync + 'static>(
                    st: &State<T>, f: impl FnOnce(&mut Seq<T>),
                ) {
                    let mut builder = Seq {
                        seq: ::std::collections::VecDeque::new(),
                    };
                    f(&mut builder);
                    *st.__sp_sequence.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        builder.seq;
                }

                pub fn set_every<T: Send + Sync + 'static>(
                    st: &State<T>, f: impl FnOnce(&mut Every<T>),
                ) {
                    let mut every = Every { $( $method: None, )+ };
                    f(&mut every);
                    *st.__sp_every.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = every;
                }

                fn report_while_unwinding(message: &str) {
                    use ::std::io::Write;
                    let _ = writeln!(
                        std::io::stderr(),
                        "shadow-point: {message} (while unwinding)"
                    );
                }

                #[track_caller]
                pub fn assert_fires<T: Send + Sync + 'static>(
                    st: &State<T>, from_drop: bool,
                ) {
                    if from_drop
                        && st.__sp_reported.load(::std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    let unwinding = std::thread::panicking();
                    let shared = st.__sp_shared;
                    let seq = st
                        .__sp_sequence
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !seq.is_empty() {
                        let message = format!(
                            "sync point sequence not fully consumed: \
                             {} entr{} remaining [{}] ({}installed at {})",
                            seq.len(),
                            if seq.len() == 1 { "y" } else { "ies" },
                            seq.iter()
                                .map(|e| e.name())
                                .collect::<Vec<_>>()
                                .join(", "),
                            if shared { "shared, " } else { "" },
                            st.__sp_installed_at,
                        );
                        drop(seq);
                        st.__sp_reported.store(true, ::std::sync::atomic::Ordering::SeqCst);
                        if unwinding {
                            report_while_unwinding(&message);
                        } else {
                            panic!("{message}");
                        }
                    } else {
                        drop(seq);
                    }
                    let expected = st
                        .__sp_expected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for (i, expected) in expected.iter().enumerate() {
                        let Some(expected) = expected else { continue };
                        let actual = st.__sp_calls[i]
                            .load(::std::sync::atomic::Ordering::SeqCst);
                        if actual == *expected {
                            continue;
                        }
                        let message = format!(
                            "sync point hook `{}` fired {actual} \
                             time(s), expected {expected} ({}installed at {})",
                            Hook::__SP_NAMES[i],
                            if shared { "shared, " } else { "" },
                            st.__sp_installed_at,
                        );
                        st.__sp_reported.store(true, ::std::sync::atomic::Ordering::SeqCst);
                        if unwinding {
                            report_while_unwinding(&message);
                        } else {
                            assert_eq!(actual, *expected, "{message}");
                        }
                    }
                }

                // ── Owner: the single assertion/reclamation point ────

                pub struct Owner<T: Send + Sync + 'static> {
                    state: Option<::std::sync::Arc<State<T>>>,
                }

                impl<T: Send + Sync + 'static> Owner<T> {
                    pub fn __sp_new(state: ::std::sync::Arc<State<T>>) -> Self {
                        Self { state: Some(state) }
                    }

                    pub fn __sp_state(&self) -> &State<T> {
                        self.state
                            .as_ref()
                            .expect("shadow-point: owner used after drop")
                    }
                }

                impl<T: Send + Sync + 'static> Hooks for Owner<T> {
                    $(
                        fn $method(&self $( , $arg: $arg_ty)*) {
                            let st = self.__sp_state();
                            let addr = st as *const State<T> as *const ();
                            let Some(_e) = $crate::ExecSink::try_enter(addr) else {
                                return;
                            };
                            let h = Hook::$method;
                            let idx = bump_call(st, h);
                            if $crate::trace_enabled() {
                                eprintln!(
                                    "[sp] {} call #{} thread={}",
                                    h.__sp_name(),
                                    idx + 1,
                                    std::thread::current()
                                        .name()
                                        .unwrap_or("<unnamed>"),
                                );
                            }
                            let _f = $crate::CurrentFire::install($crate::FireInfo {
                                hook: h.__sp_name(),
                                index: idx,
                            });
                            let every = {
                                let guard = st
                                    .__sp_every
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                guard.$method.clone()
                            };
                            if let Some(f) = every {
                                f(&st.__sp_value $( , $arg)*);
                            }
                            let (head_expects, consumed) = {
                                let mut seq = st
                                    .__sp_sequence
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let mut timed_out_last = false;
                                loop {
                                    match seq.front() {
                                        None => break (false, None),
                                        Some(front) if front.matches(h) => {
                                            let pass = match &front.action {
                                                SeqAction::$method(Some(pred), _) => {
                                                    pred($($arg),*)
                                                }
                                                SeqAction::$method(None, _) => true,
                                                _ => true,
                                            };
                                            if pass {
                                                let entry = seq.pop_front();
                                                st.__sp_sequence_cv.notify_all();
                                                break (true, entry.map(|e| e.action));
                                            }
                                            break (true, None);
                                        }
                                        Some(front) => {
                                            let has_more =
                                                seq.iter().any(|e| e.matches(h));
                                            if !has_more {
                                                break (false, None);
                                            }
                                            if st.__sp_shared {
                                                if timed_out_last {
                                                    panic!(
                                                        "shadow-point: sequence park \
                                                         timeout ({:?}): thread waiting \
                                                         for `{}`, head is `{}` \
                                                         (installed at {})",
                                                        $crate::park_timeout(),
                                                        h.__sp_name(),
                                                        front.name(),
                                                        st.__sp_installed_at,
                                                    );
                                                }
                                                let (g, w) = st
                                                    .__sp_sequence_cv
                                                    .wait_timeout(
                                                        seq,
                                                        $crate::park_timeout(),
                                                    )
                                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                                seq = g;
                                                timed_out_last = w.timed_out();
                                                continue;
                                            }
                                            panic!(
                                                "sync point ordering violation: \
                                                 expected `{}` next but `{}` fired",
                                                front.name(),
                                                h.__sp_name()
                                            );
                                        }
                                    }
                                }
                            };
                            match consumed {
                                Some(SeqAction::$method(_, f)) => {
                                    f(&st.__sp_value $( , $arg)*);
                                }
                                Some(_) => unreachable!(
                                    "shadow-point: consumed a non-matching sequence entry"
                                ),
                                None => {
                                    if idx == 0 && !head_expects {
                                        let fire_once = {
                                            let mut slot = st
                                                .$method
                                                .lock()
                                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                                            slot.take()
                                        };
                                        if let Some(f) = fire_once {
                                            f(&st.__sp_value $( , $arg)*);
                                        }
                                    }
                                }
                            }
                        }
                    )+
                }

                impl<T: Send + Sync + 'static> Drop for Owner<T> {
                    fn drop(&mut self) {
                        // The single assertion/reclamation point (D24). Take
                        // the state into a ManuallyDrop so a panic out of the
                        // assertions leaks it instead of running consumer
                        // Drop mid-unwind.
                        let state = std::mem::ManuallyDrop::new(
                            self.state
                                .take()
                                .expect("shadow-point: owner dropped twice"),
                        );
                        assert_fires(&state, true);
                        if std::thread::panicking() {
                            return;
                        }
                        drop(std::mem::ManuallyDrop::into_inner(state));
                    }
                }

                // ── Fire-once handle ─────────────────────────────────

                /// The handle returned by a fire-once registration;
                /// `.fires(n)` pins the count and returns the registration
                /// handle for chaining.
                pub struct Expect<'a, T: Send + Sync + 'static, R> {
                    reg: &'a R,
                    state: &'a State<T>,
                    hook: Hook,
                }

                impl<'a, T: Send + Sync + 'static, R> Expect<'a, T, R> {
                    fn __sp_new(reg: &'a R, state: &'a State<T>, hook: Hook) -> Self {
                        Self { reg, state, hook }
                    }

                    /// Assert the hook fires exactly `n` times; returns the
                    /// registration handle.
                    pub fn fires(self, n: usize) -> &'a R {
                        set_expected(self.state, self.hook, n);
                        self.reg
                    }
                }

                // ── Guard ────────────────────────────────────────────

                /// The sync-point guard for both private and shared mode.
                /// Derefs to the guarded value; drop asserts counts and
                /// sequence. `!Send`: drop it on the thread that created it.
                #[clippy::has_significant_drop]
                #[must_use]
                pub struct Guard<T: Send + Sync + 'static> {
                    owner: ::std::sync::Arc<Owner<T>>,
                    old: Option<::std::sync::Arc<dyn Hooks>>,
                    _not_send: ::std::marker::PhantomData<*const ()>,
                }

                impl<T: Send + Sync + 'static> Guard<T> {
                    fn __sp_state(&self) -> &State<T> {
                        self.owner.__sp_state()
                    }

                    $(
                        /// Register a fire-once closure for this hook; chain
                        /// `.fires(n)` for a different count.
                        pub fn $method(
                            &self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + Send + 'static,
                        ) -> Expect<'_, T, Self> {
                            self.__sp_state().$method(f);
                            Expect::__sp_new(self, self.__sp_state(), Hook::$method)
                        }
                    )+

                    /// Register an ordered sequence of hook fires.
                    pub fn sequence(&self, f: impl FnOnce(&mut Seq<T>)) {
                        build_sequence(self.__sp_state(), f);
                    }

                    /// Register closures that fire on every invocation.
                    pub fn every(&self, f: impl FnOnce(&mut Every<T>)) {
                        set_every(self.__sp_state(), f);
                    }

                    /// Assert that `hook` fires exactly `n` times before drop.
                    pub fn fires(&self, hook: Hook, n: usize) -> &Self {
                        set_expected(self.__sp_state(), hook, n);
                        self
                    }

                    /// Run the drop assertions now, at the caller's location.
                    #[track_caller]
                    pub fn verify(&self) {
                        assert_fires(self.__sp_state(), false);
                    }
                }

                impl<T: Send + Sync + 'static> std::ops::Deref for Guard<T> {
                    type Target = T;
                    fn deref(&self) -> &T {
                        &self.__sp_state().__sp_value
                    }
                }

                impl<T: Send + Sync + 'static> Drop for Guard<T> {
                    fn drop(&mut self) {
                        // Restore TLS first, with the borrow dropped before
                        // the Arc<Owner> field releases — the assertion (in
                        // Owner::drop) then runs after every TLS install is
                        // gone.
                        TL.with(|c| {
                            *(c.borrow_mut()) = self.old.take()
                        });
                    }
                }

                // ── Shared ───────────────────────────────────────────

                /// A sync point shared across threads. `Clone` gives each
                /// worker its own handle; `guard()` attaches the calling
                /// thread.
                #[clippy::has_significant_drop]
                #[must_use]
                pub struct Shared<T: Send + Sync + 'static> {
                    owner: ::std::sync::Arc<Owner<T>>,
                }

                impl<T: Send + Sync + 'static> Clone for Shared<T> {
                    fn clone(&self) -> Self {
                        Self { owner: ::std::sync::Arc::clone(&self.owner) }
                    }
                }

                impl<T: Send + Sync + 'static> Shared<T> {
                    fn __sp_from_owner(owner: ::std::sync::Arc<Owner<T>>) -> Self {
                        Self { owner }
                    }

                    fn __sp_state(&self) -> &State<T> {
                        self.owner.__sp_state()
                    }

                    /// Attach this sync point to the calling thread,
                    /// returning a guard that restores the previous install
                    /// on drop.
                    #[track_caller]
                    pub fn guard(&self) -> Guard<T> {
                        attach(::std::sync::Arc::clone(&self.owner))
                    }

                    $(
                        /// Register a fire-once closure for this hook.
                        pub fn $method(
                            &self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + Send + 'static,
                        ) -> Expect<'_, T, Self> {
                            self.__sp_state().$method(f);
                            Expect::__sp_new(self, self.__sp_state(), Hook::$method)
                        }
                    )+

                    /// Register an ordered sequence of hook fires across threads.
                    pub fn sequence(&self, f: impl FnOnce(&mut Seq<T>)) {
                        build_sequence(self.__sp_state(), f);
                    }

                    /// Register closures that fire on every invocation.
                    pub fn every(&self, f: impl FnOnce(&mut Every<T>)) {
                        set_every(self.__sp_state(), f);
                    }

                    /// Assert that `hook` fires exactly `n` times in total.
                    pub fn fires(&self, hook: Hook, n: usize) -> &Self {
                        set_expected(self.__sp_state(), hook, n);
                        self
                    }

                    /// Run the drop assertions now (after joining workers).
                    #[track_caller]
                    pub fn verify(&self) {
                        assert_fires(self.__sp_state(), false);
                    }
                }

                impl<T: Send + Sync + 'static> std::ops::Deref for Shared<T> {
                    type Target = T;
                    fn deref(&self) -> &T {
                        &self.__sp_state().__sp_value
                    }
                }

                // ── TLS attach (shared by both constructors) ─────────

                pub fn attach<T: Send + Sync + 'static>(
                    owner: ::std::sync::Arc<Owner<T>>,
                ) -> Guard<T> {
                    // `.clone()` at the binding, not `Arc::clone`: the
                    // unsizing coercion to `Arc<dyn Hooks>` happens at the
                    // binding.
                    let handle: ::std::sync::Arc<dyn Hooks> = owner.clone();
                    let old = TL.with(|c| {
                        let mut slot = c.borrow_mut();
                        slot.replace(handle)
                    });
                    Guard { owner, old, _not_send: ::std::marker::PhantomData }
                }

                // Constructors live here, not in the public module: the
                // parameter `value` would otherwise collide with a hook
                // constant named `value` in the module's value namespace.
                /// Install a private guard driving this sync point for `value`.
                #[track_caller]
                pub fn guard<T: Send + Sync + 'static>(value: T) -> Guard<T> {
                    let owner = ::std::sync::Arc::new(Owner::__sp_new(
                        ::std::sync::Arc::new(State::__sp_new(
                            value,
                            false,
                            std::panic::Location::caller(),
                        )),
                    ));
                    attach(owner)
                }

                /// Create a sync point shared across threads.
                #[track_caller]
                pub fn shared<T: Send + Sync + 'static>(value: T) -> Shared<T> {
                    Shared::__sp_from_owner(::std::sync::Arc::new(Owner::__sp_new(
                        ::std::sync::Arc::new(State::__sp_new(
                            value,
                            true,
                            std::panic::Location::caller(),
                        )),
                    )))
                }
            }

            // ── Public surface (re-exports + constructors) ───────────

            #[doc(inline)]
            pub use __sp_impl::{guard, shared, Guard, Shared};
            #[doc(hidden)]
            pub use __sp_impl::{with_dyn, Every, Expect, Seq, When};
        }
    };
}

/// Probe that the generated public items are documented.
///
/// `#[cfg(test)]` so it never enters the published API, and *not*
/// `#[doc(hidden)]` because the lint skips hidden items: the point is to
/// make `define_sp!`'s output publicly reachable under this crate's own
/// `deny(missing_docs)`. A generated item that loses its doc comment
/// breaks `cargo test` here instead of breaking a consumer's build three
/// projects away.
#[cfg(test)]
pub mod sp_docs {
    define_sp! {
        pub mod docs_sp {
            before_probe(key: &u32),
            after_probe(len: usize),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn park_timeout_defaults_to_the_constant() {
        assert_eq!(
            Duration::from_millis(resolve_park_timeout(Err(std::env::VarError::NotPresent))),
            DEFAULT_PARK_TIMEOUT
        );
    }

    #[test]
    fn park_timeout_takes_the_env_value() {
        assert_eq!(resolve_park_timeout(Ok("250".into())), 250);
        assert_eq!(resolve_park_timeout(Ok(" 250 ".into())), 250);
        assert_eq!(resolve_park_timeout(Ok("0".into())), 0);
    }

    /// The cached accessor, pinned separately from the resolution rule: the
    /// value that comes out of `park_timeout` is what a park waits on, and
    /// the first version of the cache returned its own "unresolved" sentinel
    /// as a timeout (~584 million years). The pure resolver cannot see that
    /// bug; only a test on the cached path can.
    ///
    /// The bound mirrors the env this process started with, so a legitimate
    /// `SP_PARK_TIMEOUT_MS` on the runner is honored, not flagged — the same
    /// rule `the_cache_honors_what_the_process_started_with` pins from the
    /// other side. Only a value the resolver could not have produced (the
    /// sentinel, wrapped, or not a number of milliseconds this process
    /// parsed) is a cache bug.
    #[test]
    fn park_timeout_never_returns_the_unresolved_sentinel() {
        let expected = match std::env::var("SP_PARK_TIMEOUT_MS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(ms) if ms < UNRESOLVED_PARK_TIMEOUT as u64 => Duration::from_millis(ms),
                // The sentinel is refused, so the cache must hold the default.
                _ => DEFAULT_PARK_TIMEOUT,
            },
            Err(_) => DEFAULT_PARK_TIMEOUT,
        };
        let t = park_timeout();
        assert_eq!(
            t, expected,
            "the cache resolved to something the env does not describe"
        );
        assert_eq!(park_timeout(), t, "resolved once, and stable");
    }

    /// The same sentinel, reached through the front door instead of through
    /// the cache: an env value equal to the marker must not be published as
    /// a timeout (the cache would then never resolve at all).
    #[test]
    fn a_value_colliding_with_the_unresolved_sentinel_falls_back() {
        assert_eq!(
            Duration::from_millis(resolve_park_timeout(
                Ok(UNRESOLVED_PARK_TIMEOUT.to_string())
            )),
            DEFAULT_PARK_TIMEOUT,
            "the cache's marker is never a timeout"
        );
    }

    /// The cache's boundary is exclusive, and the value just below it is
    /// still a legal timeout. The upper half is reachable only where
    /// `usize` is narrower than `u64` (the 32-bit `test32` CI job); on a
    /// 64-bit host `checked_add` returns `None` and only the lower half
    /// runs. This is the test that distinguishes `>=` from `==` at the
    /// boundary: the exact-sentinel test above cannot.
    #[test]
    fn the_cache_boundary_is_exclusive() {
        let below = UNRESOLVED_PARK_TIMEOUT - 1;
        assert_eq!(
            resolve_park_timeout(Ok(below.to_string())),
            below as u64,
            "the value just below the cache's capacity must be accepted"
        );
        if let Some(above) = (UNRESOLVED_PARK_TIMEOUT as u64).checked_add(1) {
            assert_eq!(
                Duration::from_millis(resolve_park_timeout(Ok(above.to_string()))),
                DEFAULT_PARK_TIMEOUT,
                "a value above the cache's capacity must not be published"
            );
        }
    }

    #[test]
    fn a_value_that_does_not_parse_falls_back_to_the_default() {
        // A typo in a CI env must not silently change what the suite proves.
        for bad in ["", "abc", "-5", "10.5", "1_000"] {
            assert_eq!(
                Duration::from_millis(resolve_park_timeout(Ok(bad.into()))),
                DEFAULT_PARK_TIMEOUT,
                "{bad:?} is not a number of milliseconds"
            );
        }
        assert_eq!(
            Duration::from_millis(resolve_park_timeout(Err(std::env::VarError::NotUnicode(
                "not utf8".into()
            )))),
            DEFAULT_PARK_TIMEOUT
        );
    }
}
