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

use super::{CodexFirstProof, codex_first_proof_candidate, first_prompt_matches};
use crate::services::tui_prompt_dedupe::binding_context::{self, PreparedIncarnation};
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
    let prompt = json!("첫 argv\n문자열");
    let expected = digest(prompt.as_str().unwrap());
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        "proof-candidate-test",
        Some(42),
        None,
        false,
        Some(sessions.path().into()),
        (Some(expected.clone()), Some("shadow".into())),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
    let parent =
        json!({"type":"session_meta","payload":{"id":id,"cwd":"/synthetic","source":"cli"}});
    let write = |header: &Value| std::fs::write(&path, format!("{header}\n")).unwrap();
    write(&parent);
    let mut launch = CodexFirstProof {
        captured: &prepared.context,
        prepared: &prepared.context,
        current_nonce: Some(&prepared.context.execution_nonce),
        verified_fresh_spawn: true,
        no_prior_claim_or_transition: true,
        event: "UserPromptSubmit",
        source: None,
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
    for field in [
        "mode", "expected", "channel", "root", "schema", "provider", "digest", "policy",
    ] {
        let mut invalid = prepared.context.clone();
        match field {
            "mode" => invalid.launch_mode = "resume".into(),
            "expected" => invalid.expected_native_session_id = Some(id.clone()),
            "channel" => invalid.channel_id = None,
            "root" => invalid.provider_root = None,
            "schema" => invalid.schema = 2,
            "provider" => invalid.provider = "claude".into(),
            "digest" => invalid.first_prompt_digest = None,
            "policy" => invalid.source_policy = Some("legacy".into()),
            _ => unreachable!(),
        }
        std::fs::write(&prepared.path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let invalid_launch = CodexFirstProof {
            event: "UserPromptSubmit",
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
    let context_mtime = std::fs::metadata(&prepared.path)
        .unwrap()
        .modified()
        .unwrap();
    let native_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert!(codex_first_proof_candidate(&launch, &claim).is_ok());
    assert_eq!(
        std::fs::metadata(&prepared.path)
            .unwrap()
            .modified()
            .unwrap(),
        context_mtime
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        native_mtime
    );
    assert_eq!(std::fs::read(&prepared.path).unwrap(), canonical_bytes);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        format!("{parent}\n").as_bytes()
    );
}

#[test]
fn shadow_ingress_is_readonly_before_equality_and_without_a_source_map() {
    use crate::services::claude_tui::hook_server::observation_ingress::{
        IngressOutcome, ProceedReason, observe_binding_hook,
    };
    use crate::services::codex::{CodexSourceMode, SOURCE_MODE_TEST};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use binding_context::{BINDING_HEADER, CapturedContext, HookBindingEnvelope};
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn capture(run: impl FnOnce()) -> String {
        let sink = Sink(Arc::new(Mutex::new(Vec::new())));
        let copy = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || copy.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = sink.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }
    fn tree(path: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for item in entries.flatten() {
                if item.path().is_dir() {
                    files.extend(tree(&item.path()));
                } else {
                    files.push((
                        item.path(),
                        std::fs::read(item.path()).unwrap(),
                        item.metadata().unwrap().modified().unwrap(),
                    ));
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (runtime, _guards) = binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    let sessions = tempfile::tempdir().unwrap();
    let prompt = "SYNTHETIC_PRIVATE_PROMPT\n한글";
    let tmux = "shadow-source-less";
    let id = uuid::Uuid::new_v4().to_string();
    let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":id,"source":"cli","cwd":"/synthetic"}})
        ),
    )
    .unwrap();
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        tmux,
        Some(42),
        None,
        false,
        Some(sessions.path().into()),
        (Some(digest(prompt)), Some("shadow".into())),
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(prepared.context.clone()),
        observed: Default::default(),
    };
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let payload = json!({"prompt":prompt,"transcript_path":path});
    binding_events::set_test_root(Some(runtime.path()));
    let watch = binding_events::subscribe_binding_events(42).unwrap();
    let before = tree(runtime.path());
    let native_before = tree(sessions.path());
    dedupe::SHADOW_IO_CALLS.with(|calls| calls.set(0));
    for event in ["pre_tool_use", "post_tool_use", "stop"] {
        let hook = binding_events::HookSignal::from_payload(event, &payload);
        let trace = capture(|| {
            dedupe::observe_codex_shadow(None, Some(&id), &payload, &hook, Some(&envelope));
        });
        assert_eq!(
            dedupe::SHADOW_IO_CALLS.with(|calls| calls.get()),
            0,
            "tool/stop hooks must return before context, marker, or history IO"
        );
        assert!(
            trace.is_empty(),
            "non-proof hook emitted a shadow trace: {trace}"
        );
    }
    let route = |command, mode, headers: &axum::http::HeaderMap| {
        SOURCE_MODE_TEST.with(|m| m.set(Some(mode)));
        let trace = capture(|| {
            let outcome = observe_binding_hook(
                "codex",
                "user_prompt_submit",
                command,
                Some(&id),
                &payload,
                &headers,
            );
            assert_eq!(
                outcome,
                IngressOutcome::Proceed(ProceedReason::NoSessionSwitch)
            );
        });
        SOURCE_MODE_TEST.with(|m| m.set(None));
        trace
    };
    for mode in [
        CodexSourceMode::parse(None),
        CodexSourceMode::parse(Some("legacy")),
        CodexSourceMode::Verified,
        CodexSourceMode::Invalid,
    ] {
        let trace = route(Some(id.as_str()), mode, &headers);
        assert!(
            !trace.contains("ObservationOnly"),
            "legacy/non-shadow must not call the observer: {trace}"
        );
    }
    for command in [None, Some(id.as_str())] {
        let trace = route(command, CodexSourceMode::Shadow, &headers);
        assert!(
            trace.contains("ObservationOnly")
                && trace.contains("candidate")
                && trace.contains("source_less=true"),
            "{trace}"
        );
        for key in [
            "channel",
            "tmux",
            "nonce",
            "root",
            "native_uuid",
            "path",
            "identity",
            "event",
            "legacy_selected_id",
            "command_present",
            "ownership_promoted=false",
        ] {
            assert!(trace.contains(key), "{key}: {trace}");
        }
        assert!(!trace.contains(prompt) && !trace.contains("SYNTHETIC_PRIVATE_PROMPT"));
    }
    assert_eq!(dedupe::SHADOW_IO_CALLS.with(|calls| calls.get()), 2);
    assert_eq!(tree(runtime.path()), before);
    assert_eq!(tree(sessions.path()), native_before);
    assert!(!watch.has_changed().unwrap());
    assert!(
        binding_events::binding_events_since(42, 0)
            .unwrap()
            .is_empty()
    );
    let binding = dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(id.clone()),
        last_offset: 13,
        relay_last_offset: Some(7),
    };
    dedupe::register_tmux_runtime_binding(tmux, binding);
    let binding_before = format!("{:?}", dedupe::peek_tmux_runtime_binding(tmux));
    let before = tree(runtime.path());
    let trace = route(Some(id.as_str()), CodexSourceMode::Shadow, &headers);
    assert!(trace.contains("source_less=false"));
    assert_eq!(
        format!("{:?}", dedupe::peek_tmux_runtime_binding(tmux)),
        binding_before
    );
    assert_eq!(tree(runtime.path()), before);
    assert!(!watch.has_changed().unwrap());

    // Execute rendered commands through the native UUID query, real ordered worker,
    // local HTTP receiver and production observer. Queue receipts are not authority.
    let _context_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_BINDING_CONTEXT",
        &prepared.path,
    );
    let mut baseline = None;
    for mode in [
        CodexSourceMode::parse(None),
        CodexSourceMode::Legacy,
        CodexSourceMode::Shadow,
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let root = runtime.path().to_owned();
        let native_id = id.clone();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            use tower::ServiceExt;
            SOURCE_MODE_TEST.with(|m| m.set(Some(mode)));
            binding_events::set_test_root(Some(&root));
            let app = crate::services::claude_tui::hook_server::hook_receiver_router_with_state(
                crate::services::claude_tui::hook_server::HookServerState::new(),
            );
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut responses = Vec::new();
            let trace = capture(|| {
                for event in ["UserPromptSubmit", "Stop"] {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    let mut socket = loop {
                        match listener.accept() {
                            Ok((socket, _)) => break socket,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "ordered HTTP transport missing"
                                );
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            Err(e) => panic!("accept ordered HTTP: {e}"),
                        }
                    };
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut encoded = Vec::new();
                    let (head_end, length) = loop {
                        let mut chunk = [0; 4096];
                        let n = socket.read(&mut chunk).unwrap();
                        assert!(n > 0);
                        encoded.extend_from_slice(&chunk[..n]);
                        if let Some(end) = encoded.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&encoded[..end]);
                            let length: usize = head
                                .lines()
                                .find_map(|l| {
                                    let (key, value) = l.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                                .unwrap();
                            if encoded.len() >= end + 4 + length {
                                break (end + 4, length);
                            }
                        }
                    };
                    let head = String::from_utf8_lossy(&encoded[..head_end]);
                    let uri = head
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    assert_eq!(uri, format!("/hooks/codex/{event}?session_id={native_id}"));
                    let mut request = axum::http::Request::builder().method("POST").uri(uri);
                    for line in head.lines().skip(1).filter(|l| !l.is_empty()) {
                        let (name, value) = line.split_once(':').unwrap();
                        request = request.header(name.trim(), value.trim());
                    }
                    let request = request
                        .body(axum::body::Body::from(
                            encoded[head_end..head_end + length].to_vec(),
                        ))
                        .unwrap();
                    assert!(request.headers().contains_key(BINDING_HEADER));
                    assert!(
                        request
                            .headers()
                            .contains_key("x-agentdesk-relay-request-id")
                    );
                    let response = executor.block_on(app.clone().oneshot(request)).unwrap();
                    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
                    let body = executor
                        .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
                        .unwrap();
                    responses.push(serde_json::from_slice::<Value>(&body).unwrap());
                    write!(socket, "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    socket.write_all(&body).unwrap();
                }
            });
            binding_events::set_test_root(None);
            SOURCE_MODE_TEST.with(|m| m.set(None));
            (trace, responses)
        });
        let rendered = crate::services::claude_tui::hook_bundle::render_codex_hook_config_override(
            &crate::services::claude_tui::hook_bundle::HookBundleConfig {
                endpoint,
                provider: "codex".into(),
                session_id: "stable-not-native".into(),
                agentdesk_exe: "/synthetic/agentdesk".into(),
            },
        );
        for event in ["UserPromptSubmit", "Stop"] {
            let event_rendered = &rendered[rendered.find(&format!("{event}=[")).unwrap()..];
            let start = event_rendered.find("command=").unwrap() + "command=".len();
            let end = event_rendered[start..].find(",timeout=").unwrap() + start;
            let command: String = serde_json::from_str(&event_rendered[start..end]).unwrap();
            let shell = std::process::Command::new("/bin/bash")
                .args(["-c", &format!("set -- {command}; printf '%s\\0' \"$@\"")])
                .output()
                .unwrap();
            assert!(shell.status.success());
            let args: Vec<String> = shell
                .stdout
                .split(|b| *b == 0)
                .filter(|v| !v.is_empty())
                .map(|v| String::from_utf8(v.to_vec()).unwrap())
                .collect();
            let mut native_payload = payload.clone();
            native_payload["session_id"] = json!(id);
            crate::services::claude_tui::hook_relay::run_rendered_codex_hook_for_test(
                &args,
                &serde_json::to_vec(&native_payload).unwrap(),
            );
        }
        let (trace, responses) = server.join().unwrap();
        assert_eq!(
            trace.contains("ObservationOnly"),
            mode == CodexSourceMode::Shadow
        );
        if mode == CodexSourceMode::Shadow {
            assert!(trace.contains("candidate"));
        }
        assert!(!trace.contains("SYNTHETIC_PRIVATE_PROMPT"));
        if let Some(expected) = &baseline {
            assert_eq!(
                &responses, expected,
                "mode must not change HTTP/routing/refusal"
            );
        } else {
            baseline = Some(responses);
        }
        assert_eq!(
            format!("{:?}", dedupe::peek_tmux_runtime_binding(tmux)),
            binding_before
        );
        for (file, bytes, modified) in &before {
            assert_eq!(&std::fs::read(file).unwrap(), bytes);
            assert_eq!(
                &std::fs::metadata(file).unwrap().modified().unwrap(),
                modified
            );
        }
        assert_eq!(tree(sessions.path()), native_before);
        assert!(!watch.has_changed().unwrap());
        assert!(
            binding_events::binding_events_since(42, 0)
                .unwrap()
                .is_empty()
        );
    }

    std::fs::write(&path, "").unwrap();
    assert!(route(Some(id.as_str()), CodexSourceMode::Shadow, &headers).contains("pending"));
    std::fs::write(&path, format!("{}\n",json!({"type":"session_meta","payload":{"id":id,"source":{"subagent":{}},"cwd":"/synthetic"}}))).unwrap();
    assert!(route(Some(id.as_str()), CodexSourceMode::Shadow, &headers).contains("rejected"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":id,"source":"cli","cwd":"/synthetic"}})
        ),
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce"),
        "stale",
    )
    .unwrap();
    assert!(route(Some(id.as_str()), CodexSourceMode::Shadow, &headers).contains("rejected"));
    std::fs::write(
        crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    let log_dir = runtime.path().join("binding_events");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("42.log");
    let history = binding_events::BindingEvent {
        seq: 1,
        channel_id: 42,
        provider: "codex".into(),
        tmux_session: tmux.into(),
        execution_nonce: Some(prepared.context.execution_nonce.clone()),
        old: None,
        new: binding_events::BindingTarget::Rejected {
            payload_session_id: id.clone(),
            payload_transcript_path: None,
            reason: "synthetic".into(),
        },
        cause: binding_events::BindingCause::Clear,
        parent_hint: None,
        evidence: binding_events::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    std::fs::write(
        &log,
        format!("{}\n", serde_json::to_string(&history).unwrap()),
    )
    .unwrap();
    assert!(route(Some(id.as_str()), CodexSourceMode::Shadow, &headers).contains("ineligible"));
    std::fs::write(&log, "corrupt\n").unwrap();
    assert!(route(Some(id.as_str()), CodexSourceMode::Shadow, &headers).contains("ineligible"));
    std::fs::write(&log, "").unwrap();

    // A legacy pane cannot gain first UPS proof merely because the process is in shadow.
    let mut legacy = prepared.context.clone();
    legacy.source_policy = None;
    std::fs::write(&prepared.path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    headers.insert(
        BINDING_HEADER,
        HookBindingEnvelope {
            context: CapturedContext::Captured(legacy),
            observed: Default::default(),
        }
        .encode()
        .unwrap()
        .parse()
        .unwrap(),
    );
    let trace = route(Some(id.as_str()), CodexSourceMode::Shadow, &headers);
    assert!(trace.contains("ineligible"), "{trace}");
    binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[test]
fn additive_launch_context_round_trips_with_the_schema_one_old_parser() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct OldContext {
        schema: u32,
        provider: String,
        created_at: chrono::DateTime<chrono::Utc>,
        execution_nonce: String,
        tmux_session: String,
        channel_id: Option<u64>,
        owner_runtime_root: String,
        host: Option<String>,
        expected_native_session_id: Option<String>,
        launch_mode: String,
        provider_root: Option<PathBuf>,
    }
    let (_runtime, _guards) = binding_context::tests::fixture();
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        "compat-context",
        Some(42),
        None,
        false,
        Some("/synthetic/sessions".into()),
        (Some(digest("한글\nargv")), Some("shadow".into())),
    )
    .unwrap();
    let bytes = std::fs::read(&prepared.path).unwrap();
    let old: OldContext = serde_json::from_slice(&bytes).unwrap();
    let old_json = serde_json::to_value(old).unwrap();
    let new_from_old: binding_context::BindingContext =
        serde_json::from_value(old_json.clone()).unwrap();
    assert_eq!(new_from_old.first_prompt_digest, None);
    assert_eq!(new_from_old.source_policy, None);
    let mut expected = serde_json::to_value(&prepared.context).unwrap();
    expected
        .as_object_mut()
        .unwrap()
        .remove("first_prompt_digest");
    expected.as_object_mut().unwrap().remove("source_policy");
    assert_eq!(old_json, expected);
    assert_eq!(
        serde_json::from_slice::<binding_context::BindingContext>(&bytes).unwrap(),
        prepared.context
    );
    assert_eq!(serde_json::to_value(&new_from_old).unwrap(), old_json);
}
