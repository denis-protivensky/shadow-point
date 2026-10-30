//! The park timeout, resolved from the environment, end to end.
//!
//! `park_timeout` caches one value per process, so this cannot be pinned
//! inside a normal test: another test in the same binary may already have
//! resolved it. The file therefore re-executes the test binary as a child
//! with the env set, and the child reports back through stdout.
//!
//! Only the *real* park is measured with a shortened timeout (200 ms), so
//! the whole file costs a fifth of a second. The fallback case is checked
//! through `park_timeout()` directly: asserting it by parking would wait
//! the full default, which is exactly the cost this knob exists to remove.
//!
//! What proves what (the wall-clock discipline): a loaded machine can
//! freeze a healthy child for seconds — observed here: ~9.5 s mid-park —
//! and that inflates any elapsed time. So elapsed time is asked only what
//! a stall cannot fake ("the park really parked", a lower bound); WHICH
//! budget the park used is read from the diagnostic, which names it; and a
//! child that never returns is the watchdog's business (see `child`).

use std::time::{Duration, Instant};

mod ct {
    shadow_point::define_sp! {
        pub prefix Ct
        {
            a(value: u32),
            b(value: u32),
        }
    }
}

/// Park with an unattainable head and report how long the wait was and
/// what the diagnostic said.
fn park_and_report() -> (Duration, String) {
    let shared = ct::CtSp::install_shared(());
    shared.sequence(|s| {
        s.b(|(), _| ()); // head: nothing will ever fire `b`
        s.a(|(), _| ()); // this fire is the one that parks
    });
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = shared.install();
        shadow_point::invoke!(ct::CtSp, a(0));
    }));
    let waited = started.elapsed();
    let err = outcome.expect_err("the parked fire must panic, not return");
    let msg = err
        .downcast_ref::<String>()
        .expect("a formatted panic message")
        .clone();
    assert!(
        msg.contains("sequence park timeout"),
        "the parked fire reports the park, not a hang: {msg}"
    );
    // Replace the deque so the Arc's drop assertions pass on the way out.
    shared.sequence(|_s| {});
    (waited, msg)
}

/// Re-execute this binary as a child running exactly `test`, with `envs`
/// added, and return its stdout, stderr and measured duration.
///
/// A child that fails is reported with both streams: its marker line lives
/// on stdout, but its panic message lives on stderr, and "the marker is
/// missing" alone would hide the reason.
///
/// The wait is BOUNDED, and that is the point: the regression this file
/// pins manifests as a park that never returns, so a child waited on
/// forever would hang the suite instead of failing it — the exact failure
/// mode the park timeout itself exists to prevent. A child that outlives
/// the watchdog is killed and reported as a hang, with both streams.
fn child(test: &str, envs: &[(&str, &str)]) -> (String, String, std::time::Duration) {
    let exe = std::env::current_exe().expect("the test binary path");
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().expect("the child ran");
    // Poll instead of `wait_with_output`: the pipes only ever hold the
    // child's small marker output, so blocking on them is not what protects
    // the suite — the deadline is. `try_wait` caches the exit status, so the
    // `wait_with_output` below still collects the streams afterwards.
    // 30 s of headroom: a loaded machine can freeze a healthy child for
    // seconds at a time (observed: ~9.5 s wall-clock jumps mid-park), and a
    // watchdog that kills healthy children is worse than one that fires
    // late. The child under test parks for at most 200 ms, so 30 s still
    // means "never returned", not "slow".
    let deadline = Duration::from_secs(30);
    let hung = loop {
        match child.try_wait().expect("the child's status is readable") {
            Some(_) => break false,
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                break true;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let output = child.wait_with_output().expect("the child ran");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !hung,
        "the child running `{test}` was still running after {deadline:?} \
         and was killed — its park never returned?\n--- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    );
    assert!(
        output.status.success(),
        "the child running `{test}` failed ({:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        stdout,
        stderr,
    );
    (stdout, stderr, started.elapsed())
}

fn marker(text: &str, key: &str) -> u128 {
    let raw = text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("{key} never printed by the child: {text}"));
    raw.trim().parse().expect("the child printed a number")
}

/// The budget this process was configured with (`SP_PARK_TIMEOUT_MS`, set
/// by the parent), as the diagnostic spells it: `Duration`'s `{:?}` —
/// "200ms", "0ns".
fn configured_budget() -> Duration {
    let ms: u64 = std::env::var("SP_PARK_TIMEOUT_MS")
        .expect("the child runs with the env its parent set")
        .trim()
        .parse()
        .expect("the parent sets a parsable budget");
    Duration::from_millis(ms)
}

/// The one claim a scheduler stall cannot fake: the diagnostic names the
/// budget the park resolved to. Elapsed time cannot make this claim —
/// "200 ms of park plus a 9 s stall" is indistinguishable from "parked the
/// 10 s default" under any wall-clock bound — so the budget travels in the
/// message instead, and elapsed time is asked only for a lower bound.
fn assert_names_budget(msg: &str) {
    let budget = format!("({:?})", configured_budget());
    assert!(
        msg.contains(&budget),
        "the diagnostic must name the resolved budget {budget}: {msg}"
    );
}

#[test]
fn the_env_value_is_what_the_park_uses() {
    if std::env::var("SP_PARK_CHILD").is_ok() {
        let (waited, msg) = park_and_report();
        assert_names_budget(&msg);
        println!("SP_CHILD_WAITED={}", waited.as_millis());
        return;
    }
    let (out, _, _) = child(
        "the_env_value_is_what_the_park_uses",
        &[("SP_PARK_CHILD", "1"), ("SP_PARK_TIMEOUT_MS", "200")],
    );
    let waited = marker(&out, "SP_CHILD_WAITED=");
    // WHICH budget the park used is the child's own assertion (the
    // diagnostic names it — stall-proof). Wall time here proves only what
    // a stall cannot fake: the park really parked, it did not fail at
    // once. Stalls inflate `waited`, never shrink it.
    assert!(
        waited >= 150,
        "SP_PARK_TIMEOUT_MS=200 should really park, measured {waited} ms"
    );
}

#[test]
fn a_typo_in_the_env_falls_back_and_says_so() {
    if std::env::var("SP_PARK_BAD").is_ok() {
        println!(
            "SP_BAD_RESOLVED={}",
            shadow_point::park_timeout().as_millis()
        );
        return;
    }
    let (out, err, _) = child(
        "a_typo_in_the_env_falls_back_and_says_so",
        &[("SP_PARK_BAD", "1"), ("SP_PARK_TIMEOUT_MS", "not-a-number")],
    );
    assert_eq!(
        marker(&out, "SP_BAD_RESOLVED="),
        shadow_point::PARK_TIMEOUT.as_millis(),
        "an unparsable value must not silently shorten what the suite proves"
    );
    assert!(
        err.contains("is not a number"),
        "the fallback is announced, not silent: {err}"
    );
    // No wall-clock bound: the child never parks on this path, and a hang
    // is the watchdog's claim to make.
}

/// `0` is legal and must reach the park path, not fall back to the default:
/// the failing park's diagnostic names a zero budget (a fall-back would
/// name `(10s)`). The "reports at once" half of the promise is wall-clock,
/// and a stall inflates any bound, so it is not pinned here; the watchdog
/// covers "never returns".
#[test]
fn zero_budget_is_honored_not_ignored() {
    if std::env::var("SP_PARK_ZERO").is_ok() {
        let (waited, msg) = park_and_report();
        assert_names_budget(&msg);
        println!("SP_ZERO_WAITED={}", waited.as_millis());
        return;
    }
    let (out, _, _) = child(
        "zero_budget_is_honored_not_ignored",
        &[("SP_PARK_ZERO", "1"), ("SP_PARK_TIMEOUT_MS", "0")],
    );
    // The marker proves the child's branch ran; without it the child could
    // have taken the parent branch and quietly recursed one level.
    let _ = marker(&out, "SP_ZERO_WAITED=");
}

/// The diagnostic itself, in the same cheap child: the park must name the
/// timeout and the blocking head, which is the whole claim of the test this
/// file replaces (`unattainable_head_reports_the_park_timeout`, ignored
/// because parking at the 10s default is the behavior under test). The
/// replacement-of-deque half of that test is pinned independently by
/// `second_sequence_replaces_the_first`.
#[test]
fn the_park_reports_the_timeout_and_names_the_head() {
    if std::env::var("SP_PARK_MSG").is_ok() {
        let shared = ct::CtSp::install_shared(());
        shared.sequence(|s| {
            s.b(|(), _| ());
            s.a(|(), _| ());
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = shared.install();
            shadow_point::invoke!(ct::CtSp, a(0));
        }));
        let err = outcome.expect_err("the parked fire must panic, not return");
        let msg = err
            .downcast_ref::<String>()
            .expect("a formatted panic message");
        println!("SP_PARK_MSG_BEGIN{msg}SP_PARK_MSG_END");
        shared.sequence(|_s| {});
        return;
    }
    let (out, _, _) = child(
        "the_park_reports_the_timeout_and_names_the_head",
        &[("SP_PARK_MSG", "1"), ("SP_PARK_TIMEOUT_MS", "200")],
    );
    let msg = out
        .split("SP_PARK_MSG_BEGIN")
        .nth(1)
        .and_then(|t| t.split("SP_PARK_MSG_END").next())
        .unwrap_or_else(|| panic!("the child printed no diagnostic: {out}"));
    assert!(
        msg.contains("sequence park timeout"),
        "the diagnostic names the park: {msg}"
    );
    assert!(msg.contains("head is `b`"), "and the blocking head: {msg}");
}

/// What this process resolved, from the environment it actually started
/// with: the default when nothing was set, the env value when it was.
/// Together with `resolve_park_timeout`'s unit tests this pins the cache,
/// which is where the sentinel bug lived.
#[test]
fn the_cache_honors_what_the_process_started_with() {
    // `u64::MAX` is the cache's own unresolved marker; the resolver refuses
    // it, so it must be expected as the default here too.
    let expected = match std::env::var("SP_PARK_TIMEOUT_MS") {
        Ok(raw) if raw.trim().parse::<u64>().map_or(false, |ms| ms != u64::MAX) => {
            std::time::Duration::from_millis(raw.trim().parse().unwrap())
        }
        _ => shadow_point::PARK_TIMEOUT,
    };
    assert_eq!(shadow_point::park_timeout(), expected);
}
