//! Hook payload and transcript facts captured from the installed Claude and Codex
//! CLIs, checked against AgentDesk's own path, trust-hash, rollout and line parsers.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde_json::Value;

use crate::services::agent_protocol::StreamMessage;
use crate::services::claude_tui::hook_bundle::{HookBundleConfig, codex_hook_state_entries};
use crate::services::claude_tui::transcript_tail::claude_transcript_path_candidates;
use crate::services::codex_tui::rollout_index::read_rollout_session_meta;
use crate::services::session_backend::{StreamLineState, process_stream_line};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hook_payload")
        .join(name)
}

fn load(name: &str) -> Value {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|found| !found.is_empty())
        .unwrap_or_else(|| panic!("missing string `{key}` in {value}"))
}

fn runs(fixture: &Value) -> &[Value] {
    fixture["runs"].as_array().expect("runs array")
}

fn events<'a>(fixture: &'a Value, run: &str) -> &'a [Value] {
    runs(fixture)
        .iter()
        .find(|entry| entry["name"] == run)
        .and_then(|entry| entry["events"].as_array())
        .unwrap_or_else(|| panic!("missing run {run}"))
}

/// Every UserPromptSubmit shares its turn key with the next Stop.
fn assert_prompt_pairs(events: &[Value], key: &str) {
    for (index, submit) in events.iter().enumerate() {
        if submit["event"] != "UserPromptSubmit" {
            continue;
        }
        let stop = events[index..]
            .iter()
            .find(|event| event["event"] == "Stop")
            .expect("UserPromptSubmit without a later Stop");
        assert_eq!(text(&stop["payload"], key), text(&submit["payload"], key));
    }
}

#[test]
fn claude_hook_payloads_name_the_transcript_agentdesk_derives_for_the_session() {
    let fixture = load("claude-2.1.283.json");
    // --settings hooks fired without a workspace trust prompt on the capture host.
    assert_eq!(fixture["activation"]["settings_flag_hooks_fire"], true);
    assert_eq!(fixture["activation"]["trust_dialog_shown"], false);
    let mut checked = 0;
    for run in runs(&fixture) {
        for event in run["events"].as_array().expect("events") {
            let payload = &event["payload"];
            let session_id = text(payload, "session_id");
            let transcript = PathBuf::from(text(payload, "transcript_path"));
            // Layout is <claude home>/projects/<encoded cwd>/<session>.jsonl.
            let claude_home = transcript.ancestors().nth(3).expect("claude home");
            let candidates = claude_transcript_path_candidates(
                Path::new(text(payload, "cwd")),
                session_id,
                Some(claude_home),
            )
            .expect("candidates");
            assert!(
                candidates.contains(&transcript),
                "{} is not among {candidates:?}",
                transcript.display()
            );
            assert_eq!(event["env_session_id"], payload["session_id"]);
            if event["event"] == "SubagentStop" {
                // Child events keep the parent session and path; the child file is separate.
                let parent_stem = transcript.with_extension("");
                let child = PathBuf::from(text(payload, "agent_transcript_path"));
                assert!(
                    child.starts_with(parent_stem.join("subagents")),
                    "{child:?}"
                );
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 24, "captured Claude hook events");
}

#[test]
fn claude_clear_rotates_the_payload_session_before_its_transcript_exists() {
    let fixture = load("claude-2.1.283.json");
    let tui = events(&fixture, "tui_startup_clear_compact_exit");
    let launch_id = text(&tui[0], "command_session_id");
    assert!(
        tui.iter()
            .all(|event| event["command_session_id"] == launch_id)
    );

    let end = tui
        .iter()
        .position(|event| event["event"] == "SessionEnd" && event["payload"]["reason"] == "clear")
        .expect("SessionEnd(clear)");
    assert_eq!(text(&tui[end]["payload"], "session_id"), launch_id);
    let start = &tui[end + 1];
    assert_eq!(start["event"], "SessionStart");
    assert_eq!(start["payload"]["source"], "clear");
    let rotated = text(&start["payload"], "session_id");
    assert_ne!(rotated, launch_id);

    // startup, clear and fork announce a transcript path before the file exists.
    for run in [
        "print_startup",
        "tui_startup_clear_compact_exit",
        "print_fork",
    ] {
        for event in events(&fixture, run).iter().filter(|event| {
            event["event"] == "SessionStart"
                && matches!(
                    event["payload"]["source"].as_str(),
                    Some("startup" | "clear" | "fork")
                )
        }) {
            assert_eq!(event["transcript_exists_at_hook"], false, "{event}");
        }
    }
    for event in tui.iter().skip(end + 1).filter(|event| {
        matches!(event["event"].as_str(), Some("PreCompact" | "PostCompact"))
            || event["payload"]["source"] == "compact"
    }) {
        assert_eq!(text(&event["payload"], "session_id"), rotated);
    }
    assert_prompt_pairs(tui, "prompt_id");

    let first = events(&fixture, "print_startup");
    let resumed = events(&fixture, "print_resume");
    assert_eq!(resumed[0]["payload"]["source"], "resume");
    assert_eq!(
        resumed[0]["payload"]["session_id"],
        first[0]["payload"]["session_id"]
    );
    assert_eq!(
        resumed[0]["payload"]["transcript_path"],
        first[0]["payload"]["transcript_path"]
    );

    // A fork switches to a new session under the same hook command, naming no parent.
    let fork = events(&fixture, "print_fork");
    assert_eq!(fork[0]["payload"]["source"], "fork");
    assert_eq!(
        fork[0]["command_session_id"],
        first[0]["payload"]["session_id"]
    );
    assert_ne!(
        fork[0]["payload"]["session_id"],
        first[0]["payload"]["session_id"]
    );
    assert!(fork.iter().all(|event| {
        event["payload"]
            .as_object()
            .is_some_and(|payload| !payload.keys().any(|key| key.contains("parent")))
    }));
}

#[test]
fn codex_hook_payloads_name_the_rollout_whose_session_meta_matches() {
    let fixture = load("codex-0.157.1.json");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut checked = 0;
    for (run_index, run) in runs(&fixture).iter().enumerate() {
        let expected_source = if run["mode"] == "exec" { "exec" } else { "cli" };
        let mut meta_ids = Vec::new();
        for (index, record) in run["rollout_session_meta"]
            .as_array()
            .expect("meta")
            .iter()
            .enumerate()
        {
            let path = dir
                .path()
                .join(format!("rollout-{run_index}-{index}.jsonl"));
            std::fs::write(&path, format!("{record}\n")).expect("write rollout header");
            let meta = read_rollout_session_meta(&path).expect("session_meta parses");
            assert_eq!(
                meta.source.as_ref().and_then(Value::as_str),
                Some(expected_source)
            );
            meta_ids.push(meta.id.expect("session_meta.id"));
        }
        let run_events = run["events"].as_array().expect("events");
        for event in run_events {
            let payload = &event["payload"];
            let session_id = text(payload, "session_id");
            let transcript = PathBuf::from(text(payload, "transcript_path"));
            let name = transcript
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name");
            assert!(name.starts_with("rollout-"), "{name}");
            assert!(name.ends_with(&format!("-{session_id}.jsonl")), "{name}");
            assert_eq!(event["transcript_exists_at_hook"], true, "{event}");
            assert!(
                meta_ids.iter().any(|id| id == session_id),
                "{session_id} vs {meta_ids:?}"
            );
            checked += 1;
        }
        assert_prompt_pairs(run_events, "turn_id");
    }
    assert_eq!(checked, 9, "captured Codex hook events");

    let tui = events(&fixture, "tui_bypass_startup_clear");
    let sources: Vec<_> = tui
        .iter()
        .filter(|event| event["event"] == "SessionStart")
        .map(|event| {
            (
                text(&event["payload"], "source"),
                text(&event["payload"], "session_id"),
            )
        })
        .collect();
    assert_eq!(sources.len(), 2);
    assert_eq!((sources[0].0, sources[1].0), ("startup", "clear"));
    assert_ne!(sources[0].1, sources[1].1);
}

#[test]
fn codex_session_flag_trust_hashes_alone_did_not_activate_hooks_on_the_captured_cli() {
    let fixture = load("codex-0.157.1.json");
    let activation = &fixture["activation"];
    let recorded = &activation["agentdesk_hook_config"];
    let config = HookBundleConfig {
        endpoint: text(recorded, "endpoint").to_string(),
        provider: text(recorded, "provider").to_string(),
        session_id: text(recorded, "session_id").to_string(),
        agentdesk_exe: text(recorded, "agentdesk_exe").to_string(),
    };
    let computed: Vec<(String, String)> = codex_hook_state_entries(&config)
        .into_iter()
        .map(|entry| (entry.state_key, entry.trusted_hash))
        .collect();
    let trials = activation["trials"].as_array().expect("trials");
    let observed: Vec<_> = trials
        .iter()
        .map(|trial| {
            (
                text(trial, "name"),
                text(trial, "mode"),
                text(trial, "state"),
                trial["bypass_hook_trust"].as_bool().expect("bypass flag"),
                trial["agentdesk_hooks_fired"]
                    .as_bool()
                    .expect("fired flag"),
            )
        })
        .collect();
    // (name, mode, trust state, bypass flag, AgentDesk hooks fired) per captured trial.
    assert_eq!(
        observed,
        [
            (
                "session_flag_trust_hashes",
                "exec",
                "rendered",
                false,
                false
            ),
            ("bypass_hook_trust", "exec", "omitted", true, true),
            (
                "bypass_hook_trust_tui",
                "interactive",
                "rendered",
                true,
                true
            ),
        ]
    );
    for (_, mode, _, _, fired) in &observed {
        let captured_events = runs(&fixture)
            .iter()
            .filter(|run| run["mode"] == *mode)
            .any(|run| {
                run["events"]
                    .as_array()
                    .is_some_and(|events| !events.is_empty())
            });
        assert!(!fired || captured_events, "no captured events for {mode}");
    }
    let hashed = &trials[0];
    let captured: Vec<(String, String)> = hashed["trusted_hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|entry| {
            (
                text(entry, "state_key").to_string(),
                text(entry, "trusted_hash").to_string(),
            )
        })
        .collect();
    // The negative trial is only evidence if it carried AgentDesk's exact hashes.
    assert_eq!(computed, captured);
    assert_eq!(activation["resume_help_advertises_bypass"], true);
}

#[test]
fn claude_tool_results_and_synthetic_errors_carry_no_block_key() {
    let path = fixture_path("claude-2.1.283.transcript-shapes.jsonl");
    let body = std::fs::read_to_string(&path).expect("read shapes");
    let shapes: Vec<Value> = body
        .lines()
        .map(|line| serde_json::from_str(line).expect("shape line"))
        .collect();
    let mut tool_use_blocks = Vec::new();
    let mut results = 0;
    let mut errors = 0;
    for shape in &shapes {
        let record = &shape["record"];
        let kind = text(shape, "shape");
        match kind {
            "assistant_tool_use" => {
                let index = record["apiBlockIndex"].as_u64().expect("apiBlockIndex");
                tool_use_blocks.push((text(&record["message"], "id").to_string(), index));
            }
            "assistant_synthetic_api_error" => {
                assert_eq!(record["isApiErrorMessage"], true);
                assert_eq!(record["message"]["model"], "<synthetic>");
                assert!(record.get("apiBlockIndex").is_none());
                errors += 1;
            }
            _ if kind.starts_with("user_tool_result") => {
                assert!(record["message"].get("id").is_none(), "{kind}");
                assert!(record.get("apiBlockIndex").is_none(), "{kind}");
                let blocks = record["message"]["content"].as_array().expect("content");
                assert!(blocks.iter().all(|block| block["type"] == "tool_result"));
                if shape["provenance"] == "derived" {
                    assert!(blocks.len() > 1, "multi-result shape");
                    continue;
                }
                assert_eq!(blocks.len(), 1, "2.1.283 writes one result per row");
                let (sender, receiver) = mpsc::channel();
                let mut state = StreamLineState::new();
                assert!(process_stream_line(
                    &record.to_string(),
                    &sender,
                    &mut state
                ));
                drop(sender);
                let parsed = receiver.iter().find_map(|message| match message {
                    StreamMessage::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => Some((tool_use_id, is_error)),
                    _ => None,
                });
                let block = &blocks[0];
                let is_error = kind == "user_tool_result_error";
                assert_eq!(block["is_error"].as_bool(), Some(is_error), "{kind}");
                let expected = (Some(text(block, "tool_use_id").to_string()), is_error);
                assert_eq!(parsed, Some(expected), "{kind}");
                results += 1;
            }
            other => panic!("unexpected shape {other}"),
        }
    }
    assert_eq!((results, errors), (3, 1));
    // Parallel tool calls stay one API message split into per-block rows.
    assert_eq!(tool_use_blocks.len(), 3);
    assert!(
        tool_use_blocks
            .iter()
            .all(|(id, _)| *id == tool_use_blocks[0].0)
    );
    let mut indexes: Vec<_> = tool_use_blocks.iter().map(|(_, index)| *index).collect();
    indexes.sort_unstable();
    indexes.dedup();
    assert_eq!(indexes.len(), 3);
}
