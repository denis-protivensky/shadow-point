# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[Unreleased]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/denis-protivensky/shadow-point/releases/tag/v0.1.0

## [Unreleased]

### Added
- The generated public items carry doc comments, so a consumer that keeps
  `#![deny(missing_docs)]` builds its **test** target with hooks installed.
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
  `(while unwinding)`; the raised path's message text is byte-for-byte what it
  always was (verified against `v0.3.1` output), so a test matching the panic
  text keeps matching it.
- `docs/guide.md`: "The leak and what it owns" — the four measured leak cases
  (`Arc` in `T` never drops, `Weak` in `T` drops when the test decides, a
  consumed fire-once closure drops, an `every` closure never does) and the
  three test shapes that work with them; the guard API list now points there
  and its drop-assertion bullet matches the new unwinding rule.
- `docs/guide.md`: "Keep a failure one test" — the three test-writing habits
  that make one bad scenario cost one test (guard in a narrow scope with
  assertions after it, `expect(n)` when the claim is only a count, collect
  through the closure instead of asserting inside it); the panic-behavior
  paragraph and the debugging reference updated for the new shape.

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
