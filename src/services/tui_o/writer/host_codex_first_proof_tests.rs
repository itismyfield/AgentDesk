use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::channel_policy::first_prompt_digest;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};
use crate::services::tui_prompt_dedupe::{
    self as dedupe, TuiRuntimeBinding, binding_context::PreparedIncarnation,
};
use crate::test_support::wait::{WAIT_CAP, await_within};
use std::io::Write;

async fn scenario(boundary: Option<Step>, invalidator: &str) {
    dedupe::reset_state_for_tests();
    let Takeover {
        harness,
        io,
        tasks,
        _reserved: reserved,
    } = hosted_over_old(8_165_201).await;
    let channel = harness.channel_id;
    let candidate =
        test_override::with_channels(|boot| boot.unwrap().candidate(channel).unwrap().clone());
    let token = Arc::new(CancelToken::new());
    let admission = candidate
        .admit_first(
            channel,
            token.turn_nonce().unwrap(),
            "hi",
            "tmux-codex",
            || {
                ChannelBindingLog::new(channel, ShadowProvider::Codex)
                    .binding_events_since(channel, 0)
                    .map(|events| events.last().map_or(0, |e| e.seq))
            },
        )
        .unwrap();
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        "tmux-codex",
        Some(channel),
        None,
        false,
        Some(harness._runtime.path().canonicalize().unwrap()),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    candidate.attach_first_execution(&prepared.context, "hi", token.turn_nonce());
    let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
        "tmux-codex",
        "spawn_nonce",
    ));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, &prepared.context.execution_nonce).unwrap();
    let mut bytes = prompted("s-codex");
    if invalidator == "wrong-prompt" {
        bytes = String::from_utf8(bytes)
            .unwrap()
            .replace("hi", "different")
            .into_bytes();
    }
    let (path, source) = rollout(&harness, "proved-first.jsonl", &bytes);
    let mutate = {
        let candidate = candidate.clone();
        let token = token.clone();
        let marker = marker.clone();
        let invalidator = invalidator.to_owned();
        move || {
            match invalidator.as_str() {
                "send" => {
                    drop(candidate.claim_body(channel));
                }
                "cancel" | "stop" => token.publish_cancel("stop at takeover boundary"),
                "clear" => cutover::invalidate_codex_first(Some(channel), None, None),
                "terminate" => candidate.invalidate_first(None, Some("tmux-codex")),
                "nonce" => std::fs::write(marker, "successor").unwrap(),
                "evict" => {
                    assert!(dedupe::evict_dead_tmux_mirror("tmux-codex"));
                }
                "valid" | "wrong-prompt" | "output" => {}
                _ => unreachable!(),
            }
            Ok(())
        }
    };
    if invalidator == "output" {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&answer("first-output", "short answer"))
            .unwrap();
    }
    let mut body = Box::pin(claim_then_send(
        Some(BodyClaim::new(channel, Some(CodexTui)).for_turn(token.turn_nonce())),
        || async { 1 },
    ));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(body.as_mut(), &mut context).is_pending(),
        "the body is retained before any Legacy claim"
    );
    if let Some(boundary) = boundary {
        test_hook::set(channel, boundary, mutate);
    }
    dedupe::register_tmux_channel("tmux-codex", channel);
    if invalidator == "missing-bind-nonce" {
        std::fs::remove_file(&marker).unwrap();
    } else if invalidator == "old-bind-nonce" {
        std::fs::write(&marker, "previous-execution").unwrap();
    }
    dedupe::register_tmux_runtime_binding(
        "tmux-codex",
        TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("s-codex".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    if matches!(invalidator, "missing-bind-nonce" | "old-bind-nonce") {
        std::fs::write(&marker, &prepared.context.execution_nonce).unwrap();
    }
    if invalidator == "evict" && boundary.is_none() {
        dedupe::evict_dead_tmux_mirror("tmux-codex");
    }
    let sent = await_within(body, WAIT_CAP).await.unwrap();
    if invalidator == "valid" {
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        assert_eq!(init_sources(&harness, channel), [(source, 0)]);
        assert!(matches!(sent, BodySend::OwnedByO));
    } else {
        assert_ne!(adoption(channel), Adoption::Committed, "{invalidator}");
        assert!(
            harness.store.read_init(channel).unwrap().is_none(),
            "{invalidator}"
        );
        assert!(
            matches!(sent, BodySend::Sent(1)),
            "{invalidator}: Legacy must resume exactly once"
        );
    }
    drop(admission);
    drop(tasks);
    drop(reserved);
    dedupe::reset_state_for_tests();
}

#[test]
fn proved_first_prefix_commits_and_releases_legacy_without_a_body_send() {
    serial_scenario(None, "valid");
}

#[test]
fn output_free_policy_refuses_output_and_wrong_admitted_prompt_without_losing_legacy() {
    for invalid in ["output", "wrong-prompt"] {
        serial_scenario(None, invalid);
    }
}

#[test]
fn invalidation_at_each_fence_boundary_prevents_a_stale_first_init() {
    for step in [Step::BeforeFence, Step::AfterFence, Step::BeforeLock] {
        for invalid in [
            "send",
            "cancel",
            "stop",
            "clear",
            "terminate",
            "nonce",
            "evict",
        ] {
            serial_scenario(Some(step), invalid);
        }
    }
}

#[test]
fn a_binding_must_name_the_admitted_execution_not_a_legacy_or_old_one() {
    for invalid in ["missing-bind-nonce", "old-bind-nonce"] {
        serial_scenario(None, invalid);
    }
}

fn serial_scenario(boundary: Option<Step>, invalidator: &str) {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    dedupe::run_serial_test(async {
        tokio::time::pause();
        scenario(boundary, invalidator).await;
    });
    drop(root_guard);
}
