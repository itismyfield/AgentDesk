use super::*;

#[test]
fn continuation_adoption_requires_every_safety_fact() {
    let all = ClaudeContinuationAdoptionFacts {
        bound_stale_for_threshold: true,
        pane_alive: true,
        candidate_newer_than_bound: true,
        candidate_grew_across_samples: true,
        candidate_starts_after_bound_activity: true,
    };
    assert!(should_adopt_newer_claude_transcript(all));

    for mutate in [
        |facts: &mut ClaudeContinuationAdoptionFacts| facts.bound_stale_for_threshold = false,
        |facts: &mut ClaudeContinuationAdoptionFacts| facts.pane_alive = false,
        |facts: &mut ClaudeContinuationAdoptionFacts| facts.candidate_newer_than_bound = false,
        |facts: &mut ClaudeContinuationAdoptionFacts| facts.candidate_grew_across_samples = false,
        |facts: &mut ClaudeContinuationAdoptionFacts| {
            facts.candidate_starts_after_bound_activity = false
        },
    ] {
        let mut facts = all;
        mutate(&mut facts);
        assert!(!should_adopt_newer_claude_transcript(facts));
    }
}

#[test]
fn continuation_candidate_requires_growth_across_two_samples() {
    let tmp = tempfile::tempdir().unwrap();
    let candidate = tmp.path().join("candidate.jsonl");
    std::fs::write(&candidate, b"first\n").unwrap();
    let tmux = format!("AgentDesk-claude-4423-growth-{}", std::process::id());

    assert!(!observe_claude_continuation_candidate_growth(
        &tmux, &candidate
    ));
    assert!(!observe_claude_continuation_candidate_growth(
        &tmux, &candidate
    ));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&candidate)
        .unwrap()
        .write_all(b"second\n")
        .unwrap();
    assert!(observe_claude_continuation_candidate_growth(
        &tmux, &candidate
    ));
}

#[test]
fn timestamp_continuation_binding_supersedes_stale_launch_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let launch_path = tmp.path().join("old.jsonl");
    let continuation_path = tmp.path().join("new.jsonl");
    std::fs::write(
        &launch_path,
        concat!(
            "{\"timestamp\":\"2026-07-10T00:00:01Z\"}\n",
            "{\"timestamp\":\"2026-07-10T00:00:09Z\"}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        &continuation_path,
        concat!(
            "{\"timestamp\":\"2026-07-10T00:00:10Z\"}\n",
            "{\"timestamp\":\"2026-07-10T00:00:11Z\"}\n"
        ),
    )
    .unwrap();
    filetime::set_file_mtime(
        &launch_path,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        &continuation_path,
        filetime::FileTime::from_unix_time(1_700_000_001, 0),
    )
    .unwrap();
    let binding =
        |path: &Path, session: &str| crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session.to_string()),
            last_offset: 0,
            relay_last_offset: None,
        };
    let existing = binding(&continuation_path, "new");
    let launch = binding(&launch_path, "old");
    assert!(claude_continuation_binding_supersedes_launch(
        &existing, &launch
    ));

    std::fs::write(
        &continuation_path,
        "{\"timestamp\":\"2026-07-10T00:00:08Z\"}\n",
    )
    .unwrap();
    assert!(!claude_continuation_binding_supersedes_launch(
        &existing, &launch
    ));
}

#[test]
fn transcript_binding_refresh_requires_recent_activity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let transcript_path = tmp.path().join("claude.jsonl");
    std::fs::write(&transcript_path, b"old transcript\n").expect("write transcript");
    filetime::set_file_mtime(
        &transcript_path,
        filetime::FileTime::from_system_time(
            std::time::SystemTime::now()
                - std::time::Duration::from_secs(
                    crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS as u64 + 1,
                ),
        ),
    )
    .expect("set stale transcript mtime");

    assert!(
        !transcript_recent_enough_for_binding_refresh(&transcript_path),
        "a dead-but-existing Claude transcript must not refresh binding TTL"
    );

    filetime::set_file_mtime(
        &transcript_path,
        filetime::FileTime::from_system_time(std::time::SystemTime::now()),
    )
    .expect("set fresh transcript mtime");
    assert!(transcript_recent_enough_for_binding_refresh(
        &transcript_path
    ));
}
