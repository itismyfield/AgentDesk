use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::{ChannelId, MessageId};

use super::*;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::placeholder_controller::PlaceholderController;
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseSnapshot};
use crate::services::provider::ProviderKind;

struct CountingGateway {
    sends: AtomicUsize,
}

impl CountingGateway {
    fn new() -> Self {
        Self {
            sends: AtomicUsize::new(0),
        }
    }
}

impl TurnGateway for CountingGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            let id = self.sends.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(MessageId::new(id as u64))
        })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a crate::services::discord::Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}

fn seed_generation(tmux_session_name: &str) -> i64 {
    let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    std::fs::create_dir_all(Path::new(&path).parent().expect("generation parent"))
        .expect("create generation parent");
    std::fs::write(&path, b"1").expect("write generation marker");
    let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux_session_name);
    assert_ne!(generation_mtime_ns, 0, "generation marker must be readable");
    generation_mtime_ns
}

fn ctx<'a>(
    channel: ChannelId,
    lease: &'a DeliveryLeaseCell,
    controller: &'a PlaceholderController,
    provider: &ProviderKind,
    tmux_session_name: &str,
    body: &'a str,
    range: Option<(u64, u64)>,
) -> TurnOutputCtx<'a, DeliveryLeaseCell> {
    let turn = TurnKey::new(channel, 4046, 1);
    TurnOutputCtx {
        turn,
        lease_key: Some(DeliveryLeaseKey::from_turn_key(turn)),
        owner: RelayOwnerKind::Watcher,
        holder: LeaseHolder::Sink,
        lease,
        channel_id: channel,
        placeholder_controller: controller,
        placeholder: PlaceholderSlot::None,
        body,
        send_range: range.unwrap_or((4096, 4096)),
        plan: OutputPlan::SendFresh {
            range,
            reference: None,
            record: FreshSendRecord {
                provider: provider.clone(),
                record_channel_id: channel,
                tmux_session_name: tmux_session_name.to_string(),
                attempts: 2,
            },
        },
        edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
        fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
        acquire_failure_mode: AcquireFailureMode::Transient,
        advance: None,
        heartbeat: None,
    }
}

#[test]
fn range_fresh_send_commits_and_records_durable_frontier() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_101);
    let tmux = "AgentDesk-claude-4046-range";
    let body = "range fresh answer";
    let range = (120, 120 + body.len() as u64);
    let generation_mtime_ns = seed_generation(tmux);
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();

    let outcome = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(
            channel,
            &lease,
            &controller,
            &provider,
            tmux,
            body,
            Some(range),
        ),
    ));

    assert!(matches!(
        outcome,
        DeliveryOutcome::FreshDelivered {
            committed_to: Some(committed_to),
            persistence_recorded: true,
        } if committed_to == range.1
    ));
    assert_eq!(gateway.sends.load(Ordering::SeqCst), 1);
    assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
    let record =
        delivery_record::read_record(&provider, channel.get()).expect("fresh-send durable record");
    assert_eq!(
        record.delivered_frontier,
        Some(delivery_record::DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: 2,
            panel_msg_id: Some(1),
            panel_channel_id: Some(channel.get()),
        })
    );
}

#[test]
fn no_range_fresh_send_records_fingerprint_and_retry_is_suppressed() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_102);
    let tmux = "AgentDesk-claude-4046-norange";
    let body = "markerless fresh answer";
    seed_generation(tmux);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();

    let first_lease = DeliveryLeaseCell::new(channel);
    let first = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(
            channel,
            &first_lease,
            &controller,
            &provider,
            tmux,
            body,
            None,
        ),
    ));
    assert!(matches!(
        first,
        DeliveryOutcome::FreshDelivered {
            committed_to: None,
            persistence_recorded: true,
        }
    ));
    assert!(delivery_record::recent_fresh_send_content_matches(
        &provider, channel, tmux, body
    ));
    assert!(
        !delivery_record::recent_delivered_content_matches(&provider, channel, tmux, body),
        "P2-4: fresh-send retry metadata must not enter watcher suppression authority"
    );

    let retry_lease = DeliveryLeaseCell::new(channel);
    let retry = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(
            channel,
            &retry_lease,
            &controller,
            &provider,
            tmux,
            body,
            None,
        ),
    ));
    assert!(matches!(retry, DeliveryOutcome::Skipped));
    assert_eq!(
        gateway.sends.load(Ordering::SeqCst),
        1,
        "mutation: removing the fingerprint write/check re-sends the retry"
    );
    assert!(
        delivery_record::read_record(&provider, channel.get()).is_none(),
        "F3/P2-4: NoRange must not create a watcher-shared delivery record"
    );
}

#[test]
fn no_range_pseudo_range_lease_closes_concurrent_dedup_gap() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_103);
    let tmux = "AgentDesk-claude-4046-pseudo-range";
    let body = "concurrent markerless answer";
    seed_generation(tmux);
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();
    let turn = TurnKey::new(channel, 4046, 1);
    let key = DeliveryLeaseKey::from_turn_key(turn);
    let expected = super::fresh_send::pseudo_range(4096, body);
    assert!(lease.try_acquire(
        key.clone(),
        LeaseHolder::Bridge,
        expected.0,
        expected.1,
        lease_now_ms().saturating_add(TURN_OUTPUT_LEASE_TTL_MS),
    ));

    let outcome = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(channel, &lease, &controller, &provider, tmux, body, None),
    ));

    assert!(matches!(
        outcome,
        DeliveryOutcome::Transient { retry_from_offset } if retry_from_offset == expected.0
    ));
    assert_eq!(
        gateway.sends.load(Ordering::SeqCst),
        0,
        "mutation: bypassing/removing the pseudo-range acquire posts into the fingerprint gap"
    );
    assert!(!delivery_record::recent_fresh_send_content_matches(
        &provider, channel, tmux, body
    ));
    assert!(lease.release(LeaseHolder::Bridge, key, expected.0, expected.1));
}

#[test]
fn no_range_fresh_send_never_invokes_owner_advance() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_104);
    let tmux = "AgentDesk-claude-4046-no-advance";
    let body = "NoRange deliver without advance";
    seed_generation(tmux);
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();
    let advance = |_: (u64, u64)| -> bool {
        panic!("NoRange must not invoke owner advance");
    };
    let mut context = ctx(channel, &lease, &controller, &provider, tmux, body, None);
    context.advance = Some(&advance);

    let outcome = futures::executor::block_on(super::deliver_turn_output(&gateway, context));

    assert!(matches!(
        outcome,
        DeliveryOutcome::FreshDelivered {
            committed_to: None,
            persistence_recorded: true,
        }
    ));
    assert_eq!(gateway.sends.load(Ordering::SeqCst), 1);
    assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
}

fn assert_channel_mismatch_skips_before_post(range: Option<(u64, u64)>, channel_id: u64) {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel_id);
    let record_channel = ChannelId::new(channel_id + 1);
    let tmux = "AgentDesk-claude-4046-channel-mismatch";
    let body = "must not post to a mismatched channel";
    seed_generation(tmux);
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();
    let mut context = ctx(channel, &lease, &controller, &provider, tmux, body, range);
    let OutputPlan::SendFresh { record, .. } = &mut context.plan else {
        unreachable!("test context must build SendFresh");
    };
    record.record_channel_id = record_channel;

    let outcome = futures::executor::block_on(super::deliver_turn_output(&gateway, context));

    assert!(matches!(outcome, DeliveryOutcome::Skipped));
    assert_eq!(
        gateway.sends.load(Ordering::SeqCst),
        0,
        "mutation: removing the channel-equality guard posts before rejecting mismatch"
    );
    assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
    assert!(!delivery_record::recent_fresh_send_content_matches(
        &provider,
        record_channel,
        tmux,
        body,
    ));
}

#[test]
fn range_channel_mismatch_is_refused_before_post() {
    assert_channel_mismatch_skips_before_post(Some((10, 20)), 40_460_105);
}

#[test]
fn no_range_channel_mismatch_is_refused_before_lookup_or_post() {
    assert_channel_mismatch_skips_before_post(None, 40_460_107);
}

#[test]
fn missing_generation_is_exposed_after_confirmed_no_range_post() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_109);
    let tmux = "AgentDesk-claude-4046-missing-generation";
    let body = "confirmed post without generation";
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();

    let outcome = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(channel, &lease, &controller, &provider, tmux, body, None),
    ));

    assert!(matches!(
        outcome,
        DeliveryOutcome::FreshDelivered {
            committed_to: None,
            persistence_recorded: false,
        }
    ));
    assert_eq!(gateway.sends.load(Ordering::SeqCst), 1);
}

#[test]
fn range_persistence_failure_is_not_hidden_as_delivered() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(40_460_110);
    let tmux = "AgentDesk-claude-4046-frontier-failure";
    let body = "confirmed range post with failed frontier";
    let range = (80, 80 + body.len() as u64);
    seed_generation(tmux);
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&runtime).expect("create runtime root");
    std::fs::write(runtime.join("discord_delivery_records"), b"not a directory")
        .expect("block delivery-record namespace");
    let lease = DeliveryLeaseCell::new(channel);
    let controller = PlaceholderController::default();
    let gateway = CountingGateway::new();

    let outcome = futures::executor::block_on(super::deliver_turn_output(
        &gateway,
        ctx(
            channel,
            &lease,
            &controller,
            &provider,
            tmux,
            body,
            Some(range),
        ),
    ));

    assert!(matches!(
        outcome,
        DeliveryOutcome::FreshDelivered {
            committed_to: Some(committed_to),
            persistence_recorded: false,
        } if committed_to == range.1
    ));
    assert_eq!(gateway.sends.load(Ordering::SeqCst), 1);
    assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
}
