use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
use crate::services::discord::tui_direct_pending_start::{PendingStartState, delete, persist};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};

fn selecting(channel: u64) -> TurnConfig {
    TurnConfig {
        channels: [channel].into(),
        all_owned: Some(false),
    }
}

/// A parent Codex rollout whose first turn opened with a prompt and has not answered yet.
fn open_rollout(root: &Path, channel: u64) -> SourceBinding {
    let path = root.join(format!("rollout-{channel}.jsonl"));
    let lines = [
        json!({"type":"session_meta","payload":{"id":"parent","cwd":root,"source":"cli"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"hi"}]}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Codex,
        source: source_id_for("parent", &path).unwrap(),
    }
}

fn answer(source: &SourceBinding) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.source.path)
        .unwrap();
    for line in [
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"m1",
            "content":[{"type":"output_text","text":"done"}]}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}),
    ] {
        writeln!(file, "{line}").unwrap();
    }
}

/// Legacy's synthetic turn row for the takeover turn, before or after its terminal delivery.
fn save_row(channel: u64, terminal: bool) -> PathBuf {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        1,
        7,
        0,
        "hi".into(),
        None,
        Some("codex-takeover-fixture".into()),
        None,
        None,
        0,
    );
    row.turn_source = TurnSource::ExternalInput;
    row.set_relay_owner_kind(RelayOwnerKind::Watcher);
    row.terminal_delivery_committed = terminal;
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Codex, channel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _guard = inflight::lock_inflight_state_path(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    path
}

fn pending(channel: u64) -> TuiDirectPendingStart {
    TuiDirectPendingStart {
        hosted_source: None,
        hosted_source_checked: false,
        provider: "codex".into(),
        channel_id: channel,
        tmux_session_name: "codex-takeover-fixture".into(),
        prompt_text: "next".into(),
        anchor_message_id: 8,
        lease_relay_owner: "tmux_watcher".into(),
        lease_runtime_kind: Some("codex_tui".into()),
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
        native_turn_id: None,
    }
}

fn retry(confirmation: Confirmation) -> String {
    match confirmation {
        Confirmation::Retry(detail) => detail,
        other => panic!("expected a retry, got {other:?}"),
    }
}

/// An open, metadata-only or missing rollout keeps turn mode off and Legacy's turn row in place;
/// the boot selection alone never confirms.
#[test]
fn codex_confirmation_waits_for_the_takeover_turn_to_end_in_its_rollout() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_301;
    let source = open_rollout(dir.path(), channel);
    let row = save_row(channel, true);
    let config = selecting(channel);

    assert_eq!(
        confirm_codex_channel(channel, None, Some(&source)),
        Confirmation::Unselected
    );
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("Open"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));

    let meta_only = dir.path().join("meta-only.jsonl");
    std::fs::copy(&source.source.path, &meta_only).unwrap();
    let header = std::fs::read_to_string(&meta_only).unwrap();
    std::fs::write(&meta_only, format!("{}\n", header.lines().next().unwrap())).unwrap();
    let meta_only = SourceBinding {
        source: source_id_for("parent", &meta_only).unwrap(),
        ..source.clone()
    };
    retry(confirm_codex_channel(
        channel,
        Some(&config),
        Some(&meta_only),
    ));
    std::fs::remove_file(&meta_only.source.path).unwrap();
    retry(confirm_codex_channel(
        channel,
        Some(&config),
        Some(&meta_only),
    ));
    assert!(row.exists() && !transcript_turns(channel));

    answer(&source);
    assert_eq!(
        confirm_codex_channel(channel, Some(&config), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(!row.exists(), "the ended turn's synthetic row retires");
}

/// After the turn ended, a pending start, an unfinished row or a held start lock each refuse with
/// nothing retired; once Legacy released all three the same call confirms.
#[test]
fn codex_confirmation_retires_nothing_while_legacy_holds_an_unfinished_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_302;
    let source = open_rollout(dir.path(), channel);
    answer(&source);
    let config = selecting(channel);
    let record = pending(channel);
    persist(&record).unwrap();
    let row = save_row(channel, false);

    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("pending start"), "{detail}");
    let durable = records::<TuiDirectPendingStart>(root(), &ProviderKind::Codex, channel);
    assert_eq!(durable.unwrap().len(), 1, "the pending start is kept");
    assert!(pending_synthetic_start_present("codex", channel) && row.exists());
    delete(&record);
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("no terminal delivery"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));

    save_row(channel, true);
    let lock = channel_lock("codex", channel);
    let held = lock.try_lock().unwrap();
    let detail = retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(detail.contains("start claim"), "{detail}");
    assert!(row.exists() && !transcript_turns(channel));
    drop(held);

    assert_eq!(
        confirm_codex_channel(channel, Some(&config), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(!row.exists());
}

/// A record longer than one read budget still lets the rollout reach its turn end, so a long tool
/// output never keeps the takeover turn open.
#[test]
fn codex_confirmation_reads_past_a_record_longer_than_one_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let channel = 64_700_303;
    let source = open_rollout(dir.path(), channel);
    let output = "x".repeat(3 << 20);
    let long = json!({"type":"response_item","payload":{"type":"function_call_output",
        "call_id":"c1","output":output}});
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.source.path)
        .unwrap();
    writeln!(file, "{long}").unwrap();
    drop(file);
    answer(&source);

    assert_eq!(
        confirm_codex_channel(channel, Some(&selecting(channel)), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
}

/// Commits a late takeover init of `sources` for `channel` in the runtime O store.
#[cfg(unix)]
fn init_takeover(channel: u64, sources: &[crate::services::tui_o::shadow::SourceId]) {
    use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
    let root = crate::config::runtime_root().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let init = Initialized {
        policy_parent: None,
        channel,
        sources: (sources.iter())
            .map(|source| InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: String::new(),
            })
            .collect(),
        initial_anchor: 0,
        build_digest: "test".into(),
        at: chrono::Utc::now(),
    };
    let store = store.unwrap().unwrap();
    store
        .init_fixture_channel_as(&init, InitOrigin::LateCodexTakeover)
        .unwrap();
}

/// A takeover rollout whose dev a reboot renumbered confirms its ended turn; a new inode at that
/// path, or an init naming two stored variants of the file, keeps Legacy turns.
#[cfg(unix)]
#[test]
fn codex_confirmation_reads_a_takeover_rollout_whose_volume_was_renumbered() {
    use crate::services::tui_o::shadow::capture::renumber;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let (channel, ambiguous) = (64_700_304, 64_700_305);
    let source = open_rollout(dir.path(), channel);
    answer(&source);
    let row = save_row(channel, true);
    init_takeover(channel, std::slice::from_ref(&source.source));
    let variant = crate::services::tui_o::shadow::SourceId {
        dev: source.source.dev ^ (1 << 41),
        ..source.source.clone()
    };
    init_takeover(ambiguous, &[source.source.clone(), variant]);
    let (config, aside) = (selecting(channel), dir.path().join("aside.jsonl"));

    std::fs::rename(&source.source.path, &aside).unwrap();
    std::fs::copy(&aside, &source.source.path).unwrap();
    retry(confirm_current(channel, &config));
    assert!(row.exists() && !transcript_turns(channel), "a new inode");
    std::fs::rename(&aside, &source.source.path).unwrap();

    let _reboot = renumber::shift(&source.source.path, 1 << 40);
    retry(confirm_current(ambiguous, &selecting(ambiguous)));
    assert!(!transcript_turns(ambiguous), "two stored variants");
    assert_eq!(confirm_current(channel, &config), Confirmation::Confirmed);
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(!row.exists(), "the ended turn's synthetic row retires");
}

#[cfg(unix)]
fn archive(source: &SourceBinding) -> PathBuf {
    let mut path = source.source.path.as_os_str().to_os_string();
    path.push(".zst");
    let path = PathBuf::from(path);
    let bytes = std::fs::read(&source.source.path).unwrap();
    std::fs::write(&path, zstd::encode_all(bytes.as_slice(), 0).unwrap()).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn archived_takeover_confirms_after_restart_and_releases_its_queued_input_once() {
    use crate::config::Config;
    use crate::services::discord::queue_io::input_owner::{admission, effect, pump};
    use crate::services::tui_o::{channel_policy, cutover};
    use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};

    crate::test_support::wait::await_within(async {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 64_700_306;
    let source = open_rollout(_root.path(), channel);
    init_takeover(channel, std::slice::from_ref(&source.source));
    let config: Config = serde_json::from_value(json!({"server":{},
        "agents":[{"id":"w","name":"W","channels":{"codex":{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]},"turn":{"channels":[channel]}}})).unwrap();
    let boot = channel_policy::BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(&config, |channels, _| {
        Ok(channel_policy::stored(Some(_root.path()), channels))
    }).unwrap();
    let _boot = cutover::test_override::force_boot(boot);
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let queue_dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap().join("codex/input-owner-takeover");
    std::fs::create_dir_all(&queue_dir).unwrap();
    mailbox.v2_adopt(queue_dir.join(format!("{channel}.json")), "takeover".into()).await.unwrap();
    let item: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":8_000_000_000_081_950_001_u64,"channel_id":channel,
        "source_message_ids":[8_000_000_000_081_950_001_u64],"queued_generation":1,
        "text":"waiting input"})).unwrap();
    let admitted = admission::admit(&mailbox, &item, admission::AdmissionTicket::new()).await;
    let admission::Admission::Accepted(ticket) = admitted else { panic!("{admitted:?}"); };
    let snapshot = || mailbox.v2_request(|slot| slot.as_ref().unwrap().json());
    retry(confirm_current(channel, &selecting(channel)));
    assert!(cutover::codex_turn_awaited(channel).is_some());
    assert!(matches!(cutover::intake_route::route("codex", channel), cutover::intake_route::IntakeRoute::Hold(_)));
    assert_eq!(snapshot().await.unwrap()["items"].as_array().unwrap().len(), 1);

    answer(&source);
    archive(&source);
    std::fs::remove_file(&source.source.path).unwrap();
    assert_eq!(confirm_current(channel, &selecting(channel)), Confirmation::Confirmed,
        "TURN_MODE_AWAITED must clear after compression");
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(cutover::codex_turn_awaited(channel).is_none());
    let prepared = effect::PreparedPayload {
        frame: item.text.clone(), sources: vec![],
        attempt: json!({"nonce":"takeover-send","provider":"codex",
            "target":{"host":"herdr","owner":"takeover-fixture","root":"/fixture","execution_nonce":"takeover-send"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent = seen.clone();
    let result = pump::submit(&mailbox, prepared, std::sync::Arc::new(|| 10_000), Box::new(move |frame| {
        sent.lock().unwrap().push(frame.to_owned());
        Ok(effect::Completion::Submitted)
    })).await.unwrap();
    assert_eq!(result, effect::Completion::Submitted);
    assert_eq!(*seen.lock().unwrap(), ["waiting input"]);
    assert!(snapshot().await.unwrap()["items"].as_array().unwrap().is_empty());
    assert!(snapshot().await.unwrap()["head_injection"].is_null());
    assert!(matches!(admission::exact_turn(&mailbox, &ticket, &item).await.unwrap(), admission::TurnBind::Settled(_)));
    assert_eq!(confirm_current(channel, &selecting(channel)), Confirmation::Confirmed);
    assert_eq!(*seen.lock().unwrap(), ["waiting input"]);
    }, crate::test_support::wait::WAIT_CAP).await;
}

#[cfg(unix)]
#[test]
fn an_archive_does_not_end_a_present_rollout_or_bypass_unfinished_legacy() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 64_700_307;
    let source = open_rollout(_root.path(), channel);
    archive(&source);
    let config = selecting(channel);
    retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(!transcript_turns(channel), "the original still runs");
    std::fs::remove_file(&source.source.path).unwrap();
    let row = save_row(channel, false);
    retry(confirm_codex_channel(channel, Some(&config), Some(&source)));
    assert!(row.exists() && !transcript_turns(channel));
    save_row(channel, true);
    assert_eq!(
        confirm_codex_channel(channel, Some(&config), Some(&source)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
}

#[cfg(unix)]
#[test]
fn claude_ended_still_refuses_an_archived_missing_transcript() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut source = open_rollout(_root.path(), 64_700_308);
    archive(&source);
    std::fs::remove_file(&source.source.path).unwrap();
    source.provider = ShadowProvider::Claude;
    assert!(ended(&source).is_err());
}

#[cfg(unix)]
fn binding_log(root: &Path, channel: u64, sources: &[(&SourceBinding, &str)]) -> PathBuf {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let mut text = Vec::new();
    for (index, (source, nonce)) in sources.iter().enumerate() {
        let event = p5::BindingEvent {
            seq: index as u64 + 1,
            channel_id: channel,
            provider: "codex".into(),
            tmux_session: "takeover-fixture".into(),
            execution_nonce: Some((*nonce).into()),
            old: index
                .checked_sub(1)
                .map(|previous| sources[previous].0.source.clone()),
            new: p5::BindingTarget::Source(source.source.clone()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        text.extend(serde_json::to_vec(&event).unwrap());
        text.push(b'\n');
    }
    let dir = root.join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.log"));
    std::fs::write(&path, text).unwrap();
    p5::set_test_root(Some(root));
    path
}

#[cfg(unix)]
#[test]
fn a_proven_later_binding_ends_the_init_takeover_without_opening_its_missing_rollout() {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 64_700_309;
    let source = open_rollout(_root.path(), channel);
    init_takeover(channel, std::slice::from_ref(&source.source));
    let mut next = open_rollout(_root.path(), channel + 1);
    next.source.session_id = "successor".into();
    let text = std::fs::read_to_string(&next.source.path)
        .unwrap()
        .replace("\"parent\"", "\"successor\"");
    std::fs::write(&next.source.path, text).unwrap();
    binding_log(_root.path(), channel, &[(&source, "n1"), (&next, "n1")]);
    std::fs::remove_file(&source.source.path).unwrap();
    assert_eq!(
        confirm_current(channel, &selecting(channel)),
        Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(channel);
    assert!(transcript_turns(channel));
    p5::set_test_root(None);
}

#[cfg(unix)]
#[test]
fn ambiguous_or_unreadable_binding_history_does_not_end_an_open_takeover() {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 64_700_311;
    let source = open_rollout(_root.path(), channel);
    init_takeover(channel, std::slice::from_ref(&source.source));
    let mut next = open_rollout(_root.path(), channel + 1);
    next.source.session_id = "successor".into();
    let config = selecting(channel);
    binding_log(_root.path(), channel, &[(&source, "n1")]);
    retry(confirm_current(channel, &config));
    let log = binding_log(
        _root.path(),
        channel,
        &[(&source, "n1"), (&source, "n2"), (&next, "n1")],
    );
    retry(confirm_current(channel, &config));
    std::fs::write(&log, b"not-json\n").unwrap();
    retry(confirm_current(channel, &config));
    assert!(!transcript_turns(channel));
    p5::set_test_root(None);
}

#[cfg(unix)]
#[test]
fn an_archive_symlink_or_directory_is_not_a_finished_takeover() {
    use std::os::unix::fs::symlink;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 64_700_313;
    let source = open_rollout(_root.path(), channel);
    let archive = archive(&source);
    let target = _root.path().join("archive-target");
    std::fs::rename(&archive, &target).unwrap();
    std::fs::remove_file(&source.source.path).unwrap();
    symlink(&target, &archive).unwrap();
    retry(confirm_codex_channel(
        channel,
        Some(&selecting(channel)),
        Some(&source),
    ));
    std::fs::remove_file(&archive).unwrap();
    std::fs::create_dir(&archive).unwrap();
    retry(confirm_codex_channel(
        channel,
        Some(&selecting(channel)),
        Some(&source),
    ));
    assert!(!transcript_turns(channel));
}
