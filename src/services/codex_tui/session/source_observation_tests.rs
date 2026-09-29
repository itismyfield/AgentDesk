//! Captured Codex 0.157.1 hooks replayed against the rollout source verifier and the
//! hook capability decision.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{
    CodexHookSourceClaim, CodexHookSourceRejection, CodexHookSourceRoute, CodexRolloutSource,
    verify_codex_hook_source,
};
use crate::services::claude_tui::hook_bundle::{
    CodexHookActivation, CodexTrustHashEvidence, codex_hook_capability,
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
    for body in [
        "",
        "{\"type\":\"event_msg\"}\n{\"type\":\"response_item\"}\n",
    ] {
        std::fs::write(startup_path, body).expect("write old rollout");
        let result = verify_codex_hook_source(
            dir.path(),
            &claim(&startup, Some(startup_path), CodexRolloutSource::Cli),
        );
        assert_eq!(result, Err(CodexHookSourceRejection::SessionMetaMissing));
        assert!(result.unwrap_err().may_resolve_later());
    }
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
