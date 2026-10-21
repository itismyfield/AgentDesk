//! Every writer that reloads a row of the same identity must still refuse a row born to another
//! turn nonce: the four identity fields match, only `turn_nonce` tells the episodes apart.

use super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::discord::inflight::{
    InflightDeliveryRewindReason, StreamRelayAuthority, bind_recovery_anchor_for_snapshot,
    bind_recovery_anchor_if_matches_identity, load_inflight_state,
    patch_bridge_entry_state_if_identity_unchanged,
    patch_bridge_entry_state_tracking_placeholder_clear,
    recovery_anchor_message_if_matches_identity, save_inflight_delivery_rewind_if_matches_identity,
    save_inflight_state, save_inflight_state_if_identity_matches_allow_output_restamp,
    save_stream_tick_state_if_bridge_authority,
    save_stream_tick_state_preserving_current_message_races,
    stamp_runtime_handoff_if_matches_identity,
};

const CHANNEL: u64 = 7_219_001;
const CALLER: &str = "test::nonce_collision_7219";

/// The actor's local, as it stood when it last read the durable row: identical to that row
/// except for the nonce the actor itself was started with.
struct Episode {
    durable: InflightTurnState,
    local: InflightTurnState,
}

fn seed(actor_nonce: Option<&str>, durable_nonce: Option<&str>, shape: Shape) -> Episode {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        CHANNEL,
        Some("adk-7219".into()),
        343_742_347_365_974_026,
        7_219_101,
        0,
        "same identity, another episode".into(),
        Some("session-7219".into()),
        Some("AgentDesk-codex-7219".into()),
        Some("/tmp/agentdesk-7219.jsonl".into()),
        None,
        64,
    );
    // The four `InflightTurnIdentity` fields, pinned so both episodes share them byte for byte.
    row.user_msg_id = 7_219_101;
    row.started_at = "2026-10-21 09:00:00".into();
    row.tmux_session_name = Some("AgentDesk-codex-7219".into());
    row.turn_start_offset = Some(64);
    row.turn_nonce = durable_nonce.map(str::to_owned);
    row.full_response = "delivered".into();
    row.response_sent_offset = row.full_response.len();
    match shape {
        Shape::Plain => {}
        Shape::WatcherOwned => row.set_relay_owner_kind(RelayOwnerKind::Watcher),
        Shape::Anchored => {
            row.current_msg_id = 7_219_404;
            row.current_msg_len = 9;
        }
        Shape::Restarting => row.restart_mode = Some(InflightRestartMode::DrainRestart),
        Shape::PlaceholderOpen => row.long_running_placeholder_active = true,
    }
    save_inflight_state(&row).expect("seed durable episode");
    let durable = load_inflight_state(&ProviderKind::Codex, CHANNEL).expect("seeded row");
    let mut local = durable.clone();
    local.turn_nonce = actor_nonce.map(str::to_owned);
    Episode { durable, local }
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    Plain,
    WatcherOwned,
    Anchored,
    Restarting,
    PlaceholderOpen,
}

#[derive(Clone, Copy, Debug)]
enum Writer {
    StreamMerge,
    StreamMergeDivergentBody,
    StrictFence,
    StrictFenceAuthorityChanged,
    StrictSelfHandoff,
    BridgeEntry,
    BridgeEntryPlaceholderClear,
    RuntimeStamp,
    AnchorReuse,
    AnchorBind,
    DeliveryRewind,
    IdentityRefresh,
    RebindAdoption,
    RestartPatch,
}

impl Writer {
    const ALL: [Writer; 14] = [
        Writer::StreamMerge,
        Writer::StreamMergeDivergentBody,
        Writer::StrictFence,
        Writer::StrictFenceAuthorityChanged,
        Writer::StrictSelfHandoff,
        Writer::BridgeEntry,
        Writer::BridgeEntryPlaceholderClear,
        Writer::RuntimeStamp,
        Writer::AnchorReuse,
        Writer::AnchorBind,
        Writer::DeliveryRewind,
        Writer::IdentityRefresh,
        Writer::RebindAdoption,
        Writer::RestartPatch,
    ];

    fn shape(self) -> Shape {
        match self {
            Writer::StrictFenceAuthorityChanged | Writer::StrictSelfHandoff => Shape::WatcherOwned,
            Writer::AnchorReuse => Shape::Anchored,
            Writer::RestartPatch => Shape::Restarting,
            Writer::BridgeEntryPlaceholderClear => Shape::PlaceholderOpen,
            _ => Shape::Plain,
        }
    }

    /// Runs the production entry point with the actor's local and returns whether it accepted,
    /// together with the outcome for the failure message.
    fn run(self, episode: &mut Episode) -> (bool, String) {
        let expected = InflightTurnIdentity::from_state(&episode.local);
        let offset = episode.local.turn_start_offset;
        let nonce = episode.local.turn_nonce.clone();
        let local = &mut episode.local;
        let saved = |outcome: GuardedSaveOutcome| {
            (outcome == GuardedSaveOutcome::Saved, format!("{outcome:?}"))
        };
        match self {
            Writer::StreamMerge | Writer::StreamMergeDivergentBody => {
                let mut baseline = local.clone();
                if matches!(self, Writer::StreamMergeDivergentBody) {
                    baseline.full_response = "an earlier body the durable row never had".into();
                }
                local.full_response.push_str(" and more");
                saved(save_stream_tick_state_preserving_current_message_races(
                    &mut baseline,
                    local,
                    &expected,
                    episode.durable.current_msg_id,
                    episode.durable.current_msg_len,
                    CALLER,
                ))
            }
            Writer::StrictFence
            | Writer::StrictFenceAuthorityChanged
            | Writer::StrictSelfHandoff => {
                let mut baseline = local.clone();
                if matches!(self, Writer::StrictFenceAuthorityChanged) {
                    local.set_relay_owner_kind(RelayOwnerKind::None);
                    baseline.set_relay_owner_kind(RelayOwnerKind::None);
                }
                local.full_response.push_str(" and more");
                saved(save_stream_tick_state_if_bridge_authority(
                    &mut baseline,
                    local,
                    &expected,
                    episode.durable.current_msg_id,
                    episode.durable.current_msg_len,
                    CALLER,
                ))
            }
            Writer::BridgeEntry => {
                let before = local.clone();
                local.full_response.push_str(" and more");
                saved(patch_bridge_entry_state_if_identity_unchanged(
                    &before, local, CALLER,
                ))
            }
            Writer::BridgeEntryPlaceholderClear => {
                let before = local.clone();
                local.long_running_placeholder_active = false;
                let mut applied = false;
                saved(patch_bridge_entry_state_tracking_placeholder_clear(
                    &before,
                    local,
                    &mut applied,
                    CALLER,
                ))
            }
            Writer::RuntimeStamp => {
                let baseline = local.clone();
                local.last_offset += 32;
                saved(stamp_runtime_handoff_if_matches_identity(
                    (&baseline, &mut *local),
                    &expected,
                    CALLER,
                ))
            }
            Writer::AnchorReuse => {
                let found = recovery_anchor_message_if_matches_identity(
                    &ProviderKind::Codex,
                    CHANNEL,
                    (&expected, offset, nonce.as_deref()),
                    None,
                    None,
                    Some(local),
                );
                (found.is_some(), format!("{found:?}"))
            }
            Writer::AnchorBind => saved(bind_recovery_anchor_if_matches_identity(
                &ProviderKind::Codex,
                CHANNEL,
                (&expected, offset, nonce.as_deref()),
                0,
                Some(0),
                7_219_505,
                5,
                None,
                Some(local),
            )),
            Writer::DeliveryRewind => {
                let owner = StreamRelayAuthority::from_state(local);
                local.full_response = "rewound".into();
                local.response_sent_offset = 0;
                let rewound = save_inflight_delivery_rewind_if_matches_identity(
                    local,
                    nonce.as_deref(),
                    owner,
                    InflightDeliveryRewindReason::TerminalErrorReset,
                );
                (rewound == Ok(true), format!("{rewound:?}"))
            }
            Writer::IdentityRefresh => {
                local.full_response.push_str(" and more");
                saved(
                    save_inflight_state_if_identity_matches_allow_output_restamp(
                        &*local, &expected, CALLER,
                    ),
                )
            }
            Writer::RebindAdoption => {
                local.full_response.push_str(" and more");
                let root = inflight_runtime_root().expect("runtime root");
                saved(
                    save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
                        &root, local, &expected, offset,
                    ),
                )
            }
            Writer::RestartPatch => {
                local.full_response.push_str(" and more");
                let root = inflight_runtime_root().expect("runtime root");
                saved(patch_restart_full_response_if_identity_unchanged_in_root(
                    &root, local, CALLER,
                ))
            }
        }
    }
}

fn row_bytes() -> Vec<u8> {
    let root = inflight_runtime_root().expect("runtime root");
    fs::read(inflight_state_path(&root, &ProviderKind::Codex, CHANNEL)).expect("row bytes")
}

/// Positive control, green before and after the nonce gate: a writer whose actor started the
/// durable episode keeps saving, nonced or legacy alike.
#[test]
fn same_actor_writers_still_save_7219() {
    let mut refused = Vec::new();
    for nonce in [Some("episode-a"), None] {
        for writer in Writer::ALL {
            if matches!(
                writer,
                Writer::StreamMergeDivergentBody | Writer::StrictFenceAuthorityChanged
            ) {
                continue;
            }
            let root = tempfile::tempdir().expect("runtime root");
            let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
            let mut episode = seed(nonce, nonce, writer.shape());
            let (accepted, outcome) = writer.run(&mut episode);
            if !accepted {
                refused.push(format!("{writer:?} nonce={nonce:?}: {outcome}"));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "same-actor writes refused: {refused:#?}"
    );
}

/// The successor's row keeps its bytes and the actor's local keeps its own nonce, whichever side
/// is the legacy `None`.
#[test]
fn successor_nonce_refused_by_every_store_writer_7219() {
    let mut adopted = Vec::new();
    let pairs = [
        (Some("episode-a"), Some("episode-b")),
        (None, Some("episode-b")),
        (Some("episode-a"), None),
    ];
    for (actor, successor) in pairs {
        for writer in Writer::ALL {
            let root = tempfile::tempdir().expect("runtime root");
            let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
            let mut episode = seed(actor, successor, writer.shape());
            let before = row_bytes();
            let (accepted, outcome) = writer.run(&mut episode);
            let row_kept = row_bytes() == before;
            let nonce_kept = episode.local.turn_nonce.as_deref() == actor;
            if accepted || !row_kept || !nonce_kept {
                adopted.push(format!(
                    "{writer:?} actor={actor:?} successor={successor:?}: {outcome} \
                     row_kept={row_kept} local_nonce_kept={nonce_kept}"
                ));
            }
        }
    }
    assert!(
        adopted.is_empty(),
        "a successor episode was adopted: {adopted:#?}"
    );
}

/// A local the successor already overwrote no longer matches the baseline or the entry snapshot
/// it was taken with; each writer refuses on that mismatch before reading the row. The snapshot
/// anchor bind keeps refusing a foreign pin as it did before.
#[test]
fn contaminated_local_and_snapshot_bind_refuse_the_successor_7219() {
    let root = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let (actor, successor) = (Some("episode-a"), Some("episode-b"));

    let mut episode = seed(successor, successor, Shape::Plain);
    let before = row_bytes();
    let mut baseline = episode.local.clone();
    baseline.turn_nonce = actor.map(str::to_owned);
    episode.local.full_response.push_str(" and more");
    let expected = InflightTurnIdentity::from_state(&episode.local);
    let merged = save_stream_tick_state_preserving_current_message_races(
        &mut baseline,
        &mut episode.local,
        &expected,
        0,
        0,
        CALLER,
    );
    assert_eq!(merged, GuardedSaveOutcome::SuccessorOwned);
    assert_eq!(row_bytes(), before);

    let mut episode = seed(successor, successor, Shape::Plain);
    let before = row_bytes();
    let mut baseline = episode.local.clone();
    baseline.turn_nonce = actor.map(str::to_owned);
    episode.local.last_offset += 32;
    let stamped = stamp_runtime_handoff_if_matches_identity(
        (&baseline, &mut episode.local),
        &expected,
        CALLER,
    );
    assert_eq!(stamped, GuardedSaveOutcome::SuccessorOwned);
    assert_eq!(row_bytes(), before);

    let mut episode = seed(actor, actor, Shape::Plain);
    let before = row_bytes();
    let entry_snapshot = episode.local.clone();
    episode.local.turn_nonce = successor.map(str::to_owned);
    episode.local.full_response.push_str(" and more");
    let patched =
        patch_bridge_entry_state_if_identity_unchanged(&entry_snapshot, &mut episode.local, CALLER);
    assert_eq!(patched, GuardedSaveOutcome::SuccessorOwned);
    assert_eq!(row_bytes(), before);

    let mut successor_local = seed(successor, actor, Shape::Plain).local;
    let before = row_bytes();
    assert!(
        bind_recovery_anchor_for_snapshot(&ProviderKind::Codex, &mut successor_local, 7_219_606, 5)
            .is_identity_mismatch_legacy()
    );
    assert_eq!(row_bytes(), before);
}
