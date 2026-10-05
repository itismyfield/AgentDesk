//! Captured Codex 0.157.1 hooks replayed against the rollout source verifier and the
//! hook capability decision.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{
    CodexHookSourceClaim, CodexHookSourceRejection, CodexHookSourceRoute, CodexRolloutSource,
    VerifyStep, at_verify_step, before_final_identity, verify_codex_hook_source,
};
use crate::services::claude_tui::hook_bundle::{
    CodexHookActivation, CodexTrustHashEvidence, codex_hook_capability,
};
use crate::services::codex_tui::rollout_index::{
    cached_meta_for_tests, fail_header_reads_for_tests, lock_cache_for_tests,
    reset_cache_for_tests, warm_cache_for_tests,
};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hook_payload/codex-0.157.1.json");
    let text = std::fs::read_to_string(&path).expect("read codex fixture");
    serde_json::from_str(&text).expect("parse codex fixture")
}

fn runs(fixture: &Value) -> &[Value] {
    fixture["runs"].as_array().expect("runs")
}

fn source_for(run: &Value) -> CodexRolloutSource {
    if run["mode"] == "exec" {
        CodexRolloutSource::Exec
    } else {
        CodexRolloutSource::Cli
    }
}

/// Maps a captured `~/.codex/sessions/...` path under a test sessions root.
fn local_path(root: &Path, captured: &str) -> PathBuf {
    let (_, relative) = captured
        .split_once("/.codex/sessions/")
        .expect("captured path under .codex/sessions");
    root.join(relative)
}

fn write_rollout(path: &Path, header: &Value) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, format!("{header}\n{{\"type\":\"event_msg\"}}\n")).expect("write rollout");
}

fn meta_for<'a>(run: &'a Value, session_id: &str) -> &'a Value {
    run["rollout_session_meta"]
        .as_array()
        .expect("meta")
        .iter()
        .find(|record| record["payload"]["id"] == session_id)
        .expect("captured session_meta for hook session")
}

/// Writes every captured rollout header where its hooks said it lives.
fn materialize(root: &Path, fixture: &Value) -> Vec<(String, PathBuf, CodexRolloutSource)> {
    let mut hooks = Vec::new();
    for run in runs(fixture) {
        for event in run["events"].as_array().expect("events") {
            let payload = &event["payload"];
            let id = payload["session_id"]
                .as_str()
                .expect("session_id")
                .to_string();
            let path = local_path(root, payload["transcript_path"].as_str().expect("path"));
            write_rollout(&path, meta_for(run, &id));
            hooks.push((id, path, source_for(run)));
        }
    }
    hooks
}

fn claim<'a>(
    id: &'a str,
    path: Option<&'a Path>,
    expected: CodexRolloutSource,
) -> CodexHookSourceClaim<'a> {
    CodexHookSourceClaim {
        session_id: id,
        transcript_path: path,
        expected_source: expected,
    }
}

fn captured_ids(fixture: &Value) -> (String, String, String) {
    let ids: Vec<String> = runs(fixture)
        .iter()
        .flat_map(|run| run["rollout_session_meta"].as_array().expect("meta"))
        .map(|record| record["payload"]["id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(ids.len(), 3, "exec, TUI startup, TUI after /clear");
    (ids[0].clone(), ids[1].clone(), ids[2].clone())
}

#[test]
fn captured_exec_tui_and_clear_hooks_verify_by_payload_path() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    assert_eq!(hooks.len(), 9, "captured Codex hook events");
    let mut accepted_paths = Vec::new();
    for (id, path, expected) in &hooks {
        let verified = verify_codex_hook_source(dir.path(), &claim(id, Some(path), *expected))
            .unwrap_or_else(|error| panic!("{id} rejected: {error:?}"));
        assert_eq!(&verified.session_id, id);
        assert_eq!(verified.route, CodexHookSourceRoute::PayloadPath);
        assert_eq!(
            verified.rollout_path,
            path.canonicalize().expect("canonical")
        );
        accepted_paths.push(verified.rollout_path);
    }
    accepted_paths.sort();
    accepted_paths.dedup();
    // /clear moves the TUI to a new id and a new rollout, so three sources in total.
    assert_eq!(accepted_paths.len(), 3);
}

#[test]
fn payload_path_naming_another_session_is_rejected() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (_, startup, cleared) = captured_ids(&fixture);
    let cleared_path = &hooks
        .iter()
        .find(|hook| hook.0 == cleared)
        .expect("clear")
        .1;
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(cleared_path), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::FileNameMismatch));

    let renamed = dir.path().join(format!("session-{startup}.jsonl"));
    std::fs::copy(cleared_path, &renamed).expect("copy");
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&renamed), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::FileNameMismatch));
}

#[test]
fn rollout_whose_session_meta_names_another_session_is_rejected() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (_, startup, cleared) = captured_ids(&fixture);
    let startup_path = &hooks
        .iter()
        .find(|hook| hook.0 == startup)
        .expect("startup")
        .1;
    let tui = &runs(&fixture)[1];
    // The startup file name, but the header /clear wrote for the next session.
    write_rollout(startup_path, meta_for(tui, &cleared));
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::SessionMetaIdMismatch));
}

#[test]
fn rollout_source_must_match_the_launch_mode() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (exec, startup, _) = captured_ids(&fixture);
    let exec_path = &hooks.iter().find(|hook| hook.0 == exec).expect("exec").1;
    let startup_path = &hooks
        .iter()
        .find(|hook| hook.0 == startup)
        .expect("startup")
        .1;

    let exec_as_tui = verify_codex_hook_source(
        dir.path(),
        &claim(&exec, Some(exec_path), CodexRolloutSource::Cli),
    );
    assert_eq!(
        exec_as_tui,
        Err(CodexHookSourceRejection::SourceMismatch {
            found: Some("exec".to_string())
        })
    );
    let tui_as_exec = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(startup_path), CodexRolloutSource::Exec),
    );
    assert_eq!(
        tui_as_exec,
        Err(CodexHookSourceRejection::SourceMismatch {
            found: Some("cli".to_string())
        })
    );

    let tui = &runs(&fixture)[1];
    let mut header = meta_for(tui, &startup).clone();
    header["payload"]["originator"] = json!("codex_exec");
    write_rollout(startup_path, &header);
    let exec_originator = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
    );
    assert!(matches!(
        exec_originator,
        Err(CodexHookSourceRejection::SourceMismatch { .. })
    ));

    header["payload"]["originator"] = json!("codex-tui");
    header["payload"]["source"] = json!({"subagent": {"other": "review"}});
    write_rollout(startup_path, &header);
    let child = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
    );
    assert_eq!(
        child,
        Err(CodexHookSourceRejection::SourceMismatch { found: None })
    );
}

#[test]
fn rollout_without_session_meta_is_never_accepted() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (_, startup, _) = captured_ids(&fixture);
    let startup_path = &hooks
        .iter()
        .find(|hook| hook.0 == startup)
        .expect("startup")
        .1;
    let header = meta_for(&runs(&fixture)[1], &startup).to_string();
    // Nothing yet, or a header still being written: retry later.
    for body in [String::new(), header.clone()] {
        std::fs::write(startup_path, body).expect("write old rollout");
        let result = verify_codex_hook_source(
            dir.path(),
            &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
        );
        assert_eq!(result, Err(CodexHookSourceRejection::SessionMetaMissing));
        assert!(result.unwrap_err().may_resolve_later());
    }
    std::fs::write(
        startup_path,
        "{\"type\":\"event_msg\"}\n{\"type\":\"response_item\"}\n",
    )
    .expect("write old rollout");
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
    );
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::FirstRecordNotSessionMeta)
    );
    assert!(!result.unwrap_err().may_resolve_later());
}

#[test]
fn malformed_first_record_is_rejected_even_with_a_valid_header_after_it() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (_, startup, _) = captured_ids(&fixture);
    let startup_path = &hooks
        .iter()
        .find(|hook| hook.0 == startup)
        .expect("startup")
        .1;
    let header = meta_for(&runs(&fixture)[1], &startup);
    let mut without_cwd = header.clone();
    without_cwd["payload"]
        .as_object_mut()
        .expect("payload")
        .remove("cwd");
    for first in [
        "{\"type\":\"session_meta\",".to_string(),
        json!({"type": "event_msg"}).to_string(),
        without_cwd.to_string(),
    ] {
        std::fs::write(startup_path, format!("{first}\n{header}\n")).expect("write rollout");
        let result = verify_codex_hook_source(
            dir.path(),
            &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
        );
        assert_eq!(
            result,
            Err(CodexHookSourceRejection::FirstRecordNotSessionMeta),
            "{first}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_payload_path_must_resolve_to_its_own_rollout_inside_the_root() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let (_, startup, cleared) = captured_ids(&fixture);
    let header = meta_for(&runs(&fixture)[1], &startup);
    let name = format!("rollout-2026-09-27T21-04-25-{startup}.jsonl");
    let link = dir.path().join("2026/09/27").join(&name);
    std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");

    let other_name = dir
        .path()
        .join(format!("rollout-2026-09-27T21-04-26-{cleared}.jsonl"));
    write_rollout(&other_name, header);
    std::os::unix::fs::symlink(&other_name, &link).expect("symlink");
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&link), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::FileNameMismatch));

    let outside = elsewhere.path().join(&name);
    write_rollout(&outside, header);
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(&outside, &link).expect("symlink");
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&link), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::OutsideSessionsRoot));

    let same_name = dir.path().join("moved").join(&name);
    write_rollout(&same_name, header);
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(&same_name, &link).expect("symlink");
    let verified = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&link), CodexRolloutSource::Cli),
    )
    .expect("in-root target with its own name");
    assert_eq!(
        verified.rollout_path,
        same_name.canonicalize().expect("canonical")
    );
}

#[cfg(unix)]
#[test]
fn rollout_replaced_during_verification_is_not_accepted() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let (_, startup, _) = captured_ids(&fixture);
    let header = meta_for(&runs(&fixture)[1], &startup);
    let day = dir.path().join("2026/09/27");
    let name = format!("rollout-2026-09-27T21-04-25-{startup}.jsonl");
    let path = day.join(&name);
    write_rollout(&path, header);
    let replacement = dir.path().join("replacement.jsonl");
    let parked = dir.path().join("parked.jsonl");
    let verify = || {
        verify_codex_hook_source(
            dir.path(),
            &claim(&startup, Some(&path), CodexRolloutSource::Cli),
        )
    };

    // Same header, different inode: the header read is no longer what the path names.
    write_rollout(&replacement, header);
    let (from, to) = (replacement.clone(), path.clone());
    before_final_identity(move || std::fs::rename(from, to).expect("swap"));
    assert_eq!(verify(), Err(CodexHookSourceRejection::RolloutReplaced));

    // A -> B -> A: the header came from A's descriptor and A is back, so A is the source.
    let original = std::fs::metadata(&path).expect("stat");
    write_rollout(&replacement, header);
    let (a, b, spare) = (path.clone(), replacement.clone(), parked.clone());
    before_final_identity(move || {
        std::fs::rename(&a, &spare).expect("park A");
        std::fs::rename(&b, &a).expect("B in");
        std::fs::rename(&a, &b).expect("B out");
        std::fs::rename(&spare, &a).expect("A back");
    });
    let verified = verify().expect("restored original");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        verified.identity,
        crate::services::cluster::stream_relay::SourceFileIdentity::Unix {
            dev: original.dev(),
            ino: original.ino(),
        }
    );

    // The dated directory swapped for a symlink out of the root after the open.
    write_rollout(&elsewhere.path().join(&name), header);
    let (leaf, outside, spare_dir) = (
        day.clone(),
        elsewhere.path().to_path_buf(),
        dir.path().join("old-day"),
    );
    before_final_identity(move || {
        std::fs::rename(&leaf, spare_dir).expect("move day");
        std::os::unix::fs::symlink(outside, &leaf).expect("symlink day");
    });
    assert_eq!(verify(), Err(CodexHookSourceRejection::OutsideSessionsRoot));
}

#[test]
fn payload_path_outside_the_root_or_not_yet_written_is_not_accepted() {
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let (_, startup, _) = captured_ids(&fixture);
    let tui = &runs(&fixture)[1];
    let name = format!("rollout-2026-09-27T21-04-25-{startup}.jsonl");

    let outside = elsewhere.path().join(&name);
    write_rollout(&outside, meta_for(tui, &startup));
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&outside), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::OutsideSessionsRoot));

    let absent = dir.path().join("2026/09/27").join(&name);
    let result = verify_codex_hook_source(
        dir.path(),
        &claim(&startup, Some(&absent), CodexRolloutSource::Cli),
    );
    assert_eq!(result, Err(CodexHookSourceRejection::RolloutUnavailable));
    assert!(result.unwrap_err().may_resolve_later());

    for bad in [
        startup.to_uppercase(),
        format!("{{{startup}}}"),
        String::new(),
    ] {
        let result =
            verify_codex_hook_source(dir.path(), &claim(&bad, None, CodexRolloutSource::Cli));
        assert_eq!(
            result,
            Err(CodexHookSourceRejection::InvalidSessionId),
            "{bad}"
        );
    }
}

#[test]
fn missing_payload_path_uses_the_single_rollout_index_candidate() {
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let hooks = materialize(dir.path(), &fixture);
    let (exec, startup, cleared) = captured_ids(&fixture);
    for (id, expected) in [
        (&exec, CodexRolloutSource::Exec),
        (&startup, CodexRolloutSource::Cli),
        (&cleared, CodexRolloutSource::Cli),
    ] {
        let path = &hooks.iter().find(|hook| &hook.0 == id).expect("hook").1;
        let verified = verify_codex_hook_source(dir.path(), &claim(id, None, expected))
            .unwrap_or_else(|error| panic!("{id} rejected: {error:?}"));
        assert_eq!(verified.route, CodexHookSourceRoute::RolloutIndex);
        assert_eq!(
            verified.rollout_path,
            path.canonicalize().expect("canonical")
        );
    }
    let wrong_mode =
        verify_codex_hook_source(dir.path(), &claim(&exec, None, CodexRolloutSource::Cli));
    assert!(matches!(
        wrong_mode,
        Err(CodexHookSourceRejection::SourceMismatch { .. })
    ));
}

#[test]
fn rollout_index_without_a_candidate_is_pending_not_accepted() {
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_, startup, _) = captured_ids(&fixture);
    let result =
        verify_codex_hook_source(dir.path(), &claim(&startup, None, CodexRolloutSource::Cli));
    assert_eq!(result, Err(CodexHookSourceRejection::NoCandidate));
    assert!(result.unwrap_err().may_resolve_later());
}

#[test]
fn rollout_index_with_two_candidates_rejects_instead_of_picking_one() {
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let tui = &runs(&fixture)[1];
    let (_, startup, _) = captured_ids(&fixture);
    let header = meta_for(tui, &startup);

    let same_name = tempfile::tempdir().expect("tempdir");
    for day in ["2026/09/27", "2026/09/28"] {
        let path = same_name
            .path()
            .join(day)
            .join(format!("rollout-2026-09-27T21-04-25-{startup}.jsonl"));
        write_rollout(&path, header);
    }
    let result = verify_codex_hook_source(
        same_name.path(),
        &claim(&startup, None, CodexRolloutSource::Cli),
    );
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::AmbiguousCandidates(2))
    );
    assert!(!result.unwrap_err().may_resolve_later());

    // A second file that only claims the id in its header still makes the lookup ambiguous.
    let header_only = tempfile::tempdir().expect("tempdir");
    let named = header_only
        .path()
        .join(format!("rollout-2026-09-27T21-04-25-{startup}.jsonl"));
    write_rollout(&named, header);
    let other = header_only.path().join(format!(
        "rollout-2026-09-27T21-04-26-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    write_rollout(&other, header);
    let result = verify_codex_hook_source(
        header_only.path(),
        &claim(&startup, None, CodexRolloutSource::Cli),
    );
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::AmbiguousCandidates(2))
    );
}

#[cfg(unix)]
#[test]
fn incomplete_rollout_index_lookup_is_retryable_not_a_single_candidate() {
    use std::os::unix::fs::PermissionsExt;
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let (_, startup, _) = captured_ids(&fixture);
    let header = meta_for(&runs(&fixture)[1], &startup);
    let dir = tempfile::tempdir().expect("tempdir");
    let name = format!("rollout-2026-09-27T21-04-25-{startup}.jsonl");
    write_rollout(&dir.path().join("2026/09/27").join(&name), header);
    let hidden = dir.path().join("2026/09/28");
    write_rollout(&hidden.join(&name), header);
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let unreadable = std::fs::read_dir(&hidden).is_err();
    let result =
        verify_codex_hook_source(dir.path(), &claim(&startup, None, CodexRolloutSource::Cli));
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        unreadable,
        "the test user must not bypass directory permissions"
    );
    assert_eq!(result, Err(CodexHookSourceRejection::IndexIncomplete));
    assert!(result.unwrap_err().may_resolve_later());
    // The existing discovery path still skips the unreadable directory silently.
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let discovered =
        crate::services::codex_tui::rollout_index::rollout_files_under(dir.path()).len();
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(discovered, 1);
}

/// Writes the rollout named for `id` plus a differently named competitor whose header claims `id`.
fn named_and_competitor(root: &Path, id: &str, header: &Value) -> PathBuf {
    let day = root.join("2026/09/27");
    write_rollout(
        &day.join(format!("rollout-2026-09-27T21-04-25-{id}.jsonl")),
        header,
    );
    let competitor = day.join(format!(
        "rollout-2026-09-27T21-04-26-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    write_rollout(&competitor, header);
    competitor
}

#[cfg(unix)]
#[test]
fn rollout_index_rereads_competitors_the_discovery_cache_cannot_vouch_for() {
    use std::os::unix::fs::PermissionsExt;
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let (_, startup, _) = captured_ids(&fixture);
    let header = meta_for(&runs(&fixture)[1], &startup);
    let dir = tempfile::tempdir().expect("tempdir");
    let competitor = named_and_competitor(dir.path(), &startup, header);
    let set_mode = |mode| {
        std::fs::set_permissions(&competitor, std::fs::Permissions::from_mode(mode)).expect("chmod")
    };
    let verify =
        || verify_codex_hook_source(dir.path(), &claim(&startup, None, CodexRolloutSource::Cli));
    assert_eq!(
        verify(),
        Err(CodexHookSourceRejection::AmbiguousCandidates(2))
    );

    // Unreadable while discovery fills the cache, so the cache holds a negative header.
    set_mode(0o000);
    let unreadable = std::fs::File::open(&competitor).is_err();
    warm_cache_for_tests(dir.path());
    let cached_negative = cached_meta_for_tests(dir.path(), &competitor);
    let result = verify();
    set_mode(0o644);
    assert!(unreadable, "the test user must not bypass file permissions");
    assert_eq!(
        cached_negative,
        Some(None),
        "discovery must have cached the unreadable header as absent"
    );
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::IndexIncomplete),
        "a cached negative header must not hide an unreadable competitor"
    );
    assert!(result.unwrap_err().may_resolve_later());

    // Readable while the cache fills, unreadable at lookup: permissions leave (mtime, len) alone.
    reset_cache_for_tests();
    warm_cache_for_tests(dir.path());
    let cached_header = cached_meta_for_tests(dir.path(), &competitor);
    set_mode(0o000);
    let result = verify();
    let discovered = warm_cache_for_tests(dir.path());
    set_mode(0o644);
    assert!(matches!(cached_header, Some(Some(_))));
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::IndexIncomplete),
        "a cached header must not stand in for a competitor that can no longer be read"
    );
    assert!(result.unwrap_err().may_resolve_later());
    // Discovery itself still serves the cached header for the unchanged file.
    let served = discovered
        .iter()
        .find(|item| item.path == competitor)
        .and_then(|item| item.meta.as_ref())
        .and_then(|meta| meta.id.clone());
    assert_eq!(served.as_deref(), Some(startup.as_str()));
}

#[test]
fn rollout_index_header_read_error_is_incomplete_not_absent() {
    let _index = lock_cache_for_tests();
    let fixture = fixture();
    let (_, startup, _) = captured_ids(&fixture);
    let header = meta_for(&runs(&fixture)[1], &startup);
    let dir = tempfile::tempdir().expect("tempdir");
    let competitor = named_and_competitor(dir.path(), &startup, header);
    let verify =
        || verify_codex_hook_source(dir.path(), &claim(&startup, None, CodexRolloutSource::Cli));

    fail_header_reads_for_tests(Some(competitor.clone()));
    let result = verify();
    fail_header_reads_for_tests(None);
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::IndexIncomplete),
        "a competitor whose header read fails must not count as absent"
    );
    assert!(result.unwrap_err().may_resolve_later());
    assert_eq!(
        verify(),
        Err(CodexHookSourceRejection::AmbiguousCandidates(2))
    );
}

#[cfg(unix)]
#[test]
fn header_is_read_from_the_open_descriptor_not_by_reopening_the_path() {
    use std::cell::RefCell;
    use std::os::unix::fs::MetadataExt;
    use std::rc::Rc;
    let fixture = fixture();
    let (_, startup, _) = captured_ids(&fixture);
    let a_header = meta_for(&runs(&fixture)[1], &startup);
    let mut b_header = a_header.clone();
    b_header["payload"]["id"] = json!(uuid::Uuid::new_v4().to_string());
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("2026/09/27")
        .join(format!("rollout-2026-09-27T21-04-25-{startup}.jsonl"));
    write_rollout(&path, a_header);
    let original = std::fs::metadata(&path).expect("stat");
    let a_identity = crate::services::cluster::stream_relay::SourceFileIdentity::Unix {
        dev: original.dev(),
        ino: original.ino(),
    };
    let b = dir.path().join("replacement.jsonl");
    let spare = dir.path().join("parked.jsonl");
    let log = Rc::new(RefCell::new(Vec::new()));
    let verify = || {
        verify_codex_hook_source(
            dir.path(),
            &claim(&startup, Some(&path), CodexRolloutSource::Cli),
        )
    };
    let swap_in = {
        let (a, b, spare, log) = (path.clone(), b.clone(), spare.clone(), log.clone());
        move || {
            let ok = std::fs::rename(&a, &spare).is_ok() && std::fs::rename(&b, &a).is_ok();
            log.borrow_mut().push(("after-open", ok));
        }
    };

    // B names another session while the header is read; A is back before the final check.
    write_rollout(&b, &b_header);
    at_verify_step(VerifyStep::AfterOpen, swap_in.clone());
    at_verify_step(VerifyStep::AfterHeader, {
        let (a, b, spare, log) = (path.clone(), b.clone(), spare.clone(), log.clone());
        move || {
            let ok = std::fs::rename(&a, &b).is_ok() && std::fs::rename(&spare, &a).is_ok();
            log.borrow_mut().push(("after-header", ok));
        }
    });
    let result = verify().map(|verified| verified.identity);
    assert_eq!(
        log.take(),
        [("after-open", true), ("after-header", true)],
        "both read-window seams must fire in order and swap the files"
    );
    assert_eq!(
        result,
        Ok(a_identity),
        "identity and header must both come from A's open descriptor"
    );

    // B stays in place: the path no longer names the descriptor whose header was read.
    at_verify_step(VerifyStep::AfterOpen, swap_in);
    let result = verify();
    assert_eq!(log.take(), [("after-open", true)]);
    assert_eq!(
        result,
        Err(CodexHookSourceRejection::RolloutReplaced),
        "a header read through the descriptor sees A and the final check must see B"
    );
}

#[test]
fn captured_cli_advertising_the_bypass_has_hooks_only_through_it() {
    let fixture = fixture();
    let version = fixture["cli_version"].as_str().expect("cli_version");
    let activation = &fixture["activation"];
    assert_eq!(activation["resume_help_advertises_bypass"], true);
    let capability = codex_hook_capability(Some(version), true);
    assert_eq!(capability.activation, CodexHookActivation::BypassRequired);
    assert!(capability.hooks_available());
    // The captured negative trial is the evidence behind ObservedInactive.
    let hash_only = &activation["trials"][0];
    assert_eq!(hash_only["bypass_hook_trust"], false);
    assert_eq!(hash_only["agentdesk_hooks_fired"], false);
    assert_eq!(
        capability.trust_hash,
        CodexTrustHashEvidence::ObservedInactive
    );
}

#[test]
fn cli_without_the_bypass_has_no_hooks_whatever_its_trust_hashes() {
    let fixture = fixture();
    let captured = fixture["cli_version"].as_str().expect("cli_version");
    for version in [
        Some(captured),
        Some("codex-cli 0.130.0"),
        Some("codex-cli 9.9.9"),
        None,
    ] {
        let capability = codex_hook_capability(version, false);
        assert_eq!(
            capability.activation,
            CodexHookActivation::Unavailable,
            "{version:?}"
        );
        assert!(!capability.hooks_available(), "{version:?}");
    }
}

#[test]
fn trust_hash_path_is_reported_inactive_or_unverified_never_usable() {
    let fixture = fixture();
    let captured = fixture["cli_version"].as_str().expect("cli_version");
    assert_eq!(
        codex_hook_capability(Some(&format!(" {captured}\n")), false).trust_hash,
        CodexTrustHashEvidence::ObservedInactive
    );
    for version in [
        Some("codex-cli 0.130.0"),
        Some("codex-cli 0.144.1"),
        Some("codex-cli 9.9.9"),
        None,
    ] {
        for advertised in [false, true] {
            let capability = codex_hook_capability(version, advertised);
            assert_eq!(
                capability.trust_hash,
                CodexTrustHashEvidence::Unverified,
                "{version:?}"
            );
            assert_eq!(capability.hooks_available(), advertised, "{version:?}");
        }
    }
}

mod first_proof_tests {
    use super::super::{CodexFirstProof, codex_first_proof_candidate, first_prompt_matches};
    use super::*;
    use crate::services::tui_prompt_dedupe::binding_context::{self, PreparedIncarnation};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn digest(prompt: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))
    }

    #[test]
    fn first_prompt_digest_requires_exact_utf8_and_a_string() {
        for prompt in ["한글", "a\nb\n", &"가나다\n".repeat(700)] {
            let expected = digest(prompt);
            assert!(first_prompt_matches(Some(&expected), &json!(prompt)));
            assert!(!first_prompt_matches(
                Some(&expected),
                &json!(format!("{prompt} "))
            ));
            assert!(!first_prompt_matches(Some(&expected), &Value::Null));
            assert!(!first_prompt_matches(None, &json!(prompt)));
            for malformed in [
                expected.to_uppercase(),
                expected[7..].to_owned(),
                "sha256:0".into(),
                format!("{expected}0"),
            ] {
                assert!(!first_prompt_matches(Some(&malformed), &json!(prompt)));
            }
        }
        assert!(!first_prompt_matches(
            Some(&digest("é")),
            &json!("e\u{301}")
        ));
    }

    #[test]
    fn first_proof_candidate_requires_canonical_current_fresh_launch_and_strict_parent() {
        let (_runtime, _guards) = binding_context::tests::fixture();
        let sessions = tempfile::tempdir().unwrap();
        let prepared = PreparedIncarnation::prepare_at(
            "codex",
            "proof-candidate-test",
            Some(42),
            None,
            false,
            Some(sessions.path().into()),
        )
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
        let parent =
            json!({"type":"session_meta","payload":{"id":id,"cwd":"/synthetic","source":"cli"}});
        let write = |header: &Value| std::fs::write(&path, format!("{header}\n")).unwrap();
        write(&parent);
        let prompt = json!("첫 argv\n문자열");
        let expected = digest(prompt.as_str().unwrap());
        let mut launch = CodexFirstProof {
            captured: &prepared.context,
            prepared: &prepared.context,
            current_nonce: Some(&prepared.context.execution_nonce),
            verified_fresh_spawn: true,
            no_prior_claim_or_transition: true,
            event: "UserPromptSubmit",
            source: None,
            expected_digest: Some(&expected),
            prompt: &prompt,
        };
        let claim = CodexHookSourceClaim {
            session_id: &id,
            transcript_path: Some(&path),
            expected_source: CodexRolloutSource::Cli,
        };
        let accepted = codex_first_proof_candidate(&launch, &claim).unwrap();
        assert_eq!(accepted.rollout_path, path.canonicalize().unwrap());
        let canonical_bytes = std::fs::read(&prepared.path).unwrap();
        launch.expected_digest = None;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        launch.expected_digest = Some(&expected);
        let wrong_prompt = json!("other argv");
        launch.prompt = &wrong_prompt;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        launch.prompt = &prompt;
        for event in ["Stop", "PreCompact", "PostCompact"] {
            launch.event = event;
            assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        }
        launch.event = "SessionStart";
        for source in [None, Some("clear"), Some("resume"), Some("fork")] {
            launch.source = source;
            assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        }
        launch.source = Some("startup");
        launch.expected_digest = None;
        assert!(codex_first_proof_candidate(&launch, &claim).is_ok());
        launch.verified_fresh_spawn = false;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        launch.verified_fresh_spawn = true;
        launch.no_prior_claim_or_transition = false;
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        launch.no_prior_claim_or_transition = true;
        for nonce in [None, Some("older-nonce")] {
            launch.current_nonce = nonce;
            assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        }
        launch.current_nonce = Some(&prepared.context.execution_nonce);
        for field in [
            "channel", "tmux", "owner", "host", "root", "mode", "expected", "schema", "provider",
        ] {
            let mut foreign = prepared.context.clone();
            match field {
                "channel" => foreign.channel_id = Some(43),
                "tmux" => foreign.tmux_session = "foreign".into(),
                "owner" => foreign.owner_runtime_root = "foreign".into(),
                "host" => foreign.host = Some("foreign".into()),
                "root" => foreign.provider_root = Some("/foreign".into()),
                "mode" => foreign.launch_mode = "resume".into(),
                "expected" => foreign.expected_native_session_id = Some(id.clone()),
                "schema" => foreign.schema = 2,
                "provider" => foreign.provider = "claude".into(),
                _ => unreachable!(),
            }
            let foreign_launch = CodexFirstProof {
                captured: &foreign,
                ..launch
            };
            assert!(
                codex_first_proof_candidate(&foreign_launch, &claim).is_err(),
                "{field}"
            );
        }
        launch.captured = &prepared.context;
        let mut child = parent.clone();
        child["payload"]["parent_thread_id"] = json!(uuid::Uuid::new_v4().to_string());
        write(&child);
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        child["payload"]
            .as_object_mut()
            .unwrap()
            .remove("parent_thread_id");
        child["payload"]["source"] = json!({"subagent":{"thread_spawn":{}}});
        write(&child);
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        let mut mismatch = parent.clone();
        mismatch["payload"]["id"] = json!(uuid::Uuid::new_v4().to_string());
        write(&mismatch);
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        write(&parent);
        let mut corrupt = prepared.context.clone();
        corrupt.channel_id = Some(43);
        std::fs::write(&prepared.path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(codex_first_proof_candidate(&launch, &claim).is_err());
        std::fs::write(&prepared.path, &canonical_bytes).unwrap();
        assert!(codex_first_proof_candidate(&launch, &claim).is_ok());
        for field in ["mode", "expected", "channel", "root"] {
            let mut invalid = prepared.context.clone();
            match field {
                "mode" => invalid.launch_mode = "resume".into(),
                "expected" => invalid.expected_native_session_id = Some(id.clone()),
                "channel" => invalid.channel_id = None,
                "root" => invalid.provider_root = None,
                _ => unreachable!(),
            }
            std::fs::write(&prepared.path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            let invalid_launch = CodexFirstProof {
                captured: &invalid,
                prepared: &invalid,
                ..launch
            };
            assert!(
                codex_first_proof_candidate(&invalid_launch, &claim).is_err(),
                "{field}"
            );
        }
        std::fs::write(&prepared.path, &canonical_bytes).unwrap();
        assert_eq!(std::fs::read(&prepared.path).unwrap(), canonical_bytes);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("{parent}\n").as_bytes()
        );
    }
}
