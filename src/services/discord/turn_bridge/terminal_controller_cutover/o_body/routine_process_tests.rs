use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui, ProcessBackend};
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::cutover::{
    BodySend, BodySite, IdentityError, claim_then_send_at, test_override,
};
use crate::services::tui_o::store::{OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{ActivationFacts, activate_with};
use serde_json::{Value, json};

fn thread(channel: u64, parent: u64, fresh: bool) -> Candidate {
    let surface = serde_json::from_value(json!({
        "id": channel.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "fixture"
    }))
    .unwrap();
    assert!(cutover::record_thread_child(&surface, fresh));
    cutover::boot_snapshot()
        .unwrap()
        .child(channel)
        .unwrap()
        .unwrap()
        .candidate
}

fn row(channel: u64, parent: u64) -> Value {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        8,
        9,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.runtime_kind = Some(ProcessBackend);
    row.turn_nonce = Some("routine-output-nonce".into());
    let mut wire = serde_json::to_value(row).unwrap();
    wire["routine_presentation"] = json!({"process_thread": {
        "parent_channel_id": parent, "channel_id": channel, "provider": "claude",
        "turn_nonce": "routine-output-nonce"
    }});
    wire
}

fn peek(channel: u64, wire: Value) -> Result<bool, IdentityError> {
    let state = serde_json::from_value(wire).unwrap();
    bridge_o_body_peek_decision(ChannelId::new(channel), &state, false)
}

#[tokio::test]
async fn i8372_process_claim_preserves_other_channel_and_turn_kind_decisions() {
    with_isolated_runtime_root(|| async {
        let (parent, channel, pending) = (9_837_201, 9_837_202, 9_837_203);
        let _boot = test_override::force_channels(&[(parent, ClaudeTui)]);
        let deferred = thread(channel, parent, false);
        let pending_candidate = thread(pending, parent, true);
        let verified = row(channel, parent);
        assert_eq!(peek(channel, verified.clone()), Ok(false));
        assert_eq!(deferred.sends(), (0, 0), "peek never reserves a send");

        for pointer in [
            "/routine_presentation/process_thread/parent_channel_id",
            "/routine_presentation/process_thread/channel_id",
            "/routine_presentation/process_thread/provider",
            "/routine_presentation/process_thread/turn_nonce",
            "/tmux_session_name",
            "/input_fifo_path",
            "/silent_turn",
        ] {
            let mut foreign = verified.clone();
            match pointer {
                "/silent_turn" => foreign["silent_turn"] = json!(true),
                "/tmux_session_name" => foreign["tmux_session_name"] = json!("another-pane"),
                "/input_fifo_path" => foreign["input_fifo_path"] = json!("/test/fifo"),
                _ => {
                    *foreign.pointer_mut(pointer).unwrap() = if pointer.ends_with("_id") {
                        json!(parent + 100)
                    } else {
                        json!("unrelated")
                    };
                }
            }
            assert!(peek(channel, foreign).is_err(), "accepted {pointer}");
        }
        assert!(peek(pending, row(pending, parent)).is_err());
        assert_eq!(pending_candidate.peek(), Adoption::Pending);
        assert!(peek(channel + 100, verified.clone()).is_err());

        let mut unproved = verified.clone();
        unproved
            .as_object_mut()
            .unwrap()
            .remove("routine_presentation");
        assert_eq!(
            peek(channel, unproved.clone()),
            Err(IdentityError::KindMismatch {
                expected: Some(ClaudeTui),
                actual: ProcessBackend,
            }),
        );
        unproved["runtime_kind"] = json!("codex_tui");
        assert_eq!(
            peek(channel, unproved.clone()),
            Err(IdentityError::KindMismatch {
                expected: Some(ClaudeTui),
                actual: CodexTui,
            }),
        );
        assert_eq!(peek(channel + 100, unproved), Ok(false));
        assert_eq!(deferred.sends(), (0, 0));

        assert!(deferred.confirm_store_for_test());
        assert!(
            peek(channel, verified).is_err(),
            "O cannot ACK a process result"
        );
    })
    .await;
}

#[tokio::test]
async fn i8372_process_claim_rechecks_o_adoption_and_counts_legacy_transport() {
    with_isolated_runtime_root(|| async {
        let (parent, first, second) = (9_837_211, 9_837_212, 9_837_213);
        let _boot = test_override::force_channels(&[(parent, ClaudeTui)]);
        let winner = thread(first, parent, false);
        let sender = thread(second, parent, false);
        let first_row = serde_json::from_value(row(first, parent)).unwrap();
        let second_row = serde_json::from_value(row(second, parent)).unwrap();
        let first_claim = bridge_body_claim(ChannelId::new(first), &first_row, false);
        assert_eq!(first_claim.peek_o_owns_tui_output_for_channel(), Ok(false));
        assert_eq!(
            claim_then_send_at(BodySite::BridgeTerminal, Some(first_claim), || async {
                panic!("only the durable headless transport can send this body")
            })
            .await,
            Err::<BodySend<()>, _>(IdentityError::ChildDeliveryHeld),
        );
        assert!(winner.confirm_store_for_test());
        assert_eq!(
            claim_then_send_at(BodySite::BridgeHeadless, Some(first_claim), || async {
                panic!("O committed first")
            })
            .await,
            Err::<BodySend<()>, _>(IdentityError::ChildDeliveryHeld),
        );

        let stores = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, stores.path())
            .unwrap()
            .unwrap();
        let sent = claim_then_send_at(
            BodySite::BridgeHeadless,
            Some(bridge_body_claim(
                ChannelId::new(second),
                &second_row,
                false,
            )),
            || async {
                assert_eq!(sender.sends(), (1, 0));
                assert_eq!(
                    activate_with(
                        &store,
                        second,
                        Ok(ActivationFacts::default()),
                        || Ok(false),
                        &sender,
                        || Ok(Vec::new()),
                    ),
                    Err("Legacy body send in flight".to_owned()),
                );
                assert!(store.read_init(second).unwrap().is_none());
            },
        )
        .await;
        assert_eq!(sent, Ok(BodySend::Sent(())));
        assert_eq!(sender.sends(), (1, 1));
        assert_eq!(sender.peek(), Adoption::Deferred);
    })
    .await;
}
