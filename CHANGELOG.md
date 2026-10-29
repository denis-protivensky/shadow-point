# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[Unreleased]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/denis-protivensky/shadow-point/releases/tag/v0.1.0

## [Unreleased]

### Added
- `park_timeout()`, the value the shared-mode sequence actually parks for:
  `PARK_TIMEOUT` unless `SP_PARK_TIMEOUT_MS` names milliseconds. The public
  constant keeps its meaning and stays the default, so a consumer importing
  it is untouched; the env is read once per process and cached in one atomic
  (no `unsafe`, no `OnceLock`, so MSRV 1.65 holds). A value that does not
  parse falls back to the default **and says so on stderr** — a typo in a CI
  env should not silently change what a suite proves. `0` is legal: a
  choreography that can never advance then reports at once instead of
  parking. Resolution is unit-tested, and the park itself end-to-end in
  `tests/park_timeout_env.rs` (child processes with their own env, since the
  cache is per-process).
  Set the env for the ignored `unattainable_head_reports_the_park_timeout`
  to make that pass cheap; do **not** set it for a whole CI leg — parking is
  a mutual-deadlock guard, and a 200 ms budget can mistake a loaded
  scheduler for a stuck head and fail a correct test.

### Changed
- **Breaking (semantics): the sync-point state is reclaimed, not leaked.**
  `install_guard`/`install_shared` held it through `Box::leak`, so the
  guarded value's `Drop` never ran and everything a registration captured
  lived until the process did. Both now own it through `Arc`, and the last
  handle releases it — the guarded value's destructor becomes observable at
  guard drop (shared mode: at the last `Arc`), and an `every` closure's or an
  unconsumed fire-once's captures go with the state. No public signature
  changed and no `unsafe` appeared (`forbid(unsafe_code)` holds): the
  reference count replaced the `'static` proof, dispatch cloning the handle
  before it calls out, which also keeps `EXEC_STACK`'s address identity from
  aliasing a freed state.
  On a thread that is already unwinding the state is **deliberately leaked**
  rather than freed: running a consumer's `Drop` there risks a double panic,
  the abort this crate's 0.3.2 work exists to prevent.
  Cost, measured (min of 11, two pinned cores, loaded machine): a fire
  through a live guard ~60 ns either way (≈+5 ns for the clone/drop),
  `install`+`drop` per cycle 290 ns → 95 ns, an uninstalled fire 1.7 →
  2.5 ns (test-only path).
  **Migration:** a test whose guarded value asserts inside its own `Drop`
  used to have that assertion silently skipped and now runs it, after the
  sync-point assertions. Verified against a real consumer's whole
  shadow-point suite: **0 tests changed verdict** (54 passed), so this is a
  risk to check, not a cost measured in the wild.
  The `Weak` and flag shapes are reframed as choices about the destructor's
  *moment* rather than workarounds for a leak; the API list's "Leaks
  per-install state" entry, the `'static` bound's rationale and the generated
  `SpGuard`/`SharedSp`/`install` doc comments (which stated the value is
  never dropped) are updated to match.
- `docs/guide.md`: "The leak and what it owns" → **"Reclamation and what it
  owns"**, because the leak it described is gone. The section now states who
  holds a strong reference (the guard, the TLS entry, one per worker, and the
  dispatch for the length of one fire), the release order (TLS restore →
  assertions → release), the new ownership table, the measured cost, and a
  migration note. `with_dyn` documents the clone-before-call rule and why a
  hook may install or drop a guard without breaking the TLS borrow.
- `docs/shared-mode.md`: the parking section names `park_timeout()` instead
  of asserting that `PARK_TIMEOUT` is "not per-install configurable", and
  adds how to lower the budget — with the warning that it is a deadlock
  guard, so a short budget on a busy machine can fail a correct test.

## [0.3.2] - 2026-10-30

### Added
- The generated public items carry doc comments, so a consumer that keeps
  `#![deny(missing_docs)]` builds the **test build of its library crate**
  with hooks installed (`missing_docs` is inert in integration-test crates,
  so `tests/` files never saw the lint).
  Before, every generated trait, method, struct, enum variant, builder and
  associated constant tripped the lint (production builds were clean because
  the caller gates `define_sp!` with `#[cfg(test)]`), which forced consumers
  into an `#[allow(missing_docs)]` wrapper module. Pinned by the crate's own
  `deny(missing_docs)` plus a `#[cfg(test)] pub mod sp_docs` probe: the lint
  is inert outside a library crate and for `#[doc(hidden)]` items, so only
  this shape catches a generated item losing its doc comment.
- `tests/cross_thread.rs`: `unattainable_head_reports_the_park_timeout`
  (`#[ignore]` — it parks for the full `PARK_TIMEOUT` by design) pins that
  the shared park arm fails with the timeout diagnostic instead of hanging.
- `tests/single_thread.rs`: `second_sequence_replaces_the_first` pins that a
  second `sequence` call swaps the whole deque rather than appending.

### Changed
- Drop-time assertions (`sequence` not fully consumed, `expect_calls`
  mismatch) are **printed instead of raised when the thread is already
  unwinding**. A panic from a `Drop` during unwinding is a double panic, and
  a double panic aborts the test binary: the harness stops reporting, tests
  that had not started are lost, and the test's own failure — the real one —
  is buried under the abort. An incomplete scenario is a symptom of that
  failure, not a second one. The assertion is raised exactly as before on a
  thread that is not unwinding. The printed line carries
  `(while unwinding)`, and it is written with `let _ = writeln!(stderr(), ..)`
  rather than `eprintln!`: a failed write makes `eprintln!` panic, and on this
  path a second panic is exactly the abort the change removes. Writing to
  `stderr()` directly also bypasses the harness's output capture, so the line
  reaches the terminal even on a passing `#[should_panic]` test, where
  `eprintln!` would be captured and discarded. The **raised** path keeps
  its payload byte-for-byte:
  the sequence assertion was `assert!(cond, msg)` (bare message, so `panic!`
  reproduces it) and the count assertion was `assert_eq!(a, b, msg)` and is
  raised that way again — the wrapper line and the left/right echo are part
  of what consumers match (verified against `v0.3.1` on stable and 1.65;
  `raised_assertions_keep_their_payload_shape` pins it). The generated
  `install_shared` doc, the `SpGuard` doc and the README mode table, which
  all promised an unconditional drop-time panic, now state the conditional
  form — in both table cells, not just the private one (`install_shared`'s
  Caveats and `SharedSp`'s drop doc among them); `docs/shared-mode.md` gains
  the same rule at the last-`Arc` drop step of the install checklist.
- The unwind trade now has a named cost: `thread::panicking()` is true in a
  `#[should_panic]` test as well, so such a test **passes** with an
  incomplete scenario where the old code aborted the binary. Documented in
  the guide's "Keep a failure one test" (four habits), pinned by
  `should_panic_test_passes_with_an_incomplete_scenario`. The two shapes
  that keep the teeth are named beside it: end the scenario and
  `drop(guard)` before the expected `panic!` (pinned by
  `guard_dropped_before_the_expected_panic_still_asserts`), or keep the
  guard outside a `catch_unwind`.
- `docs/guide.md`: "The leak and what it owns" — the four leak cases, each
  derived from who holds what (`Arc` in `T` never drops, `Weak` in `T` drops
  when the test decides, a consumed fire-once closure drops, an `every`
  closure never does) and the three test shapes that work with them. The
  shapes name the pitfalls they avoid: hook closures are `'static`, so the
  `Weak`-guarded shape carries its own `Weak` clone, never the `Arc`; the
  flag shape names which object implements `Drop` and who owns it; and the
  third tests the destructor outside the sync point entirely — ordinary
  `Drop`, no hook. The guard API list points at the section, and its
  drop-assertion bullet matches the new unwinding rule.
- Generated doc comments no longer link `[`Gate`](crate::Gate)` — v0.3.1's
  one generated `crate::` link: inside a consumer crate `crate::` resolves
  against *that* crate's root, so rustdoc renders a dead
  `href="crate::Gate"` (verified in a consumer's rendered HTML;
  `-D warnings` does not catch it).
- `docs/guide.md`: "Keep a failure one test" — the four test-writing habits
  that make one bad scenario cost one test (guard in a narrow scope with
  assertions after it, `expect(n)` when the claim is only a count, finish
  the scenario before a `#[should_panic]`, collect through the closure
  instead of asserting inside it); the panic-behavior paragraph and the
  debugging reference updated for the new shape, the double-panic rationale
  now in one place with a link from the other. The narrow-scope example
  counts with `expect_calls` and collects with `every` — a fire-once
  registration runs its closure on the FIRST fire only, so three fires
  collect one key while `.expect(3)` still passes (the counter counts
  fires, not closure runs) — and reads the collected Vec under the lock
  instead of `Arc::try_unwrap`, which the leaked registration's own `Arc`
  clone defeats with an opaque `Err(Arc)`.

### Internal
- The `# Example` block in the `define_sp!` docs was fenced
  ```` ```ignore ````, which still **collects** it as a doctest — and that
  test does not compile (`define_sp!` unqualified in a doctest scope). It was
  invisible because the default suite skips ignored items; it turned red the
  moment the suite gained a test that is ignored by design, since
  `cargo test -- --ignored` runs every ignored item in the crate. Now fenced
  ```` ```text ````, like the sibling `# Specification syntax` block in the
  same doc. This is what makes an `-- --ignored` pass possible at all:
  before it, such a run could not be green for any reason.

## [0.3.1] - 2026-09-26

### Changed
- README: crate badges (crates.io, docs.rs, MSRV, license, CI); Quick start
  now precedes the install-modes taxonomy; the three-patterns table merged
  Mode+Threads and dropped the Count checks column (covered by the modes
  table and the guide) — tables render poorly on crates.io.

### Internal
- Repository now publishes CI (GitHub Actions: test on stable + 1.65 MSRV,
  fmt, clippy, docs) and a committed `Cargo.lock` pinning tokio 1.21.2 for
  the MSRV leg.

## [0.3.0] - 2026-09-26

### Added
- `TokioAsyncGate` behind the `tokio-async` feature: non-parking async
  milestone counter for tests on a tokio `current_thread` runtime. `fire()`
  from hook closures, `wait_at_least(n)` from the async test body
  ([#12](https://github.com/denis-protivensky/shadow-point/pull/12)).
- `current_fire()` / `FireInfo`: metadata about the hook fire currently being
  dispatched on this thread (hook name, call index) ([#12](https://github.com/denis-protivensky/shadow-point/pull/12)).

### Changed
- Deduplicated `define_sp!` generated code (−76 net lines): sequence/every/
  fire-once registration bodies moved into `__Sp` helpers; guard and `SharedSp`
  delegate in one line each; single `assert_fires` used by both `Drop` impls
  ([#21](https://github.com/denis-protivensky/shadow-point/pull/21)).
- Per-hook call counters are fixed-size arrays indexed by hook discriminant
  instead of a shared map — lock-free on the fire path
  ([#23](https://github.com/denis-protivensky/shadow-point/pull/23)).
- `tokio` dependency trimmed to `sync` only (no `rt`/`macros`), keeping the
  proc-macro chain out of the build graph
  ([#23](https://github.com/denis-protivensky/shadow-point/pull/23)).
- Derived `Default` replaces hand-written constructors where possible
  ([#23](https://github.com/denis-protivensky/shadow-point/pull/23)).
- `Gate` panic-safety: poisoned locks recovered via `PoisonError::into_inner`;
  `Gate::wait_timeout_while` semantics documented
  ([#22](https://github.com/denis-protivensky/shadow-point/pull/22)).
- Docs split: README trimmed to a scannable core; full guides live in
  `docs/` (`guide.md`, `shared-mode.md`, `integration.md`, `alternatives.md`).
  docs.rs builds with all features so `TokioAsyncGate` renders ([#20](https://github.com/denis-protivensky/shadow-point/pull/20)).

### Fixed
- Lint hygiene: `clippy::ignored_unit_patterns` in tests (`|_, _|` → `|()|`),
  `clippy::doc_markdown` backticks in test doc comments, and `#[must_use]` on
  `ExecSink::try_enter` (an ignored `Option` silently disables re-entry
  suppression). `cargo clippy --all-features --all-targets -- -W
  clippy::pedantic` is now clean.

## [0.2.0] - 2026-09-06

### Added
- Cross-thread sync points: `{$prefix}Sp::install_shared(value)` returns an
  `Arc<{$prefix}SharedSp<T>>`; every worker installs it via
  `SharedSp::install()` inside its own closure (TLS is per-thread), `invoke!`
  then dispatches into the shared state on the firing thread
  ([#4](https://github.com/denis-protivensky/shadow-point/pull/4)).
- `Gate`: reusable boolean rendezvous for synchronous choreography —
  `set`/`clear`/`wait`/`wait_timeout`/`must_wait` ([#4](https://github.com/denis-protivensky/shadow-point/pull/4)).
- Shared-mode sequence parking with `PARK_TIMEOUT` (10 s): a thread whose
  sequence head is a different hook parks on a condvar instead of panicking,
  and a timeout fails the test with a diagnostic (never hangs)
  ([#4](https://github.com/denis-protivensky/shadow-point/pull/4)).

## [0.1.0] - 2026-08-31

### Added
- `define_sp!` macro generating per-value sync-point infrastructure:
  extension trait, hook enum, entry struct with associated hook constants,
  guard with `Deref` and drop-time assertions.
- Guard API: fire-once closures (`.expect(n)` for call-count assertions),
  ordered `sequence(...)` with `_when` predicates and `optional()` entries,
  `every` closures, `expect_calls`.
- `invoke!` macro: compiles away to a no-op in production builds
  (`cfg(test)` evaluated at the call-site crate).
- `SP_TRACE` environment variable: when set, every hook fire logs its name
  and call index to stderr — for discovering expected sequences and call
  counts.
- Zero production cost: the crate resolves no dependencies for default
  feature sets except `paste`.
