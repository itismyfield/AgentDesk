use super::*;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;

#[test]
fn s1_tail_setting_is_exact_and_frozen_on_first_read() {
    const NAME: &str = "services::discord::turn_presence::activity::tail::switch_tests::s1_tail_setting_is_exact_and_frozen_on_first_read";
    if let Some(expected) = std::env::var_os("ADK_S1_SWITCH_CHILD") {
        let expected = expected == "1";
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
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", NAME, "--nocapture"])
            .env("ADK_S1_SWITCH_CHILD", if expected { "1" } else { "0" })
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        if let Some(value) = value {
            child.env("ADK_TURN_PRESENCE_TAIL", value);
        } else {
            child.env_remove("ADK_TURN_PRESENCE_TAIL");
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
