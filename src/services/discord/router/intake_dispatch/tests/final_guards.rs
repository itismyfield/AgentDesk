use super::*;
use crate::services::discord::input_runtime::fence::{Gate, test_health};
use crate::services::discord::router::message_handler;
use crate::services::discord::{DiscordSession, HistoryItem, HistoryType};
use std::sync::atomic::{AtomicUsize, Ordering};

fn isolated_config() -> crate::config::TestEnvVarGuard {
    let config = crate::config::runtime_root()
        .unwrap()
        .join("isolated-intake-config.yml");
    std::fs::write(&config, "server: {}\n").unwrap();
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config)
}

fn closed_http() -> Arc<serenity::Http> {
    Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:1")
            .ratelimiter_disabled(true)
            .build(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn c2b_protected_final_admission_never_returns_false_local_or_nonlocal_success() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let _config = isolated_config();
    let channel = ChannelId::new(6_325_721);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = closed_http();
    let deps = deps(&http, &shared);
    let old_local =
        super::super::admit_text_intake(&deps, &submission_for_admission(channel, 6_325_722)).await;
    assert!(
        matches!(old_local, IntakeAdmission::Local(_)),
        "legacy local admission fixture"
    );
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let mut refused = Vec::new();
    for admission in [
        old_local,
        IntakeAdmission::DeferredOpenRoute {
            target_instance_id: "old-owner".into(),
            prepared_uploads: Vec::new(),
        },
        IntakeAdmission::ReplayDeferred(super::super::replay_admission::ReplayDefer::LookupFailed),
        IntakeAdmission::SkippedDuplicate,
        IntakeAdmission::ConsumedToHold,
        IntakeAdmission::Forwarded {
            target_instance_id: "old-owner".into(),
            outbox_id: 4,
        },
    ] {
        let outcome = super::super::finish_text_intake_admission(
            &deps,
            admission,
            submission_for_admission(channel, 6_325_722),
        )
        .await;
        refused.push(outcome.is_err());
    }
    assert_eq!(
        refused,
        vec![true; 6],
        "protected local/deferred/replay/duplicate/hold/forwarded outcomes all refuse legacy success"
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2b_protected_live_dispatch_does_not_checkpoint_or_consume_the_original() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("enforce"),
    );
    let _config = isolated_config();
    let channel = ChannelId::new(6_325_723);
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = closed_http();
    let deps = deps(&http, &shared);
    let checkpoints = AtomicUsize::new(0);
    let outcome = super::super::dispatch_live_text_intake(
        &deps,
        submission_for_admission(channel, 6_325_724),
        || {
            checkpoints.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert_eq!(
        checkpoints.load(Ordering::SeqCst),
        0,
        "protected original cannot advance legacy checkpoint"
    );
    assert!(
        outcome.is_err(),
        "no legacy success without typed C custody"
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2b_protected_cached_attachment_permit_does_not_write_history() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let _config = isolated_config();
    let channel = ChannelId::new(6_325_725);
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.core.lock().await.sessions.insert(
        channel,
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![HistoryItem {
                item_type: HistoryType::User,
                content: "existing original".into(),
            }],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
    let before: Vec<_> = shared.core.lock().await.sessions[&channel]
        .history
        .iter()
        .map(|item| (format!("{:?}", item.item_type), item.content.clone()))
        .collect();
    let http = closed_http();
    let deps = deps(&http, &shared);
    let permit = super::super::LocalAdmissionPermit {
        channel_id: channel,
        request_owner: UserId::new(4350),
        prepared_uploads: vec!["cached attachment".into()],
    };
    let outcome = super::super::prepare_admitted_live_attachments(
        &deps,
        &permit,
        channel,
        channel,
        false,
        &[message_handler::AttachmentDescriptor {
            filename: "original.txt".into(),
            url: "https://cdn.discordapp.com/attachments/1/2/original.txt".into(),
        }],
    )
    .await;
    assert!(
        outcome.is_err(),
        "an old local permit cannot authorize protected attachment IO"
    );
    let after: Vec<_> = shared.core.lock().await.sessions[&channel]
        .history
        .iter()
        .map(|item| (format!("{:?}", item.item_type), item.content.clone()))
        .collect();
    assert_eq!(
        after, before,
        "cached uploads cannot enter protected history"
    );
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2b_protected_skill_refuses_both_local_permit_and_unadmitted_dispatch() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let _config = isolated_config();
    let channel = ChannelId::new(6_325_727);
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = closed_http();
    let deps = deps(&http, &shared);
    for permit in [
        None,
        Some(super::super::LocalAdmissionPermit {
            channel_id: channel,
            request_owner: UserId::new(4351),
            prepared_uploads: Vec::new(),
        }),
    ] {
        let outcome = dispatch_skill_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            MessageId::new(6_325_728),
            UserId::new(4350),
            "original owner".into(),
            "skill prompt".into(),
            IntakeOrigin::TextSkill,
            Vec::new(),
            permit,
        )
        .await;
        assert!(
            outcome.is_err(),
            "skill cannot acknowledge execution through an old local permit"
        );
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
    }
}

#[test]
fn c2b_legacy_guard_keeps_a_small_caller_future() {
    fn future_bytes<F>(
        _: impl FnOnce(
            &'static Arc<crate::services::discord::SharedData>,
            &'static Arc<serenity::Http>,
            &'static ProviderKind,
            ChannelId,
        ) -> F,
    ) -> usize {
        std::mem::size_of::<F>()
    }

    let bytes = future_bytes(super::super::legacy_allowed);
    println!("G2C_LEGACY_GUARD_FUTURE_BYTES={bytes}");
    assert!(
        bytes <= 1024,
        "legacy guard embeds {bytes} bytes in each caller future"
    );
}
