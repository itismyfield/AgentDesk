use std::path::Path;

use serde_json::json;

use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::clear_end::{self, ClearEnd};
use crate::services::tui_o::store::{InitSource, Initialized, StoreConfig};
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};
use crate::services::tui_prompt_dedupe::binding_events as p5;

const SESSION: &str = "c1a0de00-0000-4000-8000-000000007619";

fn selecting(channel: u64) -> TurnConfig {
    TurnConfig {
        channels: [channel].into(),
        all_owned: Some(false),
    }
}

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

/// A transcript through its first prompt, with no output: what the takeover committed.
fn prompted(root: &Path, channel: u64) -> SourceBinding {
    let path = root.join(format!("claude-{channel}.jsonl"));
    let text = [
        line(json!({"type": "mode", "mode": "normal", "sessionId": SESSION})),
        line(json!({"type": "user", "sessionId": SESSION, "uuid": "u1",
            "message": {"role": "user", "content": "hi"}})),
    ]
    .concat();
    std::fs::write(&path, text).unwrap();
    SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Claude,
        source: source_id_for(SESSION, &path).unwrap(),
    }
}

/// The store's init as a Claude takeover writes it, over `source` from its first byte.
fn taken(root: &Path, channel: u64, source: &SourceId) {
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root);
    let init = Initialized {
        channel,
        sources: vec![InitSource {
            source_id: source.clone(),
            delivery_start: 0,
            prefix_hash: String::new(),
        }],
        initial_anchor: 0,
        build_digest: "test".into(),
        at: chrono::Utc::now(),
    };
    let origin = InitOrigin::LateClaudeTakeover;
    store
        .unwrap()
        .unwrap()
        .init_channel_as(&init, origin)
        .unwrap();
}

/// Binds `source` once per nonce, in order, as the hook logs it.
fn bound(root: &Path, channel: u64, source: &SourceId, nonces: &[&str]) {
    let mut text = Vec::new();
    for (seq, nonce) in (1..).zip(nonces) {
        let event = p5::BindingEvent {
            seq,
            channel_id: channel,
            provider: "claude".into(),
            tmux_session: "tmux-claude".into(),
            execution_nonce: Some((*nonce).into()),
            old: None,
            new: p5::BindingTarget::Source(source.clone()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: None,
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        text.extend(serde_json::to_vec(&event).unwrap());
        text.push(b'\n');
    }
    let dir = root.join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{channel}.log")), text).unwrap();
    p5::set_test_root(Some(root));
}

fn clear_of(root: &Path, channel: u64, source: &SourceId, nonce: &str) {
    let end = ClearEnd {
        channel,
        provider: "claude".into(),
        tmux_session: "tmux-claude".into(),
        execution_nonce: nonce.into(),
        source: source.clone(),
        binding_seq: 1,
        host: None,
        ended_at: chrono::Utc::now(),
        basis: "confirmed_clear".into(),
    };
    clear_end::append(root, end).unwrap();
}

fn retry(confirmation: Confirmation) -> String {
    match confirmation {
        Confirmation::Retry(detail) => detail,
        other => panic!("expected a retry, got {other:?}"),
    }
}

/// A resume binds the taken transcript again under another nonce: a clear of that later execution
/// does not end the taken one, a clear of the taken nonce does.
#[test]
fn claude_takeover_confirmation_follows_the_taken_nonce_through_a_resume() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let root = root.as_path();
    let channel = 64_761_801;
    let takeover = prompted(root, channel);
    taken(root, channel, &takeover.source);
    bound(root, channel, &takeover.source, &["n-taken", "n-resumed"]);
    let config = selecting(channel);
    let none = |_: &str| false;

    let detail = retry(confirm_takeover(channel, &config, &takeover, none));
    assert!(detail.contains("not ended"), "{detail}");
    clear_of(root, channel, &takeover.source, "n-resumed");
    retry(confirm_takeover(channel, &config, &takeover, none));
    assert!(!transcript_turns(channel));

    clear_of(root, channel, &takeover.source, "n-taken");
    let confirmed = confirm_takeover(channel, &config, &takeover, none);
    assert_eq!(confirmed, Confirmation::Confirmed);
    drop(TestConfirmation::confirmed(channel));
}

/// A promptless takeover whose Herdr execution was terminated never reads Idle; the Herdr end of
/// the taken nonce confirms it, another execution's does not.
#[test]
fn claude_promptless_takeover_confirms_on_a_herdr_end_without_idle() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let root = root.as_path();
    let channel = 64_761_802;
    let takeover = prompted(root, channel);
    taken(root, channel, &takeover.source);
    bound(root, channel, &takeover.source, &["n-taken"]);
    let config = selecting(channel);

    retry(confirm_takeover(channel, &config, &takeover, |nonce| {
        nonce == "n-other"
    }));
    assert!(!transcript_turns(channel));
    let confirmed = confirm_takeover(channel, &config, &takeover, |nonce| nonce == "n-taken");
    assert_eq!(confirmed, Confirmation::Confirmed);
    drop(TestConfirmation::confirmed(channel));
}

/// A takeover committed before a restart is not confirmed at boot, where its turn may still run;
/// the task boot started confirms it once the taken turn ends, with no adoption in this process.
#[tokio::test(start_paused = true)]
async fn claude_takeover_turn_mode_waits_at_boot_and_its_task_confirms_after_the_turn() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let root = root.as_path();
    let channel = 64_761_803;
    let takeover = prompted(root, channel);
    taken(root, channel, &takeover.source);
    bound(root, channel, &takeover.source, &["n-taken"]);
    let config: crate::config::Config = serde_json::from_value(json!({"server": {},
        "agents": [{"id": "c", "name": "C",
            "channels": {"claude": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel]}, "turn": {"channels": [channel]}}}))
    .unwrap();
    let boot = crate::services::tui_o::channel_policy::BootChannels::validate(&config).unwrap();
    let committed = crate::services::tui_o::channel_policy::Adoption::Committed;
    let _boot = crate::services::tui_o::cutover::test_override::force_boot(boot.adopted(committed));

    let confirmed = super::super::confirm_at_boot(&ProviderKind::Claude, config.tui_o.as_ref());
    assert!(
        confirmed.is_empty(),
        "boot leaves a takeover's turn to Legacy"
    );
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert!(!transcript_turns(channel));

    let end = [
        line(
            json!({"type": "assistant", "sessionId": SESSION, "uuid": "a1", "apiBlockIndex": 0,
            "message": {"id": "m1", "role": "assistant",
            "content": [{"type": "text", "text": "done"}]}}),
        ),
        line(
            json!({"type": "system", "subtype": "turn_duration", "durationMs": 5,
            "sessionId": SESSION}),
        ),
    ]
    .concat();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&takeover.source.path);
    std::io::Write::write_all(file.as_mut().unwrap(), end.as_bytes()).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert!(
        transcript_turns(channel),
        "the boot task confirmed after the turn"
    );
    drop(TestConfirmation::confirmed(channel));
}
