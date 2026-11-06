use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::{
    KickoffCompletion, set_owner_wake_for_tests,
};
use crate::services::herdr_launch::tests::retired_durability_tests::SyncFault;

struct WakeTrace {
    channel: ChannelId,
    count: Arc<AtomicUsize>,
    completion: KickoffCompletion,
}

impl WakeTrace {
    fn install(case: &Case, channel: ChannelId) -> Self {
        Self::install_for_provider(case, channel, &ProviderKind::Claude)
    }

    fn install_for_provider(case: &Case, channel: ChannelId, provider: &ProviderKind) -> Self {
        let completion = KickoffCompletion::install(channel);
        let count = Arc::new(AtomicUsize::new(0));
        let wakes = count.clone();
        let shared = case.shared.clone();
        assert!(
            set_owner_wake_for_tests(
                channel,
                Some(Arc::new(move || {
                    assert!(
                        shared
                            .session_transition_lock(channel)
                            .try_lock_owned()
                            .is_ok(),
                        "completion wake must run after the recovery transition lease releases"
                    );
                    wakes.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .is_none()
        );
        case.fx.rt.block_on(async {
            let persistence = crate::services::discord::queue_dispatch::persistence_context(
                &case.shared,
                provider,
                channel,
            );
            case.shared
                .mailbox(channel)
                .v2_ensure_channel(&persistence, channel)
                .await
                .unwrap();
        });
        Self {
            channel,
            count,
            completion,
        }
    }

    fn processed(&self, case: &Case) -> usize {
        case.fx.rt.block_on(
            self.completion
                .processed(0, std::time::Duration::from_secs(5)),
        );
        self.count.load(Ordering::SeqCst)
    }
}

impl Drop for WakeTrace {
    fn drop(&mut self) {
        assert!(set_owner_wake_for_tests(self.channel, None).is_some());
    }
}

#[test]
fn rr_wake_old_retired_rows_at_boot_do_not_wake_owner_pg() {
    let case = Case::launched("rr-wake-old-retired");
    let channels = [CHANNEL, CHANNEL + 1, CHANNEL + 2];
    let hosts: Vec<_> = channels.iter().map(|channel| (*channel, NODE)).collect();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &hosts);
    let HostedRecord::Known(mut retired) = case.fx.record() else {
        panic!("original fixture execution");
    };
    retired.state = HostedState::Retired;
    case.fx.store(&retired);
    for (offset, channel) in channels.iter().enumerate().skip(1) {
        let mut old = retired.clone();
        old.owner.channel_id = channel.to_string();
        old.owner.logical_key = format!("{}-{offset}", old.owner.logical_key);
        old.execution_nonce = format!("{:032x}", channel);
        old.source_ref.channel = old.owner.channel_id.clone();
        old.source_ref.logical_key = old.owner.logical_key.clone();
        old.source_ref.execution_nonce = old.execution_nonce.clone();
        old.expected.as_mut().unwrap().binding_nonce = old.execution_nonce.clone();
        let key = super::super::super::session_key(&old.owner);
        case.fx.rt.block_on(async {
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(key)
            .bind(TOKEN)
            .bind(channel.to_string())
            .bind(serde_json::to_value(&old).unwrap())
            .execute(&case.fx.pool)
            .await
            .unwrap();
        });
    }
    let wakes: Vec<_> = channels
        .into_iter()
        .map(|channel| WakeTrace::install(&case, ChannelId::new(channel)))
        .collect();
    crate::services::tui_prompt_dedupe::withhold_herdr_execution(
        &retired.owner.logical_key,
        Some(&retired.execution_nonce),
    );
    assert!(
        crate::services::tui_prompt_dedupe::herdr_claim_admission(
            &retired.owner.logical_key,
            true,
        )
        .is_err(),
        "a booted process has no live admission for the old Retired execution"
    );
    let effects = case.effects();
    case.pass();
    assert_eq!(
        case.cleanups(),
        3,
        "all three old local Retired rows completed durable cleanup"
    );
    for wake in &wakes {
        assert_eq!(wake.processed(&case), 0, "old Retired is not a transition");
    }
    case.pass();
    assert_eq!(case.cleanups(), 3);
    for wake in &wakes {
        assert_eq!(wake.processed(&case), 0);
    }
    assert_eq!(case.effects(), effects);
}

#[test]
fn rr_wake_held_to_retired_wakes_owner_once_pg() {
    let case = Case::launched("rr-wake-held");
    let wake = WakeTrace::install(&case, ChannelId::new(CHANNEL));
    let effects = case.effects();
    case.fx.rig.exists(PROVIDER, true);
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.cleanups(), 0);
    assert_eq!(wake.processed(&case), 0, "Held has no completion wake");
    case.fx.rig.exists(PROVIDER, false);
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert_eq!(
        wake.processed(&case),
        1,
        "completed recovery wakes its owner"
    );
    case.pass();
    assert_eq!(wake.processed(&case), 1, "a completed row wakes only once");
    assert_eq!(case.effects(), effects);
}

#[test]
fn rr_wake_bound_candidate_completed_in_first_pass_wakes_owner_pg() {
    let case = Case::launched("rr-wake-candidate");
    let wake = WakeTrace::install(&case, ChannelId::new(CHANNEL));
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert_eq!(
        wake.processed(&case),
        1,
        "this process observed the Bound candidate before completing it"
    );
    case.pass();
    assert_eq!(wake.processed(&case), 1);
}

#[test]
fn rr_wake_cleanup_pending_wakes_only_after_cleanup_completes_pg() {
    let case = Case::launched("rr-wake-cleanup");
    let wake = WakeTrace::install(&case, ChannelId::new(CHANNEL));
    let path = case.hold_path();
    let fault = SyncFault::new(&path);
    fault.fail(true);
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert_eq!(wake.processed(&case), 0, "failed cleanup is not completion");
    fault.fail(false);
    case.pass();
    assert_eq!(case.cleanups(), 2);
    assert_eq!(
        wake.processed(&case),
        1,
        "RetiredCleanupPending becoming complete wakes its owner"
    );
    case.pass();
    assert_eq!(case.cleanups(), 2);
    assert_eq!(wake.processed(&case), 1);
}

#[test]
fn rr_wake_claude_pass_preserves_codex_completion_marker_pg() {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, load_hosted_execution_pg,
    };
    use crate::services::discord::recovery_engine::herdr_reader::prepare_root_replaced_candidate;

    let mut case = Case::launched("rr-wake-provider-order");
    let _codex = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    Arc::get_mut(&mut case.shared).unwrap().provider = ProviderKind::Codex;
    let old_key = super::super::super::session_key(&case.fx.owner);
    let HostedRecord::Known(mut codex) = case.fx.record() else {
        panic!("original fixture execution");
    };
    codex.owner.provider = "codex".into();
    codex.owner.logical_key =
        codex
            .owner
            .logical_key
            .replacen("AgentDesk-claude-", "AgentDesk-codex-", 1);
    codex.source_ref.provider = codex.owner.provider.clone();
    codex.source_ref.logical_key = codex.owner.logical_key.clone();
    codex.expected.as_mut().unwrap().binding_provider = codex.owner.provider.clone();
    let key = format!("codex/{TOKEN}/{NODE}:{}", codex.owner.logical_key);
    let changed = case.fx.rt.block_on(async {
        sqlx::query(
            "UPDATE sessions SET session_key = $1, provider = 'codex', hosted_execution = $2
             WHERE session_key = $3",
        )
        .bind(&key)
        .bind(serde_json::to_value(&codex).unwrap())
        .bind(old_key)
        .execute(&case.fx.pool)
        .await
        .unwrap()
        .rows_affected()
    });
    assert_eq!(changed, 1);
    let load = || {
        let HostedLookup::Found(row) = case.fx.rt.block_on(load_hosted_execution_pg(
            &case.fx.pool,
            HostedLookupKey::SessionKey(&key),
        )) else {
            panic!("exact Codex fixture row");
        };
        row
    };
    let bound = load();
    assert_eq!(
        bound.record,
        HostedRecord::Known(codex.clone()),
        "the provider conversion must decode as the exact Known Bound Codex record"
    );
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let _registry = case.fx.rig.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let candidate = prepare_root_replaced_candidate(&bound)
        .unwrap()
        .observe()
        .unwrap()
        .expect("the Codex row has a fresh RootReplaced refusal");
    recovery::remember_candidate(&candidate);
    codex.state = HostedState::Retired;
    case.fx.rt.block_on(canonical_fixture::set_raw(
        &case.fx.pool,
        &key,
        Some(serde_json::to_value(&codex).unwrap()),
    ));
    let retired = load();
    assert_eq!(
        retired.record,
        HostedRecord::Known(codex.clone()),
        "only the known Codex execution's state changed to Retired"
    );
    assert!(recovery::incomplete_for(&retired));
    let wake =
        WakeTrace::install_for_provider(&case, ChannelId::new(CHANNEL), &ProviderKind::Codex);
    let effects = case.effects();
    case.pass();
    assert!(
        recovery::incomplete_for(&retired),
        "the Claude pass must preserve the Codex row's unfinished recovery marker"
    );
    assert_eq!(case.cleanups(), 0);
    assert_eq!(wake.processed(&case), 0);
    {
        let _runtime = case.fx.rt.enter();
        reconnect_restarted_herdr_with_shared(&case.shared, &ProviderKind::Codex);
    }
    assert_eq!(case.cleanups(), 1);
    assert_eq!(wake.processed(&case), 1);
    assert!(!recovery::incomplete_for(&retired));
    {
        let _runtime = case.fx.rt.enter();
        reconnect_restarted_herdr_with_shared(&case.shared, &ProviderKind::Codex);
    }
    assert_eq!(case.cleanups(), 1);
    assert_eq!(wake.processed(&case), 1);
    assert_eq!(case.effects(), effects);
}
