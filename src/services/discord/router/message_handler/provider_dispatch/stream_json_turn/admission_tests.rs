use super::*;
use crate::services::discord::inflight::InflightTurnState;
#[cfg(unix)]
use crate::services::provider_auth_profile::ProviderAuthOverlay;
#[cfg(unix)]
use crate::services::stream_json_cli::policy::ToolPolicy;
#[cfg(unix)]
use crate::services::turn_host::TurnHost;

const DESTINATION: u64 = 41;
const PARENT: u64 = 42;
const MESSAGE: u64 = 43;

fn inflight(cancel: &CancelToken) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Antigravity,
        DESTINATION,
        None,
        7,
        MESSAGE,
        0,
        "hello".into(),
        None,
        None,
        None,
        None,
        0,
    );
    state.logical_channel_id = Some(PARENT);
    state.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    state.session_key = Some("discord:41".into());
    state
}

#[test]
fn only_the_exact_saved_discord_mailbox_claim_can_supply_the_input() {
    let cancel = CancelToken::new();
    let state = inflight(&cancel);
    let source = Some(MessageId::new(MESSAGE));
    assert!(
        AgyForeground::from_claim(source, TurnKind::Foreground, true, &state, &cancel).is_some()
    );
    assert!(AgyForeground::from_claim(None, TurnKind::Foreground, true, &state, &cancel).is_none());
    assert!(
        AgyForeground::from_claim(source, TurnKind::BackgroundTrigger, true, &state, &cancel)
            .is_none()
    );
    assert!(
        AgyForeground::from_claim(source, TurnKind::Foreground, false, &state, &cancel).is_none()
    );
    assert!(
        AgyForeground::from_claim(
            source,
            TurnKind::Foreground,
            true,
            &state,
            &CancelToken::new(),
        )
        .is_none()
    );
}

#[tokio::test]
async fn http_claim_requires_a_saved_inflight_and_same_writer_seat() {
    use crate::services::imsg_reply::agy_input::{
        ClaimedInput, SELECTED_WRITER, WriterGate, WriterSelection, with_claim,
    };

    SELECTED_WRITER
        .scope(WriterGate::unready(DESTINATION), async {
            assert!(agy_input::selected(&ProviderKind::Antigravity, DESTINATION).is_some());
            assert!(agy_input::ready(&ProviderKind::Antigravity, DESTINATION).is_none());
        })
        .await;

    let root = tempfile::tempdir().unwrap();
    let selection = WriterSelection {
        root: root.path().to_path_buf(),
        destination: DESTINATION,
        logical_channel: PARENT,
        generation: NonZeroU64::MIN,
    };
    let source = ImessageSource::from_guid("p:0/GUID").unwrap();
    assert!(ImessageSource::from_guid(" ").is_none());
    let claim = ClaimedInput {
        selection: selection.clone(),
        source: source.clone(),
        guid: "p:0/GUID".into(),
        author: 7,
        message: MESSAGE,
    };
    let cancel = CancelToken::new();
    let mut state = inflight(&cancel);
    assert!(AgyForeground::from_http_claim(true, &state, &cancel, Some(PARENT)).is_none());
    SELECTED_WRITER
        .scope(
            WriterGate::ready(selection),
            with_claim(Some(claim), async {
                AgyForeground::stamp_http(&mut state);
                assert_eq!(state.input_origin_ids, ["p:0/GUID"]);
                assert_eq!(
                    state.agy_imessage_source_sha256.as_deref(),
                    Some(source.digest())
                );
                let proof = |saved, parent| {
                    AgyForeground::from_http_claim(saved, &state, &cancel, Some(parent))
                        .unwrap()
                        .imessage
                };
                assert_eq!(proof(true, PARENT), Ok(Some(source)));
                assert!(proof(false, PARENT).is_err());
                assert!(proof(true, PARENT + 1).is_err());
            }),
        )
        .await;
}

#[cfg(unix)]
#[test]
fn writer_selection_reserves_once_and_preparation_failure_settles_it() {
    let root = tempfile::tempdir().unwrap();
    let cancel = Arc::new(CancelToken::new());
    let state = inflight(&cancel);
    let source = Some(MessageId::new(MESSAGE));
    let mut input =
        AgyForeground::from_claim(source, TurnKind::Foreground, true, &state, &cancel).unwrap();
    let request = ProviderTurnRequest {
        provider: ProviderKind::Antigravity,
        prompt: "hello".into(),
        system_prompt: None,
        tool_policy: ConfiguredToolPolicy::Explicit(ToolPolicy::ReadOnly),
        model: None,
        reasoning_effort: None,
        working_directory: root.path().into(),
        session: None,
        timeout: Duration::ZERO,
        cancel: Some(cancel.clone()),
        auth_overlay: ProviderAuthOverlay::default_for(ProviderKind::Antigravity),
    };
    let mut turn = StreamingTurn {
        pool: None,
        registration_origin: None,
        agy_foreground: None,
        shared: None,
        provider: &ProviderKind::Antigravity,
        prompt: "hello",
        session_id: None,
        working_dir: "",
        system_prompt: None,
        allowed_tools: &[],
        cancel,
        codex_observation: None,
        tmux_session_name: None,
        teardown: None,
        host: &TurnHost::Tmux,
        channel_id: DESTINATION,
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    assert!(input.reserve(&turn, &request).unwrap().is_none());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    input.ready = Some(ReadyWriter {
        runtime_root: root.path().into(),
        destination: DESTINATION,
        logical_channel: PARENT + 1,
        generation: NonZeroU64::new(1).unwrap(),
    });
    assert!(input.reserve(&turn, &request).is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    input.ready.as_mut().unwrap().logical_channel = PARENT;
    turn.agy_foreground = Some(input.clone());
    let (tx, _rx) = std::sync::mpsc::channel();
    assert!(
        execute_request(StreamJsonDialectId::Agy, &turn, request.clone(), tx)
            .unwrap_err()
            .contains("UnsupportedToolPolicy")
    );
    assert!(input.reserve(&turn, &request).is_err());
    let written =
        std::fs::read_to_string(root.path().join("o_store/41/agy/v1/43/0000.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["kind"], "open");
    assert_eq!(records[1]["data"]["reason"], "preparation");
}
