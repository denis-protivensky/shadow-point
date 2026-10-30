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

/// Park with an unattainable head and report how long the wait was.
fn park_and_report() -> Duration {
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
        .expect("a formatted panic message");
    assert!(
        msg.contains("sequence park timeout"),
        "the parked fire reports the park, not a hang: {msg}"
    );
    // Replace the deque so the Arc's drop assertions pass on the way out.
    shared.sequence(|_s| {});
    waited
}

/// Re-execute this binary as a child running exactly `test`, with `envs`
/// added, and return its stdout, stderr and measured duration.
///
/// A child that fails is reported with both streams: its marker line lives
/// on stdout, but its panic message lives on stderr, and "the marker is
/// missing" alone would hide the reason.
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
    let output = cmd
        .spawn()
        .and_then(std::process::Child::wait_with_output)
        .expect("the child ran");
    assert!(
        output.status.success(),
        "the child running `{test}` failed ({:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        started.elapsed(),
    )
}

fn marker(text: &str, key: &str) -> u128 {
    let raw = text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("{key} never printed by the child: {text}"));
    raw.trim().parse().expect("the child printed a number")
}

#[test]
fn the_env_value_is_what_the_park_uses() {
    if std::env::var("SP_PARK_CHILD").is_ok() {
        println!("SP_CHILD_WAITED={}", park_and_report().as_millis());
        return;
    }
    let (out, _, _) = child(
        "the_env_value_is_what_the_park_uses",
        &[("SP_PARK_CHILD", "1"), ("SP_PARK_TIMEOUT_MS", "200")],
    );
    let waited = marker(&out, "SP_CHILD_WAITED=");
    // The park is the env value and not the 10s default: this is the whole
    // claim, and it fails loudly if dispatch still waits on the constant.
    // The upper bound is loose on purpose — a scheduler under load can hold
    // a thread past its deadline; it cannot hand back 200 ms from a 10 s
    // wait.
    assert!(
        (150..3_000).contains(&waited),
        "SP_PARK_TIMEOUT_MS=200 should park near 200 ms, measured {waited} ms"
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
    let (out, err, elapsed) = child(
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
    assert!(
        elapsed < Duration::from_secs(5),
        "checking the fallback must not itself park for the default"
    );
}

#[test]
fn zero_means_fail_fast() {
    // The documented escape hatch for a wrong choreography: the head is
    // re-checked and the diagnostic raised on the next wake, so a test
    // that can never advance reports instead of waiting.
    if std::env::var("SP_PARK_ZERO").is_ok() {
        println!("SP_ZERO_WAITED={}", park_and_report().as_millis());
        return;
    }
    let (out, _, _) = child(
        "zero_means_fail_fast",
        &[("SP_PARK_ZERO", "1"), ("SP_PARK_TIMEOUT_MS", "0")],
    );
    let waited = marker(&out, "SP_ZERO_WAITED=");
    assert!(
        waited < 1_000,
        "SP_PARK_TIMEOUT_MS=0 should report at once, measured {waited} ms"
    );
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
