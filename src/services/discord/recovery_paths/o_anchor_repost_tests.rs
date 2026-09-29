//! The committed-anchor repost follows the anchor's real Discord channel: an O-owned channel is
//! never reposted by Legacy, and an unknown selected kind preserves the row for retry.

use super::restart::{AnchorRepostOutcome, try_recover_anchor_repost};
use super::shared::RecoveryRelayOutcome;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::recovery_engine::o_cut_recorder::start_with;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::test_override::force_channels;
use poise::serenity_prelude::ChannelId;

const CHILD: &str = "ADK_O_ANCHOR_REPOST_CHILD";
const BODY: &str = "ADK-O-anchor-repost-body";

/// Runs the named test in a child whose anchor-repost flag and runtime root are its own.
fn isolated(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("AGENTDESK_RECOVERY_ANCHOR_REPOST", "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("TMPDIR", root.path())
        .env_remove(crate::services::tui_o::cutover::test_override::CHILD_ENV)
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

/// A committed row in `destination` whose delivery record, keyed by `owner`, anchors a
/// message in `destination` that the recorder reports as deleted.
fn committed_row(
    destination: u64,
    owner: u64,
    kind: Option<RuntimeHandoffKind>,
) -> InflightTurnState {
    let provider = ProviderKind::Codex;
    let tmux = format!("AgentDesk-codex-o-anchor-{destination}");
    let root = crate::config::runtime_root().unwrap();
    let output = root.join(format!("o-anchor-{destination}.jsonl"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&output, [b'x'; 200]).unwrap();
    let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
    std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
    std::fs::write(&generation, "g").unwrap();
    let generation_mtime_ns = crate::services::discord::tmux::read_generation_file_mtime_ns(&tmux);
    assert_ne!(generation_mtime_ns, 0);
    let mut state = InflightTurnState::new(
        provider.clone(),
        destination,
        None,
        7,
        destination + 7,
        destination + 8,
        "prompt".to_string(),
        None,
        Some(tmux.clone()),
        Some(output.display().to_string()),
        None,
        150,
    );
    state.turn_start_offset = Some(0);
    state.watcher_owner_channel_id = Some(owner);
    state.runtime_kind = kind;
    inflight::save_inflight_state(&state).unwrap();
    delivery_record::record_watcher_owner_channel_context(
        &provider,
        ChannelId::new(destination),
        ChannelId::new(owner),
        &tmux,
    )
    .unwrap();
    delivery_record::write_delivered_frontier(
        &provider,
        owner,
        &tmux,
        delivery_record::DeliveredCommit {
            range: (0, 200),
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: Some(destination + 9),
            panel_channel_id: Some(destination),
        },
    )
    .unwrap();
    state
}

#[tokio::test(flavor = "current_thread")]
async fn o_delegated_anchor_repost_is_skipped() {
    if !isolated("o_delegated_anchor_repost_is_skipped") {
        return;
    }
    let shared = crate::services::discord::make_shared_data_for_tests();
    let codex = Some(RuntimeHandoffKind::CodexTui);
    // (case, destination, listed channel, recorded kind, expected outcome, Legacy reposts)
    let cases = [
        (
            "listed_destination",
            9_435_110u64,
            9_435_110u64,
            codex,
            AnchorRepostOutcome::NotReposted,
            false,
        ),
        (
            "listed_owner_only",
            9_435_120,
            9_435_121,
            codex,
            AnchorRepostOutcome::Relayed(RecoveryRelayOutcome::Delivered),
            true,
        ),
        (
            "listed_unknown_kind",
            9_435_130,
            9_435_130,
            None,
            AnchorRepostOutcome::RefusedPreserveRow,
            false,
        ),
    ];
    for (name, destination, listed, kind, expected, reposts) in cases {
        let owner = destination + 1;
        let state = committed_row(destination, owner, kind);
        let recorder = start_with(destination, true).await;
        let _o = force_channels(&[(listed, RuntimeHandoffKind::CodexTui)]);
        let outcome =
            try_recover_anchor_repost(&recorder.http, &shared, &ProviderKind::Codex, &state, BODY)
                .await;
        assert_eq!(outcome, expected, "{name}");
        let posted = recorder
            .contents()
            .iter()
            .any(|content| content.contains(BODY));
        assert_eq!(posted, reposts, "{name}");
        let row = inflight::load_inflight_state_read_only(&ProviderKind::Codex, destination)
            .expect("the committed row stays for the caller's disposition");
        assert_eq!(
            row.anchor_repost_attempts,
            u32::from(reposts),
            "{name}: only a Legacy repost spends an attempt"
        );
    }
}
