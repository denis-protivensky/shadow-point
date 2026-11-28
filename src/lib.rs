#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
// The crate dogfoods the lint its consumers are subject to. `define_sp!`
// emits public items (guard, hook enum/constants, hook methods, builders), and
// a consumer that keeps `#![deny(missing_docs)]` fails its
// *test* build on any of them that carries no doc comment — production
// builds are clean because the caller gates the macro with `#[cfg(test)]`,
// so only a test build sees the generated items. `missing_docs` is inert
// outside a library crate and for `#[doc(hidden)]` items, so the pin lives
// here, in the test build of this crate, on the `sp_docs` probe module.
#![deny(missing_docs)]

// --- invoke! macro ---

/// Invoke the currently installed sync-point hook.
///
/// Takes the sync-point module path and a plain `hook(args…)` call; dispatch
/// prepends the guarded `&T`, so it is not written at the call site. It
/// expands to a block expression (`{ #[cfg(test)] { … } }`): a real dispatch
/// under `#[cfg(test)]`, an empty block otherwise. It works as a statement
/// (`invoke!(…);`) and in expression position — a closure body, a block's
/// trailing value, `let () = invoke!(…);` — in both test and production
/// builds. The outer brace pair is what makes the expression form legal: a
/// bare `#[cfg(test)]`-attributed block is a statement, not an expression.
/// Production builds carry no dispatch code and never resolve the crate.
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
/// ```rust,no_run
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
/// `sp_invoke!(map_sp, before_insert(&key))`. In production the seam
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
/// ```rust,no_run
/// struct Full;
///
/// fn alloc(capacity: usize) -> Result<(), Full> {
///     macro_rules! sp_inject {
///         ($sp:path, $hook:ident($($arg:expr),* $(,)?), $body:block) => {
///             #[cfg(test)]
///             {
///                 let __sp_inject = ::core::cell::Cell::new(false);
///                 ::shadow_point::invoke!($sp, $hook($($arg,)* &__sp_inject));
///                 if __sp_inject.get() {
///                     let _: () = $body; // rejects a value block; write `return`
///                 }
///             }
///         };
///     }
///
///     sp_inject!(map_sp, before_alloc_node(capacity), { return Err(Full); });
///     Ok(())
/// }
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
/// the CI job timeout, unlike `Gate::must_wait`.
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

// --- trace_enabled ---
/// Check whether sync-point tracing is enabled via the `SP_TRACE` environment
/// variable. When set, every hook fire prints its name and zero-based call
/// index (the same value [`current_fire`] reports) to stderr — useful for
/// discovering expected sequences and call counts.
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
/// A private `sequence` orders when entries are *consumed*; the step closure
/// may still be running and may still hold production locks. A gate waits
/// for a step to actually finish: set it at the end of the step and wait on
/// it where the next action must not start. Waits belong in the worker body
/// after `invoke!`, or in a private dispatch closure with a budget; a blocked
/// dispatch blocks the worker inside the instrumented function, holding its
/// production locks.
///
/// Inside a private dispatch closure that waits, use
/// [`must_wait`](Self::must_wait), never [`wait`](Self::wait): a missed
/// [`set`](Self::set) must fail with a diagnostic rather than hang the
/// suite. [`wait`](Self::wait) belongs in the test body.
/// `wait_timeout`/`must_wait` accept any duration, zero included: zero
/// inspects the flag without waiting, and
/// `must_wait(Duration::ZERO)` fails the test at once when the gate is
/// unset. The budget is a hang detector, not a timing assumption: a correct
/// test waits on an event its scenario guarantees, so load delays the wait
/// but never fails it — a timeout means a missing release, a test bug. Pick
/// it per test (a local constant), generous enough to never fire on a
/// correct run.
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
    /// private sequence-step closures: a missed [`set`](Self::set) fails the
    /// test with a message instead of hanging it.
    ///
    /// # Panics
    ///
    /// With `shadow-point: gate wait timed out after {timeout:?}`.
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
#[derive(Clone, Copy, Debug)]
pub struct FireInfo {
    /// Name of the hook being dispatched.
    pub hook: &'static str,
    /// Zero-based index of this fire on this hook, counted per guard (and
    /// therefore per firing thread). The closure runs on the firing
    /// thread, so `std::thread::current()` is the thread info — no fields
    /// for it.
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

// See CURRENT_FIRE: inline `const {}` blocks are experimental until 1.79.
thread_local! {
    // Addresses of `State<T>` instances currently dispatching a hook on this
    // thread. Per-thread: a genuinely concurrent fire from another thread is
    // never suppressed, and `contains` still gives per-install (guard)
    // re-entry suppression that survives transitive nesting (A -> B -> A).
    #[allow(clippy::missing_const_for_thread_local)]
    static EXEC_STACK: std::cell::RefCell<Vec<*const ()>> =
        std::cell::RefCell::new(Vec::new());
}

/// RAII guard that marks a sync point as executing on this thread for the
/// duration of a hook dispatch (panic-safe). Hidden: used only from macro
/// expansions generated by [`define_sp!`].
///
/// Re-entry suppression is per-thread and per installed guard: a dispatch
/// of any hook of the currently dispatching guard from the same thread is
/// suppressed, while a new guard of the same module (its own state) and a
/// genuinely concurrent fire from another thread both proceed.
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
/// fixed names: one constructor (`guard`), a private `Guard`, a hook-identifier
/// constant per hook, and the hidden dispatch machinery. Several sync
/// points with distinct module names can share a file directly.
///
/// # Surface
///
/// | Item | Role |
/// |---|---|
/// | `guard(value)` | install on this thread, returns `Guard<T>` |
/// | `Guard<T>` | `Deref`, fire-once/`sequence`/`every`/`fires`/`verify`, `!Send` |
/// | `Hook` | hook-identifier type; one constant per hook (`<hook>: Hook`) |
///
/// # Example
///
/// `no_run`: the example type-checks but is not executed — `invoke!` is
/// `#[cfg(test)]`-only and expands to nothing in a doctest, so the fires
/// would not happen and `verify()` would fail. See
/// `examples/register_private.rs` for a runnable version.
///
/// ```rust,no_run
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
///   count; both `fires` forms return the registration handle (`&Guard`).
/// - ordered: `g.sequence(|s| { s.a(f).b(g); })` —
///   entries consumed in order; a gated step is `s.when(pred).<hook>(f)`.
///   A second `sequence` call replaces the whole queue. Cross-thread order
///   comes from [`Gate`] waits in the worker bodies, after `invoke!`; a
///   worker's `sequence` orders that worker's own protocol.
/// - every fire: `g.every(|e| { e.a(f); })` — runs on every non-suppressed
///   fire. A second `every` call replaces the whole registration.
/// - count only: `g.fires(<hook>, n)` — no closure.
///
/// Closure bounds: a fire-once or sequence closure is
/// `FnOnce(&T, args…) + 'static`; an `every` closure is
/// `Fn(&T, args…) + 'static`; a `when` predicate is
/// `Fn(args…) -> bool + 'static`. The guard is single-threaded, so closures
/// may capture `Rc` and other `!Send` values; a captured borrow still fails
/// the `'static` bound.
///
/// A `when` predicate is evaluated on every matching fire, before the step; it
/// must be a pure check. Calling `sequence` or `verify` on the same guard from
/// it panics with a `shadow-point:` diagnostic naming the guard's install
/// site. A blocking predicate parks the firing thread inside the instrumented
/// function, like any dispatch closure, so a `Gate` wait there needs the same
/// budget reasoning. Nothing else is enforced: `fires`, `every`, and
/// registration from a predicate are allowed.
///
/// Every closure that dispatch calls runs on the caller's stack inside the
/// instrumented function: `every`, fire-once, and sequence steps. A blocking
/// wait inside a dispatch closure is legal choreography: the dispatcher never
/// waits on another thread (out-of-order fires panic), so a gated freeze is
/// deliberate. The wait is safe only when the thread that releases it needs
/// nothing the blocked frame holds. The frame still owns the instrumented
/// function's locks; [`Gate::must_wait`] with a budget bounds a mistake to a
/// failing test, it does not remove the class. Record traces and set gates in
/// dispatch closures; wait in the worker body after `invoke!` or in a private
/// step with a local budget.
///
/// # Reserved hook names
///
/// `sequence`, `every`, `fires`, `verify`, `guard`, `with_dyn`, and `when`
/// collide with the fixed public surface; `define_sp!` rejects them with a
/// `compile_error!` naming the hook; the raw forms (`r#sequence`, …) get
/// the same diagnostic. Any other identifier — including `value`, `calls`,
/// `expected`, `state`, `new`, `hook`, `f`, `NAMES`, `__SP_NAMES`,
/// `__sp_state`, `__sp_value`, `__sp_fixed` — is legal:
/// fixed fields and functions live apart from the generated ones in a
/// private submodule, and the fixed and per-hook fields sit in separate
/// structs, so none of them can collide with a hook name. (Keywords such as
/// `loop` are not identifiers; a raw identifier like `r#type` works.)
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
/// Dispatch is per-thread: every worker that fires a hook must install the
/// sync point on its own thread first — `guard(value)` per worker. `Guard`
/// is `!Send` — drop it on the thread that created it. The state and the
/// dispatch closures never leave that thread, so `T` need not be
/// `Send`/`Sync` and the closures need not be `Send`. See the README for the
/// full ownership and panic/drop contract.
///
/// A multithreaded test gives each worker its own private guard and orders
/// that worker's protocol with `sequence`, while [`Gate`] choreographs the
/// workers against each other. Fires from threads without an install are
/// no-ops, so one worker's sequence never waits on another thread.
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
             Constructor `guard`, a private `Guard`, a hook constant \
             per declared hook. See the crate README for the full contract."
        )]
        #[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
        $vis mod $name {
            // ── Reserved hook names (the fixed public surface) ───────
            //
            // A local macro (not exported): reject hook names that collide with
            // the fixed public surface — the registration methods
            // (sequence/every/fires/verify), the module constructor (guard),
            // the dispatch fn (with_dyn), and the gated-step constructor
            // (when). All fixed machinery lives in a private submodule, with
            // the generated per-hook fields and functions kept apart from its
            // own fields and functions, so no other identifier can collide
            // (D26; 7 arms).
            const _: () = {
                macro_rules! __sp_reject {
                    (@reject $n:literal, $w:literal) => {
                        ::core::compile_error!(::core::concat!(
                            "`", $n, "` is a reserved hook name: ", $w
                        ));
                    };
                    (sequence) => { __sp_reject!(@reject "sequence",
                        "it collides with the generated registration method `sequence`"); };
                    (every) => { __sp_reject!(@reject "every",
                        "it collides with the generated registration method `every`"); };
                    (fires) => { __sp_reject!(@reject "fires",
                        "it collides with the generated `fires` method and hook constant"); };
                    (verify) => { __sp_reject!(@reject "verify",
                        "it collides with the generated `verify` method"); };
                    (guard) => { __sp_reject!(@reject "guard",
                        "it collides with the module constructor `guard` in the value namespace"); };
                    (with_dyn) => { __sp_reject!(@reject "with_dyn",
                        "it collides with the module dispatch fn `with_dyn` in the value namespace"); };
                    (when) => { __sp_reject!(@reject "when",
                        "it collides with the gated-step constructor `Seq::when`"); };
                    // A raw identifier names the same item to rustc; route it
                    // to the plain arm so it gets the same diagnostic.
                    (r#sequence) => { __sp_reject!(sequence); };
                    (r#every) => { __sp_reject!(every); };
                    (r#fires) => { __sp_reject!(fires); };
                    (r#verify) => { __sp_reject!(verify); };
                    (r#guard) => { __sp_reject!(guard); };
                    (r#with_dyn) => { __sp_reject!(with_dyn); };
                    (r#when) => { __sp_reject!(when); };
                    ($other:ident) => {};
                }
                $( __sp_reject!($method); )+
            };

            // ── Hook identifiers ─────────────────────────────────────

            /// Identifiers for this sync point's hooks, one variant per
            /// declared hook. The type of the module's hook constants and
            /// the argument of `fires`.
            #[repr(usize)]
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            #[allow(non_camel_case_types)]
            pub enum Hook {
                $(
                    #[doc = concat!("The `", stringify!($method), "` hook.")]
                    $method,
                )+
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

                /// Hook names indexed by discriminant, for diagnostics. A
                /// free item in the private submodule, so a hook named
                /// `__SP_NAMES` cannot shadow it (and the name is not reserved).
                const __SP_NAMES: &[&str] = &[$(stringify!($method)),+];

                /// The hook's own name, for diagnostics.
                fn __sp_name(h: Hook) -> &'static str {
                    __SP_NAMES[h as usize]
                }

                // Dispatch trait: erases the guarded value type so one
                // thread-local slot serves any `T`.
                pub trait Hooks {
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
                        Option<::std::rc::Rc<dyn Hooks>>
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

                pub enum SeqAction<T: 'static> {
                    $( $method(
                        Option<::std::rc::Rc<dyn Fn($($arg_ty),*) -> bool>>,
                        Box<dyn FnOnce(&T $( , $arg_ty)*)>,
                    ), )+
                }

                impl<T: 'static> SeqAction<T> {
                    fn hook(&self) -> Hook {
                        match self {
                            $( Self::$method(..) => Hook::$method, )+
                        }
                    }
                    fn matches(&self, h: Hook) -> bool {
                        self.hook() == h
                    }
                    fn name(&self) -> &'static str {
                        __sp_name(self.hook())
                    }
                }

                // ── Every-fire closures ──────────────────────────────

                /// The `every` registration builder.
                pub struct Every<T: 'static> {
                    $( $method: Option<
                        ::std::rc::Rc<dyn Fn(&T $( , $arg_ty)*)>
                    >, )+
                }

                impl<T: 'static> Every<T> {
                    $(
                        /// Register a closure that runs on every fire of this hook.
                        #[allow(clippy::type_complexity)]
                        pub fn $method(
                            &mut self,
                            f: impl Fn(&T $( , $arg_ty)*) + 'static,
                        ) -> &mut Self {
                            self.$method = Some(::std::rc::Rc::new(f));
                            self
                        }
                    )+
                }

                // ── Sequence builder ─────────────────────────────────

                /// The `sequence` registration builder.
                pub struct Seq<T: 'static> {
                    seq: ::std::collections::VecDeque<SeqAction<T>>,
                }

                impl<T: 'static> Seq<T> {
                    $(
                        /// Push a sequence entry for this hook.
                        pub fn $method(
                            &mut self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + 'static,
                        ) -> &mut Self {
                            self.seq.push_back(SeqAction::$method(None, Box::new(f)));
                            self
                        }
                    )+

                    /// Start a predicate-gated step: `s.when(pred).<hook>(f)`.
                    /// The predicate is stored untyped here; each `.<hook>`
                    /// on the returned handle adds the bound for that hook's
                    /// arguments, so a single-hook predicate need only
                    /// satisfy its own signature.
                    ///
                    /// The predicate runs on every matching fire, before the
                    /// step; it must be a pure check. Calling `sequence` or
                    /// `verify` on the same guard from it panics with a
                    /// `shadow-point:` diagnostic. A blocking predicate parks
                    /// the firing thread inside the instrumented function, like
                    /// any dispatch closure. Only those two calls are
                    /// diagnosed; `fires`, `every`, and registration are not.
                    /// The step closure runs after the predicate returns
                    /// `true`.
                    pub fn when<P>(&mut self, pred: P) -> When<'_, T, P> {
                        When { seq: self, pred }
                    }
                }

                /// The gated-step sub-builder returned by `Seq::when`.
                pub struct When<'a, T: 'static, P> {
                    seq: &'a mut Seq<T>,
                    pred: P,
                }

                impl<'a, T: 'static, P> When<'a, T, P> {
                    $(
                        /// Push a predicate-gated sequence entry for this hook.
                        /// The bound on `P` is applied here, at hook
                        /// selection, so it covers only this hook's argument
                        /// signature.
                        pub fn $method(
                            self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + 'static,
                        ) -> &'a mut Seq<T>
                        where
                            P: Fn($($arg_ty),*) -> bool + 'static,
                        {
                            self.seq.seq.push_back(
                                SeqAction::$method(
                                    Some(::std::rc::Rc::new(self.pred)),
                                    Box::new(f),
                                ),
                            );
                            self.seq
                        }
                    )+
                }

                // ── State ────────────────────────────────────────────

                // Fixed fields and generated per-hook fields live in two
                // structs wrapped by a tuple: no named field is shared
                // between the fixed machinery and a hook's own field name,
                // so every identifier is legal as a hook name (D26).
                struct StateFixed<T: 'static> {
                    __sp_value: T,
                    __sp_every: ::std::cell::RefCell<Every<T>>,
                    __sp_sequence: ::std::cell::RefCell<
                        ::std::collections::VecDeque<SeqAction<T>>
                    >,
                    __sp_installed_at: &'static std::panic::Location<'static>,
                    __sp_calls: ::std::vec::Vec<::std::cell::Cell<usize>>,
                    __sp_expected: ::std::vec::Vec<::std::cell::Cell<Option<usize>>>,
                    __sp_reported: ::std::cell::Cell<bool>,
                    __sp_predicate_busy: ::std::cell::Cell<bool>,
                }

                struct StateRegs<T: 'static> {
                    $(
                        $method: ::std::cell::Cell<
                            Option<Box<dyn FnOnce(&T $( , $arg_ty)*)>>
                        >,
                    )+
                }

                struct State<T: 'static>(StateFixed<T>, StateRegs<T>);

                fn new_state<T: 'static>(
                    value: T,
                    installed_at: &'static std::panic::Location<'static>,
                ) -> State<T> {
                    let n = [$( stringify!($method) ),+].len();
                    State(
                        StateFixed {
                            __sp_value: value,
                            __sp_every: ::std::cell::RefCell::new(Every {
                                $( $method: None, )+
                            }),
                            __sp_sequence: ::std::cell::RefCell::new(
                                ::std::collections::VecDeque::new()
                            ),
                            __sp_installed_at: installed_at,
                            __sp_calls: (0..n)
                                .map(|_| ::std::cell::Cell::new(0))
                                .collect(),
                            __sp_expected: (0..n)
                                .map(|_| ::std::cell::Cell::new(None))
                                .collect(),
                            __sp_reported: ::std::cell::Cell::new(false),
                            __sp_predicate_busy: ::std::cell::Cell::new(false),
                        },
                        StateRegs { $( $method: ::std::cell::Cell::new(None), )+ },
                    )
                }

                // Generated per-hook functions live in their own module: a
                // hook name can then shadow any fixed item imported from the
                // parent, but never collide with one.
                mod __sp_hooks {
                    use super::*;

                    $(
                        pub(super) fn $method<T: 'static>(
                            st: &State<T>,
                            f: impl FnOnce(&T $( , $arg_ty)*) + 'static,
                        ) {
                            st.1.$method.set(Some(Box::new(f)));
                            super::set_expected_if_unset(st, Hook::$method, 1);
                        }
                    )+
                }

                // ── Per-hook operations (free functions) ─────────────

                fn bump_call<T: 'static>(
                    st: &State<T>, h: Hook,
                ) -> usize {
                    let cell = &st.0.__sp_calls[h as usize];
                    let previous = cell.get();
                    cell.set(previous.wrapping_add(1));
                    previous
                }

                fn set_expected<T: 'static>(
                    st: &State<T>, h: Hook, n: usize,
                ) {
                    st.0.__sp_expected[h as usize].set(Some(n));
                }

                fn set_expected_if_unset<T: 'static>(
                    st: &State<T>, h: Hook, n: usize,
                ) {
                    let slot = &st.0.__sp_expected[h as usize];
                    if slot.get().is_none() {
                        slot.set(Some(n));
                    }
                }

                /// The single access point to the sequence queue.
                ///
                /// Every read and write goes through here. A predicate runs
                /// outside the borrow; `__sp_predicate_busy` is the tripwire
                /// for a reentrant `sequence`/`verify` from it, so the panic
                /// names the install site and the rule instead of surfacing a
                /// bare `BorrowMutError`. The plain `borrow_mut` is an
                /// internal invariant: no user callback or destructor runs
                /// while the queue is borrowed.
                fn borrow_sequence<T: 'static>(
                    st: &State<T>,
                ) -> ::std::cell::RefMut<
                    '_,
                    ::std::collections::VecDeque<SeqAction<T>>,
                > {
                    if st.0.__sp_predicate_busy.get() {
                        predicate_reentry_panic(st);
                    }
                    st.0.__sp_sequence.borrow_mut()
                }

                #[cold]
                fn predicate_reentry_panic<T: 'static>(st: &State<T>) -> ! {
                    panic!(
                        "shadow-point: the guard installed at {} is inside a \
                         `when` predicate: calling `sequence`/`verify` from it \
                         is not allowed; predicates must be pure checks",
                        st.0.__sp_installed_at,
                    )
                }

                /// Sets the predicate-busy flag for the duration of the call
                /// and clears it on normal return and on unwind.
                struct PredicateBusy<'a>(&'a ::std::cell::Cell<bool>);

                impl<'a> PredicateBusy<'a> {
                    fn enter(flag: &'a ::std::cell::Cell<bool>) -> Self {
                        flag.set(true);
                        Self(flag)
                    }
                }

                impl Drop for PredicateBusy<'_> {
                    fn drop(&mut self) {
                        self.0.set(false);
                    }
                }

                /// The phase-1 result of inspecting the sequence head.
                enum Head<P> {
                    /// No head match: the fire falls through to fire-once.
                    Fallthrough,
                    /// A plain head entry: pop it.
                    Consume,
                    /// A predicate-gated head entry: evaluate the predicate
                    /// outside the borrow, then pop it on `true`.
                    Gated(P),
                }

                fn build_sequence<T: 'static>(
                    st: &State<T>, f: impl FnOnce(&mut Seq<T>),
                ) {
                    let mut builder = Seq {
                        seq: ::std::collections::VecDeque::new(),
                    };
                    f(&mut builder);
                    // Replace under the borrow; destroy the old queue only
                    // after it is released — its closures may capture values
                    // whose `Drop` re-enters `sequence`.
                    let replaced = {
                        let mut seq = borrow_sequence(st);
                        ::std::mem::replace(&mut *seq, builder.seq)
                    };
                    drop(replaced);
                }

                fn set_every<T: 'static>(
                    st: &State<T>, f: impl FnOnce(&mut Every<T>),
                ) {
                    let mut every = Every { $( $method: None, )+ };
                    f(&mut every);
                    // Same rule as `build_sequence`: `replace` hands the old
                    // registration back for destruction after the borrow.
                    drop(st.0.__sp_every.replace(every));
                }

                fn report_while_unwinding(message: &str) {
                    use ::std::io::Write;
                    let _ = writeln!(
                        std::io::stderr(),
                        "shadow-point: {message} (while unwinding)"
                    );
                }

                /// Mark the state reported, then raise `panic` (or print it
                /// while unwinding). Shared by both `assert_fires` paths; the
                /// caller supplies the panic form so the count path keeps its
                /// `assert_eq!` payload shape.
                fn report_mismatch<T: 'static>(
                    st: &State<T>,
                    unwinding: bool,
                    message: &str,
                    raise: impl FnOnce(),
                ) {
                    st.0.__sp_reported.set(true);
                    if unwinding {
                        report_while_unwinding(message);
                    } else {
                        raise();
                    }
                }

                #[track_caller]
                fn assert_fires<T: 'static>(
                    st: &State<T>, from_drop: bool,
                ) {
                    if from_drop && st.0.__sp_reported.get() {
                        return;
                    }
                    let unwinding = std::thread::panicking();
                    let seq = borrow_sequence(st);
                    if !seq.is_empty() {
                        let message = format!(
                            "sync point sequence not fully consumed: \
                             {} entr{} remaining [{}] (installed at {})",
                            seq.len(),
                            if seq.len() == 1 { "y" } else { "ies" },
                            seq.iter()
                                .map(|e| e.name())
                                .collect::<Vec<_>>()
                                .join(", "),
                            st.0.__sp_installed_at,
                        );
                        drop(seq);
                        report_mismatch(st, unwinding, &message, || panic!("{message}"));
                    } else {
                        drop(seq);
                    }
                    for (i, expected) in st.0.__sp_expected.iter().enumerate() {
                        let Some(expected) = expected.get() else { continue };
                        let actual = st.0.__sp_calls[i].get();
                        if actual == expected {
                            continue;
                        }
                        let message = format!(
                            "sync point hook `{}` fired {actual} \
                             time(s), expected {expected} (installed at {})",
                            __SP_NAMES[i],
                            st.0.__sp_installed_at,
                        );
                        report_mismatch(st, unwinding, &message, || {
                            assert_eq!(actual, expected, "{message}");
                        });
                    }
                }

                // ── Owner: the single assertion/reclamation point ────

                struct Owner<T: 'static> {
                    state: Option<State<T>>,
                }

                fn new_owner<T: 'static>(state: State<T>) -> Owner<T> {
                    Owner { state: Some(state) }
                }

                fn owner_state<T: 'static>(owner: &Owner<T>) -> &State<T> {
                    owner.state
                        .as_ref()
                        .expect("shadow-point: owner used after drop")
                }

                impl<T: 'static> Hooks for Owner<T> {
                    $(
                        fn $method(&self $( , $arg: $arg_ty)*) {
                            let st = owner_state(self);
                            let addr = st as *const State<T> as *const ();
                            let Some(_e) = $crate::ExecSink::try_enter(addr) else {
                                return;
                            };
                            let h = Hook::$method;
                            let idx = bump_call(st, h);
                            if $crate::trace_enabled() {
                                eprintln!(
                                    "[sp] {} call #{} thread={}",
                                    __sp_name(h),
                                    idx,
                                    std::thread::current()
                                        .name()
                                        .unwrap_or("<unnamed>"),
                                );
                            }
                            let _f = $crate::CurrentFire::install($crate::FireInfo {
                                hook: __sp_name(h),
                                index: idx,
                            });
                            let every = {
                                let guard = st.0.__sp_every.borrow();
                                guard.$method.clone()
                            };
                            if let Some(f) = every {
                                f(&st.0.__sp_value $( , $arg)*);
                            }
                            // Phase 1: inspect the head under a short borrow.
                            // A gated head only clones its predicate out
                            // here. An ordering violation formats its message
                            // under the borrow but panics after the release,
                            // so even a user panic hook never sees the queue
                            // borrowed.
                            let head: Result<Head<_>, String> = {
                                let seq = borrow_sequence(st);
                                match seq.front() {
                                    None => Ok(Head::Fallthrough),
                                    Some(front) if front.matches(h) => match front {
                                        SeqAction::$method(Some(pred), _) => {
                                            Ok(Head::Gated(::std::rc::Rc::clone(pred)))
                                        }
                                        SeqAction::$method(None, _) => Ok(Head::Consume),
                                        // front.matches(h) guarantees this variant.
                                        _ => unreachable!(
                                            "shadow-point: sequence head matched a \
                                             different hook variant"
                                        ),
                                    },
                                    Some(front) => {
                                        if seq.iter().any(|e| e.matches(h)) {
                                            // A guard owns its own sequence,
                                            // so an out-of-order fire is a
                                            // one-thread ordering violation —
                                            // panic, never park.
                                            Err(format!(
                                                "sync point ordering violation: \
                                                 expected `{}` next but `{}` fired",
                                                front.name(),
                                                __sp_name(h),
                                            ))
                                        } else {
                                            Ok(Head::Fallthrough)
                                        }
                                    }
                                }
                            };
                            let head = match head {
                                Ok(head) => head,
                                Err(message) => panic!("{message}"),
                            };
                            // Phase 2: evaluate a gated head outside the
                            // borrow. `PredicateBusy` makes a reentrant
                            // `sequence`/`verify` a diagnosed panic; the queue
                            // cannot change otherwise.
                            let (head_expects, consumed) = match head {
                                Head::Fallthrough => (false, None),
                                Head::Consume => (true, borrow_sequence(st).pop_front()),
                                Head::Gated(pred) => {
                                    let pass = {
                                        let _busy = PredicateBusy::enter(
                                            &st.0.__sp_predicate_busy,
                                        );
                                        pred($($arg),*)
                                    };
                                    if pass {
                                        let consumed = borrow_sequence(st).pop_front();
                                        debug_assert!(
                                            matches!(
                                                consumed,
                                                Some(SeqAction::$method(..))
                                            ),
                                            "shadow-point: the sequence head changed \
                                             under a `when` predicate"
                                        );
                                        (true, consumed)
                                    } else {
                                        (true, None)
                                    }
                                }
                            };
                            match consumed {
                                Some(SeqAction::$method(_, f)) => {
                                    f(&st.0.__sp_value $( , $arg)*);
                                }
                                Some(_) => unreachable!(
                                    "shadow-point: consumed a non-matching sequence entry"
                                ),
                                None => {
                                    if idx == 0 && !head_expects {
                                        let fire_once = st.1.$method.take();
                                        if let Some(f) = fire_once {
                                            f(&st.0.__sp_value $( , $arg)*);
                                        }
                                    }
                                }
                            }
                        }
                    )+
                }

                impl<T: 'static> Drop for Owner<T> {
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
                pub struct Expect<'a, T: 'static> {
                    reg: &'a Guard<T>,
                    hook: Hook,
                }

                impl<'a, T: 'static> Expect<'a, T> {
                    fn new(reg: &'a Guard<T>, hook: Hook) -> Self {
                        Self { reg, hook }
                    }

                    /// Assert the hook fires exactly `n` times; returns the
                    /// registration handle.
                    pub fn fires(self, n: usize) -> &'a Guard<T> {
                        set_expected(guard_state(self.reg), self.hook, n);
                        self.reg
                    }
                }

                // ── Guard ────────────────────────────────────────────

                /// The sync-point guard. Derefs to the guarded
                /// value; drop restores the TLS install and the last owner
                /// asserts counts and sequence. `!Send`: drop it on the
                /// thread that created it.
                /// Nested guards of the same sync point save and restore the
                /// TLS install, so drop them in reverse order. Dropping an
                /// outer guard first detaches the inner one: the inner guard's
                /// fires become no-ops, so any count it still expects fails
                /// at its drop. The inner guard's drop then restores the
                /// *outer* install, leaving the outer sync point attached
                /// with no guard alive — its verdict runs only when that
                /// install is replaced or the thread ends, beyond the
                /// test's control.
                #[clippy::has_significant_drop]
                #[must_use]
                pub struct Guard<T: 'static> {
                    owner: ::std::rc::Rc<Owner<T>>,
                    old: Option<::std::rc::Rc<dyn Hooks>>,
                }

                fn guard_state<T: 'static>(g: &Guard<T>) -> &State<T> {
                    owner_state(&g.owner)
                }

                impl<T: 'static> Guard<T> {
                    $(
                        /// Register a fire-once closure for this hook; chain
                        /// `.fires(n)` for a different count.
                        pub fn $method(
                            &self,
                            f: impl FnOnce(&T $( , $arg_ty)*) + 'static,
                        ) -> Expect<'_, T> {
                            __sp_hooks::$method(guard_state(self), f);
                            Expect::new(self, Hook::$method)
                        }
                    )+

                    /// Register an ordered sequence of hook fires.
                    pub fn sequence(&self, f: impl FnOnce(&mut Seq<T>)) {
                        build_sequence(guard_state(self), f);
                    }

                    /// Register closures that fire on every invocation.
                    pub fn every(&self, f: impl FnOnce(&mut Every<T>)) {
                        set_every(guard_state(self), f);
                    }

                    /// Assert that `hook` fires exactly `n` times before drop.
                    pub fn fires(&self, hook: Hook, n: usize) -> &Self {
                        set_expected(guard_state(self), hook, n);
                        self
                    }

                    /// Run the drop assertions now, at the caller's location.
                    ///
                    /// On failure the state is marked reported: drop then
                    /// skips the remaining assertions and, on the unwind path,
                    /// the guarded value's destructor, so this call owns the
                    /// diagnostic.
                    #[track_caller]
                    pub fn verify(&self) {
                        assert_fires(guard_state(self), false);
                    }
                }

                impl<T: 'static> std::ops::Deref for Guard<T> {
                    type Target = T;
                    fn deref(&self) -> &T {
                        &guard_state(self).0.__sp_value
                    }
                }

                impl<T: 'static> Drop for Guard<T> {
                    fn drop(&mut self) {
                        // Restore TLS first, with the borrow dropped before
                        // the Rc<Owner> field releases — the assertion (in
                        // Owner::drop) then runs after every TLS install is
                        // gone.
                        TL.with(|c| {
                            *(c.borrow_mut()) = self.old.take()
                        });
                    }
                }

                // ── TLS attach ───────────────────────────────────────

                // Constructors live here, not in the public module: the
                // parameter `value` would otherwise collide with a hook
                // constant named `value` in the module's value namespace.
                /// Install a private guard driving this sync point for `value`.
                #[track_caller]
                pub fn guard<T: 'static>(value: T) -> Guard<T> {
                    let owner = ::std::rc::Rc::new(new_owner(new_state(
                        value,
                        std::panic::Location::caller(),
                    )));
                    // Unsize at the binding, not on the slot write.
                    let handle: ::std::rc::Rc<dyn Hooks> = owner.clone();
                    let old = TL.with(|c| {
                        let mut slot = c.borrow_mut();
                        slot.replace(handle)
                    });
                    Guard { owner, old }
                }
            }

            // ── Public surface (re-exports + constructors) ───────────

            #[doc(inline)]
            pub use __sp_impl::{guard, Guard};
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
