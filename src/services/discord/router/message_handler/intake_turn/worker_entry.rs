use super::*;

/// Per-message inputs of `handle_text_message` bundled into a single
/// owned struct. Phase 2-pre.3 of intake-node-routing: lets worker-side
/// callers (`execute_intake_turn_core`) accept a single deserialized
/// row from `intake_outbox` instead of a long positional parameter list.
///
/// Payload fields mirror the `intake_outbox` columns (see
/// migrations/postgres/0052_intake_node_routing.sql) and the per-message
/// parameters of the legacy `handle_text_message` signature; the row primary
/// key is carried separately as `intake_outbox_id`.
/// Adding a column to `intake_outbox` means adding a field here.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRequest {
    /// Worker rows carry their claimed `intake_outbox` primary key. Every other
    /// producer carries `None`: a leader-local request is admitted only after any
    /// stale pending route is retired, so no live outbox row remains to own the
    /// turn (the `None` is correct, but the reason is the retirement, not an
    /// absence of any row); and a worker request that loses the mailbox or
    /// session-transition race is re-queued as an `Intervention`, which does not
    /// carry this id, so its later queued-drain reconstruction is `None`. The id
    /// is therefore sealed only along the direct worker path
    /// (`InflightTurnState` into the headless delivery argument struct); the
    /// requeue path is outside that seal. This is harmless today because delivery
    /// does not yet consume the id (it is parked). Binding the requeue /
    /// `Intervention` path, and sealing the `pub` `IntakeRequest` producer seam
    /// itself, are later slices.
    pub intake_outbox_id: Option<i64>,
    pub channel_id: ChannelId,
    pub user_msg_id: MessageId,
    pub source_message_ids: Vec<MessageId>,
    pub busy_followup_retry_user_msg_id: MessageId,
    pub request_owner: UserId,
    pub request_owner_name: String,
    pub user_text: String,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub merge_consecutive: bool,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: TurnKind,
    pub preserve_on_cancel: bool,
}

/// Worker-callable entry point for executing an intake turn. Phase 2-pre.3
/// of intake-node-routing: this is the public surface a worker node will
/// invoke after claiming an `intake_outbox` row from its target queue. Pass
/// the runtime primitives the worker has (`Arc<Http>`, `Arc<SharedData>`,
/// bot token) plus the deserialized message payload; the function constructs
/// `IntakeDeps` with `cache: None` and `ctx_for_chained_dispatch: None`
/// (workers have no live gateway shard) and delegates to the existing
/// intake body.
///
/// Leader producers use `router::intake_dispatch`; a claimed worker bypasses
/// admission so it cannot recursively create another outbox row.
pub(crate) async fn execute_intake_turn_core(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    super::handle_text_message(
        &IntakeDeps {
            http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token,
        },
        request.preserve_on_cancel,
        request,
        false,
        uploads,
        // Worker dispatch has no in-process gate carry-forward; it re-resolves
        // the durable announcement row for its `user_msg_id` (#3905).
        None,
    )
    .await
}

#[cfg(all(test, unix))]
mod input_effect_tests {
    use super::*;
    use crate::services::discord::input_runtime::fence::{self, effect};
    use futures::FutureExt;

    #[tokio::test]
    async fn c1b_worker_actual_intake_holds_provider_and_bridge_through_cleanup() {
        if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
            "services::discord::router::message_handler::intake_turn::worker_entry::input_effect_tests::",
            "c1b_worker_actual_intake_holds_provider_and_bridge_through_cleanup"
        )) {
            return;
        }
        actual_intake_lifetime(false, false).await;
    }

    #[tokio::test]
    async fn c1b_worker_actual_role_override_admits_final_provider_from_off_root() {
        if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
            "services::discord::router::message_handler::intake_turn::worker_entry::input_effect_tests::",
            "c1b_worker_actual_role_override_admits_final_provider_from_off_root"
        )) {
            return;
        }
        actual_intake_lifetime(true, false).await;
    }

    #[tokio::test]
    async fn c1b_worker_actual_redirect_admits_protected_destination_from_off_root() {
        if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
            "services::discord::router::message_handler::intake_turn::worker_entry::input_effect_tests::",
            "c1b_worker_actual_redirect_admits_protected_destination_from_off_root"
        )) {
            return;
        }
        actual_intake_lifetime(false, true).await;
    }

    async fn actual_intake_lifetime(override_provider: bool, redirect: bool) {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        let channel = ChannelId::new(6_325_527);
        let original = if redirect {
            ChannelId::new(6_325_535)
        } else {
            channel
        };
        let workspace = tempfile::tempdir().unwrap();
        let dispatch_workspace = workspace.path().display().to_string();
        let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
            move |_, path| {
                if redirect && path == "/api/internal/card-thread" {
                    Some(serde_json::json!({
                        "active_thread_id": channel.to_string(),
                        "dispatch_type": "implementation",
                        "dispatch_context": serde_json::json!({"worktree_path": dispatch_workspace}).to_string()
                    }))
                } else if redirect && path.ends_with(&format!("/channels/{channel}")) {
                    Some(serde_json::json!({"id": channel.to_string(), "type": 0, "name": "input-test", "position": 0}))
                } else { None }
            },
        )).await;
        crate::services::discord::internal_api::init(api.port, None);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().api_port = api.port;
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
        if redirect {
            crate::services::discord::host_defer_gate::tests::map_channel(&shared, original, "")
                .await;
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&original).unwrap();
            session.channel_name = None;
            session.current_path = Some(workspace.path().display().to_string());
            session.session_id = Some("input-effect-existing".into());
            assert!(fence::lookup(&shared.provider, original.get()).is_none());
        }
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = None;
            session.current_path = Some(workspace.path().display().to_string());
            session.session_id = Some("input-effect-existing".into());
        }
        let final_provider = if override_provider {
            let role_channel = ChannelId::new(6_325_530);
            let path = crate::services::discord::runtime_store::role_map_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                serde_json::json!({
                    "byChannelId": {role_channel.get().to_string(): {
                        "roleId": "input-effect-codex", "promptFile": "", "provider": "codex"
                    }}
                })
                .to_string(),
            )
            .unwrap();
            shared.dispatch.role_overrides.insert(channel, role_channel);
            assert!(fence::lookup(&shared.provider, channel.get()).is_none());
            ProviderKind::Codex
        } else {
            shared.provider.clone()
        };
        let gate = fence::Gate::protect(final_provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let (entered, provider_entered) = tokio::sync::oneshot::channel();
        let (release_provider, release) = std::sync::mpsc::channel();
        *super::super::super::provider_dispatch::INPUT_EFFECT_PROBE
            .lock()
            .unwrap() = Some(super::super::super::provider_dispatch::InputEffectProbe {
            channel: channel.get(),
            entered,
            release,
            terminal_before_release: true,
        });
        let (captured, bridge_captured) = tokio::sync::oneshot::channel();
        let (release_bridge, resume) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_CAPTURE_PROBE
            .lock()
            .unwrap() = Some((channel, captured, resume));
        let (completed, bridge_completed) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((channel, completed));
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: original,
            user_msg_id: MessageId::new(6_325_528),
            source_message_ids: vec![MessageId::new(6_325_529)],
            busy_followup_retry_user_msg_id: MessageId::new(6_325_528),
            request_owner: UserId::new(7),
            request_owner_name: "owner".into(),
            user_text: if redirect {
                "DISPATCH:input-effect-local - input effect".into()
            } else {
                "input effect".into()
            },
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: false,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            execute_intake_turn_core(&api.http, &shared, "", request, Vec::new()),
        )
        .await
        .unwrap()
        .unwrap();
        let provider = tokio::time::timeout(std::time::Duration::from_secs(10), provider_entered)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge_captured)
            .await
            .unwrap()
            .unwrap();
        let row =
            crate::services::discord::inflight::load_inflight_state(&final_provider, channel.get())
                .unwrap();
        assert_eq!(row.source_message_ids, vec![6_325_529]);
        let closing = gate.close().unwrap();
        let held = closing.drain().now_or_never().is_none();
        release_bridge.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge_completed)
            .await
            .unwrap()
            .unwrap();
        let provider_holds_after_bridge = closing.drain().now_or_never().is_none();
        release_provider.send(()).unwrap();
        assert!(
            provider_holds_after_bridge,
            "provider still owns an effect after bridge cleanup and drop"
        );
        assert!(
            held,
            "intake return does not end provider or bridge effects"
        );
        assert_eq!(
            provider,
            (true, true),
            "actual intake provider has named worker capability"
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), closing.drain())
            .await
            .unwrap();
        assert!(
            crate::services::discord::inflight::load_inflight_state(&final_provider, channel.get())
                .is_none()
        );
        let mailbox = shared.mailbox_peek(channel).unwrap().snapshot().await;
        assert!(mailbox.cancel_token.is_none());
        assert!(mailbox.active_user_message_id.is_none());
        assert!(effect::current().is_none());
        assert!(!crate::services::discord::live_bridge::is_live(
            &final_provider,
            channel.get()
        ));
        shared.mailboxes.remove_fixture_for_test(channel);
    }
}
