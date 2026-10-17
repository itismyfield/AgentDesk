//! A turn that ends on a provider error, at the terminal delivery: the bridge's failure notice is
//! shown and marked failed on O's channel as on a Legacy one, and only a confirmed replace counts.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::turn_bridge::status_panel::provider_failure::PROVIDER_FAILURE_SUMMARY;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

/// A user message id the reaction view treats as a real Discord message.
const USER: u64 = 1_430_000_000_000_000_001;

fn notice() -> String {
    crate::services::discord::commands::owner_error_response(
        &format!("{PROVIDER_FAILURE_SUMMARY}\n같은 요청을 다시 시도해 주세요."),
        "Error: herdr launch did not complete: evidence EnvUnreadable",
    )
}

/// The driver channel's O ledger and its store bytes, as O itself reads them.
fn o_ledger() -> (
    crate::services::tui_o::store::ledger::LedgerState,
    Vec<(String, Vec<u8>)>,
) {
    let root = crate::config::runtime_root().expect("runtime root");
    let store = OStore::existing(&root).expect("O store");
    let era = store.read_era().unwrap().expect("era");
    let channel = store.open_channel(&era, DRIVER_CHANNEL_ID).unwrap();
    let ledger = channel.expect("channel").ledger().clone();
    let mut files = Vec::new();
    let mut dirs = vec![root.join("o_store")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push((path.display().to_string(), std::fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    (ledger, files)
}

fn seed_o_store() {
    let root = crate::config::runtime_root().expect("runtime root");
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().expect("store enabled");
    let at = chrono::Utc::now();
    let init = |channel| {
        Ok(Initialized {
            channel,
            sources: Vec::new(),
            initial_anchor: 7,
            build_digest: "provider-failure".into(),
            at,
        })
    };
    store.begin_era(&[DRIVER_CHANNEL_ID], at, init).unwrap();
}

struct Case {
    o: bool,
    transport_error: bool,
    body: String,
    consumed: bool,
    footer: bool,
    replace: ReplaceBehaviour,
}

struct Seen {
    output: TerminalOutcomeDeliveryOutput,
    calls: Vec<DriverCall>,
    bodies: Vec<String>,
    added: Vec<char>,
    o_ledger_kept: bool,
}

async fn deliver(case: &Case) -> Seen {
    let mut driver = TerminalDeliveryDriver::new(case.replace, 0).with_body(case.body.clone());
    driver.inflight.runtime_kind = Some(ClaudeTui);
    inflight::save_inflight_state(&driver.inflight).expect("seed the Claude TUI row");
    let selected = [(DRIVER_CHANNEL_ID, ClaudeTui)];
    let _o = test_override::force_channels(if case.o { &selected[..] } else { &[] });
    seed_o_store();
    let before = o_ledger();
    let (mut ctx, mut state) = driver.parts();
    ctx.transport_error = case.transport_error;
    ctx.user_msg_id = Some(MessageId::new(USER));
    ctx.single_message_panel_footer_mode = case.footer;
    state.terminal_full_replay_cleanup_msg_ids.clear();
    if case.consumed {
        state.response_sent_offset = state.full_response.len();
    }
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");
    let added = driver
        .shared
        .turn_view_reconciler
        .ops()
        .into_iter()
        .filter(|op| op.add && op.target.message_id.get() == USER)
        .map(|op| op.emoji)
        .collect();
    let calls = driver.observations().into_iter().map(|o| o.call).collect();
    let bodies = driver.published_bodies.lock().unwrap().clone();
    Seen {
        output,
        calls,
        bodies,
        added,
        o_ledger_kept: o_ledger() == before,
    }
}

/// O's tick may or may not have consumed the notice and the panel may be a footer: the bridge
/// still replaces its placeholder with the whole notice, never deletes it, commits only after
/// that replace, marks the request failed, and leaves O's ledger untouched.
#[tokio::test]
async fn provider_failure_notice_is_shown_and_failed_on_o_and_legacy_channels() {
    for (o, consumed, footer) in [
        (true, false, false),
        (true, true, false),
        (true, true, true),
        (false, false, false),
        (false, false, true),
    ] {
        let label = format!("o={o} consumed={consumed} footer={footer}");
        let case = Case {
            o,
            transport_error: true,
            body: notice(),
            consumed,
            footer,
            replace: ReplaceBehaviour::Edited,
        };
        let seen = deliver(&case).await;
        assert_eq!(
            seen.calls,
            [DriverCall::Replace],
            "{label}: {:?}",
            seen.calls
        );
        let [shown] = &seen.bodies[..] else {
            panic!("{label}: one body is shown: {:?}", seen.bodies);
        };
        assert!(shown.ends_with(&notice()), "{label}: {shown}");
        let output = &seen.output;
        assert!(output.provider_failure, "{label}");
        assert!(output.terminal_delivery_committed, "{label}");
        assert!(output.status_panel_terminal_committed, "{label}");
        assert!(!output.preserve_inflight_for_cleanup_retry, "{label}");
        assert_eq!(seen.added, ['⚠'], "{label}: the request is marked failed");
        assert!(seen.o_ledger_kept, "{label}: the notice is no O unit");
    }
}

/// A notice whose replace fails is not delivered: nothing is committed or marked, and the turn
/// stays for the existing retry.
#[tokio::test]
async fn provider_failure_notice_replace_failure_is_not_a_delivery() {
    for consumed in [false, true] {
        let seen = deliver(&Case {
            o: true,
            transport_error: true,
            body: notice(),
            consumed,
            footer: false,
            replace: ReplaceBehaviour::Failed,
        })
        .await;
        let output = &seen.output;
        assert_eq!(seen.calls, [DriverCall::Replace], "consumed={consumed}");
        assert!(!output.terminal_delivery_committed, "consumed={consumed}");
        assert!(
            !output.status_panel_terminal_committed,
            "consumed={consumed}"
        );
        assert!(
            output.preserve_inflight_for_cleanup_retry,
            "consumed={consumed}"
        );
        assert!(
            seen.added.is_empty(),
            "consumed={consumed}: {:?}",
            seen.added
        );
    }
}

/// O's own answer, and an errored turn whose body is O's native text rather than the bridge
/// notice, stay O's: the bridge writes nothing and marks nothing failed. Legacy answers are done.
#[tokio::test]
async fn answers_and_native_bodies_keep_their_owner_and_success_marks() {
    for (o, transport_error) in [(true, false), (true, true), (false, false)] {
        let label = format!("o={o} transport_error={transport_error}");
        let seen = deliver(&Case {
            o,
            transport_error,
            body: DRIVER_BODY.to_string(),
            consumed: o,
            footer: false,
            replace: ReplaceBehaviour::Edited,
        })
        .await;
        assert!(!seen.output.provider_failure, "{label}");
        assert!(seen.output.terminal_delivery_committed, "{label}");
        if o {
            let writes = seen
                .calls
                .iter()
                .filter(|c| **c != DriverCall::Delete)
                .count();
            assert_eq!(writes, 0, "{label}: {:?}", seen.calls);
            assert!(seen.added.is_empty(), "{label}: {:?}", seen.added);
        } else {
            assert_eq!(seen.bodies.len(), 1, "{label}");
            assert_eq!(seen.added, ['✅'], "{label}");
        }
    }
}
