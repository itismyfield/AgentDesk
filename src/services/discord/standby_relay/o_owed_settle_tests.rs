//! A standby relay whose body O took settles its row only when O's init covers the turn from its
//! start; otherwise the relay ends at the gate and leaves the row as it was.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::test_support::wait::{WAIT_CAP, await_within};

const PRIOR: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"earlier"}"#;
const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"standby answer"}]}}"#;
const OK: &str =
    r#"{"type":"result","subtype":"success","is_error":false,"result":"standby answer"}"#;

/// One era whose init attaches each channel's transcript from its own delivery start.
fn init_over(sources: &[(u64, String, u64)]) {
    let root = crate::config::runtime_root().unwrap();
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, &root).unwrap().unwrap();
    let source_id = crate::services::tui_o::shadow::binding_reader::source_id_for;
    let at = chrono::Utc::now();
    let channels: Vec<u64> = sources.iter().map(|(channel, _, _)| *channel).collect();
    let init = |channel| {
        let (_, path, delivery_start) = sources.iter().find(|(c, _, _)| *c == channel).unwrap();
        let source_id = source_id("session", std::path::Path::new(path)).unwrap();
        let prefix_hash = String::new();
        let sources = vec![InitSource {
            source_id,
            delivery_start: *delivery_start,
            prefix_hash,
        }];
        let (initial_anchor, build_digest) = (0, "test".into());
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    store.begin_era(&channels, at, init).unwrap();
}

/// O owns both channels; only the one whose init starts at or before the turn clears its row.
#[tokio::test(flavor = "current_thread")]
async fn a_standby_body_o_owes_settles_its_row_and_one_o_does_not_owe_stays() {
    with_isolated_runtime_root(|| async move {
        let dir = tempfile::tempdir().unwrap();
        let (owed, late) = (7_651_201u64, 7_651_202u64);
        let start = PRIOR.len() as u64 + 1;
        let cases = [(owed, start, true), (late, start + 1, false)];
        let mut sources = Vec::new();
        for (channel, delivery_start, _) in cases {
            let path = dir.path().join(format!("{channel}.jsonl"));
            std::fs::write(&path, format!("{PRIOR}\n{ASSISTANT}\n{OK}\n")).unwrap();
            sources.push((channel, path.display().to_string(), delivery_start));
        }
        init_over(&sources);
        let _owned = test_override::force_channels(&[(owed, ClaudeTui), (late, ClaudeTui)]);
        let shared = crate::services::discord::make_shared_data_for_tests();
        for ((channel, _, settles), (_, path, _)) in cases.into_iter().zip(&sources) {
            let mut row = InflightTurnState::new(
                ProviderKind::Claude,
                channel,
                None,
                7,
                channel + 1,
                0,
                "prompt".to_string(),
                None,
                None,
                Some(path.clone()),
                None,
                0,
            );
            row.turn_start_offset = Some(start);
            row.runtime_kind = Some(ClaudeTui);
            crate::services::discord::inflight::save_inflight_state(&row).unwrap();
            let relay = run_standby_relay(
                Arc::new(serenity::http::Http::new("")),
                ChannelId::new(channel),
                None,
                path.clone(),
                StandbyRelayTurnBinding::from_state(&row),
                start,
                Arc::new(AtomicBool::new(false)),
                shared.clone(),
                ProviderKind::Claude,
                Duration::from_secs(20),
            );
            await_within(relay, WAIT_CAP).await;
            let kept = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel,
            );
            assert_eq!(kept.is_none(), settles, "{channel}");
        }
    })
    .await;
}
