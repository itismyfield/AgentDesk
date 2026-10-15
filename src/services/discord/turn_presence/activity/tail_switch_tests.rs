use super::*;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;

#[test]
fn s1_tail_setting_is_exact_and_frozen_on_first_read() {
    const NAME: &str = "services::discord::turn_presence::activity::tail::switch_tests::s1_tail_setting_is_exact_and_frozen_on_first_read";
    if let Some(expected) = std::env::var_os("ADK_S1_SWITCH_CHILD") {
        let expected = expected == "1";
        super::super::tests::exercise_tail_switch_entry_order(
            expected,
            std::env::var_os("ADK_S1_SUPERVISOR_FIRST").is_some(),
        );
        assert_eq!(enabled(), expected);
        // SAFETY: this exact test runs alone in an isolated child process.
        unsafe {
            std::env::set_var("ADK_TURN_PRESENCE_TAIL", if expected { "0" } else { "1" });
        }
        assert_eq!(enabled(), expected, "the setting changed without a restart");
        return;
    }
    for (value, expected) in [
        (None, true),
        (Some("1".into()), true),
        (Some("0".into()), false),
        (Some("true".into()), false),
        (Some(" 1".into()), false),
        (Some(std::ffi::OsString::from_vec(vec![0xff])), false),
    ] {
        for supervisor_first in [false, true] {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", NAME, "--nocapture"])
                .env("ADK_S1_SWITCH_CHILD", if expected { "1" } else { "0" })
                .env_remove("TMUX")
                .env_remove("TMUX_PANE");
            if let Some(value) = &value {
                child.env("ADK_TURN_PRESENCE_TAIL", value);
            } else {
                child.env_remove("ADK_TURN_PRESENCE_TAIL");
            }
            if supervisor_first {
                child.env("ADK_S1_SUPERVISOR_FIRST", "1");
            } else {
                child.env_remove("ADK_S1_SUPERVISOR_FIRST");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        }
    }
}

#[test]
fn s1_tail_handoff_budget_and_hash_counters_cross_the_worker() {
    use crate::services::tui_o::shadow::{binding_reader::source_id_for, capture::io_trace};
    use serde_json::json;
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    let start = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}});
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item"}});
    let close = json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}});
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}})
    )
    .unwrap();
    for row in [&start, &blocked, &blocked, &close] {
        writeln!(file, "{row}").unwrap();
    }
    file.flush().unwrap();
    let source = source_id_for("parent", &path).unwrap();
    let binding = SourceBinding {
        channel_id: 69_181_699,
        provider: ShadowProvider::Codex,
        source,
    };
    let (observation_ranges, full_ranges) = std::thread::spawn(move || {
        io_trace::take();
        let budget = 8192;
        let mut reader = TailReader::open(
            binding.clone(),
            Limits {
                full: 1,
                tail: budget,
                poll: 4096,
                worker: 71,
            },
        )
        .unwrap();
        reader.bootstrap().unwrap();
        let (_, _, remaining, used) = reader.diagnostics();
        assert!(used > 0);
        assert_eq!(
            remaining,
            budget - used,
            "typed bootstrap resume refilled its budget"
        );
        let fact = reader.poll_installed().unwrap();
        assert!(reader.allows_idle(&fact, reader.facts.caught_up().unwrap()));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for row in [&blocked, &close] {
            writeln!(file, "{row}").unwrap();
        }
        file.flush().unwrap();
        assert!(reader.poll_installed().is_err());
        let before = reader.remaining;
        reader.resume().unwrap();
        assert_eq!(
            reader.remaining, before,
            "installed resume refilled bootstrap capacity"
        );
        let fact = reader.poll_installed().unwrap();
        assert!(reader.allows_idle(&fact, reader.facts.caught_up().unwrap()));
        let observation_ranges = io_trace::take();
        assert!(
            observation_ranges.iter().any(|(_, _, hash)| !hash),
            "worker reads were invisible"
        );
        assert!(
            observation_ranges.iter().all(|(_, _, hash)| !hash),
            "Tail initial/resume hashed a prefix"
        );
        let mut full = InputFacts::open(binding).unwrap();
        while !full.caught_up().unwrap() {
            if full.poll(4096).is_err() {
                let resume = full.resume_point().cloned().unwrap();
                full = InputFacts::resume(full.binding().clone(), &resume).unwrap();
            }
        }
        full.poll(4096).unwrap();
        let full_ranges = io_trace::take();
        assert!(
            full_ranges.iter().any(|(a, b, hash)| *hash && b > a),
            "same-worker Full positive control never hashed bytes"
        );
        (observation_ranges, full_ranges)
    })
    .join()
    .unwrap();
    eprintln!("S1_HELPER_IO observation={observation_ranges:?} full={full_ranges:?} skip=0");
}
#[test]
fn s1e_tail_io_ranges_are_bounded_at_each_handoff() {
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::shadow::capture::io_trace;
    const GUARD_BYTES: u64 = 4096;
    use serde_json::json;
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let metadata = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path(),"padding":"x".repeat(180)}});
    let start = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}});
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item"}});
    let close = json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}});
    let mut file = std::fs::File::create(&path).unwrap();
    for _ in 0..160 {
        writeln!(file, "{metadata}").unwrap();
    }
    for row in [&start, &blocked, &close] {
        writeln!(file, "{row}").unwrap();
    }
    file.flush().unwrap();
    let snapshot = file.metadata().unwrap().len();
    let tail = 4096;
    let expected_start = snapshot - tail;
    assert!(expected_start > GUARD_BYTES);
    let binding = SourceBinding {
        channel_id: 69_182_100,
        provider: ShadowProvider::Codex,
        source: source_id_for("parent", &path).unwrap(),
    };
    std::thread::spawn(move || {
        let ranges = |phase: &str, eof: u64| {
            let reads = io_trace::take();
            let actual = reads.iter().any(|(a, b, _)| b > a);
            if phase != "initial" {
                assert!(actual, "{phase}: no actual capture read");
            }
            assert!(
                reads.iter().all(|(_, _, hash)| !hash),
                "{phase}: prefix hashing"
            );
            assert!(
                reads
                    .iter()
                    .filter(|(a, b, _)| b > a)
                    .all(|(a, b, _)| *a >= expected_start - GUARD_BYTES && *b <= eof),
                "{phase}: read outside tail/guard/partial range: {reads:?}"
            );
            eprintln!("S1_RANGE phase={phase} window={expected_start} eof={eof} capture={reads:?}");
            actual
        };
        let limits = Limits {
            full: 1,
            tail,
            poll: 4096,
            worker: 177,
        };
        io_trace::take();
        let mut bootstrap = TailReader::open(binding.clone(), limits).unwrap();
        let initial_read = ranges("initial", snapshot);
        let bootstrapped = bootstrap.bootstrap();
        ranges("bootstrap", snapshot);
        bootstrapped.unwrap();
        assert!(initial_read, "initial guard positive control was invisible");
        let (_, _, remaining, used) = bootstrap.diagnostics();
        assert_eq!(remaining, tail.saturating_sub(used), "bootstrap refill");
        // A separate installed reader exposes the typed handoff as its own phase.
        let mut reader = TailReader::open(binding, limits).unwrap();
        io_trace::take();
        loop {
            if reader.poll_installed().is_err() {
                break;
            }
            assert!(
                !reader.facts.caught_up().unwrap(),
                "typed record was skipped"
            );
        }
        ranges("before-resume", snapshot);
        let before = reader.remaining;
        reader.resume().unwrap();
        ranges("typed-resume", snapshot);
        assert_eq!(reader.remaining, before, "typed resume refilled budget");
        let fact = reader.poll_installed().unwrap();
        assert!(reader.allows_idle(&fact, reader.facts.caught_up().unwrap()));
        ranges("installed", snapshot);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let bytes = close.to_string();
        file.write_all(bytes.as_bytes()).unwrap();
        file.flush().unwrap();
        let partial_eof = file.metadata().unwrap().len();
        reader.poll_installed().unwrap();
        assert!(!reader.facts.caught_up().unwrap());
        ranges("partial", partial_eof);
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let fact = reader.poll_installed().unwrap();
        assert!(reader.allows_idle(&fact, reader.facts.caught_up().unwrap()));
        ranges("partial-revalidation", partial_eof + 1);
    })
    .join()
    .unwrap();
}

#[test]
fn s1_tail_off_preserves_real_unbound_absence_and_live_holds_pg() {
    for test in [
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::s1_real_missing_namespace_reaches_selected_tmux_start_pg",
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::s1_fallback_only_and_restored_owner_only_live_hold_real_effects_pg",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("ADK_TURN_PRESENCE_TAIL", "0")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Tail off changed Unbound contract {test}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "zero test selection: {test}"
        );
    }
}
