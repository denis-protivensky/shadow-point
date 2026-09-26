# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[Unreleased]: https://github.com/denis-protivensky/shadow-point/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/denis-protivensky/shadow-point/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/denis-protivensky/shadow-point/releases/tag/v0.1.0

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
