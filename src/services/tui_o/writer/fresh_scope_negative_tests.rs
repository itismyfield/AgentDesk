//! Real proof history and source identities bound scoped restoration.

use super::*;
use crate::services::tui_o::cutover::test_override;
use futures::FutureExt;

fn next_context(root: &Path, policy: &str) -> BindingContext {
    let mut next = context(root, CANARY_TMUX, policy);
    next.host = dedupe::binding_context::stable_host_identity();
    next.first_prompt_digest = Some(format!("sha256:{:x}", Sha256::digest(b"warm proof input")));
    next
}

fn publish_context(next: &BindingContext) {
    PreparedIncarnation::create(next.clone()).unwrap();
    publish_nonce(next);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    dedupe::set_codex_delivery_permission_for_tests(
        next,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
}

fn hook(next: &BindingContext, id: &str, path: &Path, event: &str) {
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(next.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let mut payload = serde_json::json!({"session_id":id,"transcript_path":path});
    if event == "session_start" {
        payload["source"] = "startup".into();
    } else {
        payload["prompt"] = "warm proof input".into();
    }
    let ingress =
        crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
            "codex",
            event,
            Some(id),
            Some(id),
            &payload,
            &headers,
        );
    assert!(!ingress.refused(), "{event}: {ingress:?}");
}

fn metadata(next: &BindingContext, id: &str, path: &Path, root: &Path) {
    let at = (next.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let row = serde_json::json!({"type":"session_meta", "timestamp":at,
        "payload":{"id":id,"timestamp":at,"cwd":root,"source":"cli","originator":"codex_cli_rs"}});
    let prompt = serde_json::json!({"type":"response_item", "payload":{
        "type":"message", "role":"user", "content":[{"type":"input_text", "text":"warm proof input"}]}});
    std::fs::write(path, format!("{row}\n{prompt}\n")).unwrap();
}

fn old_harness(source: &SourceId) -> Harness {
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: source.clone(),
            delivery_start: std::fs::metadata(&source.path).unwrap().len(),
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&source.path).unwrap())),
        }]
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    harness
}

#[derive(Clone, Copy)]
enum Case {
    Preproof,
    PendingWarm,
    Alias,
    Eligibility,
}

async fn exercise(case: Case) {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, old) = proven(root.path());
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    let harness = old_harness(&old);
    let old_cursor = harness.channel().cursor(&old).unwrap().clone();
    let mut old_bytes = std::fs::read(&old.path).unwrap();
    match case {
        Case::Eligibility => {
            for policy in ["shadow", "legacy"] {
                let next = next_context(root.path(), policy);
                publish_context(&next);
                let mut installed = false;
                assert_eq!(
                    dedupe::clear_relaunch::with_fresh_scope(
                        CANARY_CHANNEL,
                        None,
                        std::slice::from_ref(&old),
                        true,
                        |_, _| installed = true
                    ),
                    Ok(false),
                    "{policy}"
                );
                assert!(!installed, "{policy} acquired a verified source selection");
            }
            let (_, _) = proven(root.path());
            let mut installed = false;
            assert_eq!(
                dedupe::clear_relaunch::with_fresh_scope(
                    CANARY_CHANNEL + 1,
                    None,
                    std::slice::from_ref(&old),
                    true,
                    |_, _| installed = true
                ),
                Ok(false)
            );
            assert!(!installed);
            let _herdr = crate::config::session_hosts::force_for_test(
                Some("scope-node"),
                &[(CANARY_CHANNEL, "scope-node")],
            );
            assert!(
                crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(
                    CANARY_CHANNEL
                ))
            );
            assert!(
                !dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL),
                "a real Herdr endpoint must close the shared scope eligibility predicate"
            );
            let mut installed = false;
            assert_eq!(
                dedupe::clear_relaunch::with_fresh_scope(
                    CANARY_CHANNEL,
                    None,
                    std::slice::from_ref(&old),
                    true,
                    |_, _| installed = true
                ),
                Ok(false)
            );
            assert!(!installed, "Herdr acquired the direct-tmux scope exception");
        }
        Case::Alias => {
            let next = next_context(root.path(), "verified");
            publish_context(&next);
            let alias_dir = next.provider_root.as_ref().unwrap().join("alias");
            std::fs::create_dir(&alias_dir).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let alias = alias_dir.join(format!("rollout-{id}.jsonl"));
            std::fs::hard_link(&old.path, &alias).unwrap();
            // Model external inode reuse during setup; the admission phase must not read it.
            metadata(&next, &id, &alias, root.path());
            old_bytes = std::fs::read(&old.path).unwrap();
            hook(&next, &id, &alias, "session_start");
            let fold = dedupe::binding_events::codex::read_ownership(&next).unwrap();
            let proof = fold
                .verified
                .expect("the alias must have a real proof before testing the alias guard");
            assert_eq!((proof.source.dev, proof.source.ino), (old.dev, old.ino));
            assert_ne!(proof.source.path, old.path);
            assert!(dedupe::codex_verified_channel_delivery_allowed(
                CANARY_CHANNEL
            ));
            assert!(!dedupe::codex_verified_o_source_allowed(
                CANARY_CHANNEL,
                &old
            ));
            let io =
                crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
            let mut installed = false;
            let result = dedupe::clear_relaunch::with_fresh_scope(
                CANARY_CHANNEL,
                None,
                std::slice::from_ref(&old),
                true,
                |_, _| installed = true,
            );
            assert_eq!(result, Err("source_alias"));
            assert!(!installed);
            assert_eq!(
                io.stats(),
                Default::default(),
                "alias rejection must precede retained native body reads"
            );
        }
        Case::Preproof | Case::PendingWarm => {
            let next = next_context(root.path(), "verified");
            publish_context(&next);
            let id = uuid::Uuid::new_v4().to_string();
            let path = next
                .provider_root
                .as_ref()
                .unwrap()
                .join(format!("rollout-{id}.jsonl"));
            hook(&next, &id, &path, "session_start");
            let pending = dedupe::binding_events::codex::read_ownership(&next).unwrap();
            assert!(pending.verified.is_none());
            assert_eq!(pending.pending.len(), 1);
            let root_seq = pending.pending[0].seq;
            assert_eq!(
                dedupe::binding_events::codex::latest_ownership(CANARY_CHANNEL, CANARY_TMUX)
                    .unwrap()
                    .unwrap()
                    .1,
                next.execution_nonce
            );
            let io =
                crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
            let writer = harness.writer();
            let selection = writer.suspension.clone();
            let (stop, stopped) = watch::channel(false);
            let (resumed, ready) = watch::channel(false);
            let actor = tokio::spawn(run_channel(
                writer,
                ShadowProvider::Codex,
                Arc::new(ChannelBindingLog::new(
                    CANARY_CHANNEL,
                    ShadowProvider::Codex,
                )),
                stopped,
                resumed,
            ));
            polls(4).await;
            let initial_empty = selection.is_empty();
            let initial_nonce = selection.nonce();
            let initial_ready = *ready.borrow();
            let initial_posts = harness.port.posts();
            let progressed = std::panic::AssertUnwindSafe(async {
                if matches!(case, Case::PendingWarm) {
                    metadata(&next, &id, &path, root.path());
                    hook(&next, &id, &path, "session_start");
                    let resolved = dedupe::binding_events::codex::read_ownership(&next)
                        .unwrap()
                        .verified
                        .unwrap();
                    assert_eq!(resolved.ownership.pending_seq, Some(root_seq));
                    assert!(resolved.seq > root_seq);
                    answer(&resolved.source, "resolved first answer");
                    polls(6).await;
                    let before_warm = harness
                        .channel()
                        .rotation()
                        .unwrap()
                        .link(&resolved.source)
                        .unwrap()
                        .seq;
                    // A repeated available-source hook is idempotent; a real Pending retry
                    // advances proof while retaining the exact native file and startup root.
                    let native_bytes = std::fs::read(&path).unwrap();
                    let parked = path.with_extension("warm-pending");
                    std::fs::rename(&path, &parked).unwrap();
                    hook(&next, &id, &path, "user_prompt_submit");
                    let warm_pending = dedupe::binding_events::codex::read_ownership(&next);
                    std::fs::rename(&parked, &path).unwrap();
                    let warm_pending = warm_pending.unwrap();
                    assert!(!warm_pending.conflicted);
                    assert_eq!(warm_pending.pending.len(), 1);
                    assert!(warm_pending.pending[0].seq > resolved.seq);
                    hook(&next, &id, &path, "user_prompt_submit");
                    let warm = dedupe::binding_events::codex::read_ownership(&next)
                        .unwrap()
                        .verified
                        .unwrap();
                    assert!(
                        warm.seq > resolved.seq,
                        "the actual warm claim must advance latest proof"
                    );
                    assert_eq!(warm.source, resolved.source, "same path, session and inode");
                    assert_eq!(
                        warm.ownership.pending_seq,
                        Some(warm_pending.pending[0].seq)
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), native_bytes);
                    answer(&warm.source, "same nonce warm answer");
                    polls(6).await;
                    let after_warm = harness
                        .channel()
                        .rotation()
                        .unwrap()
                        .link(&warm.source)
                        .unwrap()
                        .seq;
                    assert_eq!(
                        before_warm, root_seq,
                        "the root uses Pending's anchor, not its Resolved proof seq"
                    );
                    assert_eq!(
                        after_warm, root_seq,
                        "latest warm proof must not replace the root anchor"
                    );
                    assert!(*ready.borrow());
                    assert_eq!(
                        harness.port.posts(),
                        ["resolved first answer", "same nonce warm answer"]
                    );
                    assert_eq!(
                        selection.nonce().as_deref(),
                        Some(next.execution_nonce.as_str())
                    );
                }
            })
            .catch_unwind()
            .await;
            stop.send(true).unwrap();
            assert_eq!(actor.await.unwrap(), None);
            if let Err(panic) = progressed {
                std::panic::resume_unwind(panic);
            }
            if matches!(case, Case::PendingWarm) {
                restart_after_warm(&harness, &next, root_seq).await;
            }
            assert!(
                initial_empty,
                "an exact current context/marker without proof installed a suspension"
            );
            assert_eq!(
                initial_nonce, None,
                "pre-proof scope nonce must remain absent"
            );
            assert!(!initial_ready);
            assert!(initial_posts.is_empty());
            assert_eq!(
                io.stats().read_bytes,
                0,
                "pre-proof and scoped resume must not read the old body"
            );
        }
    }
    assert_eq!(harness.channel().cursor(&old), Some(&old_cursor));
    assert_eq!(std::fs::read(&old.path).unwrap(), old_bytes);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

async fn restart_after_warm(harness: &Harness, context: &BindingContext, root_seq: u64) {
    let warm = dedupe::binding_events::codex::read_ownership(context)
        .unwrap()
        .verified
        .unwrap();
    assert!(warm.seq > root_seq);
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    assert!(
        selection.is_empty(),
        "a restarted actor must rebuild its own selection"
    );
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let actor = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    let progressed = std::panic::AssertUnwindSafe(async {
        polls(6).await;
        assert!(
            *ready.borrow(),
            "the latest warm proof must restore the scoped actor"
        );
        assert_eq!(
            selection.nonce().as_deref(),
            Some(context.execution_nonce.as_str())
        );
        assert_eq!(
            harness
                .channel()
                .rotation()
                .unwrap()
                .link(&warm.source)
                .unwrap()
                .seq,
            root_seq,
            "restart must retain the Startup Pending anchor rather than latest warm proof seq",
        );
        assert_eq!(
            harness.port.posts(),
            ["resolved first answer", "same nonce warm answer"],
            "restart must not POST either settled current unit again",
        );
        answer(&warm.source, "current answer after warm restart");
        polls(6).await;
        assert_eq!(
            harness.port.posts(),
            [
                "resolved first answer",
                "same nonce warm answer",
                "current answer after warm restart"
            ],
            "the restored warm proof must still admit new current output exactly once",
        );
        assert_eq!(
            harness
                .channel()
                .rotation()
                .unwrap()
                .link(&warm.source)
                .unwrap()
                .seq,
            root_seq,
        );
        assert!(harness.channel().ledger().unresolved().is_none());
    })
    .catch_unwind()
    .await;
    stop.send(true).unwrap();
    assert_eq!(actor.await.unwrap(), None);
    if let Err(panic) = progressed {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(start_paused = true)]
async fn current_context_and_nonce_without_proof_keep_selection_empty_and_old_body_unread() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::current_context_and_nonce_without_proof_keep_selection_empty_and_old_body_unread"
    )) {
        exercise(Case::Preproof).await;
    }
}

#[tokio::test(start_paused = true)]
async fn startup_pending_resolved_anchor_survives_later_same_nonce_warm_proof() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::startup_pending_resolved_anchor_survives_later_same_nonce_warm_proof"
    )) {
        exercise(Case::PendingWarm).await;
    }
}

#[tokio::test(start_paused = true)]
async fn physical_alias_is_rejected_before_any_retained_native_read() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::physical_alias_is_rejected_before_any_retained_native_read"
    )) {
        exercise(Case::Alias).await;
    }
}

#[tokio::test(start_paused = true)]
async fn shadow_legacy_other_channel_and_herdr_do_not_gain_scope_admission() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::shadow_legacy_other_channel_and_herdr_do_not_gain_scope_admission"
    )) {
        exercise(Case::Eligibility).await;
    }
}
