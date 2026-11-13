//! The observed busy caller reaches the shared host operation without a launch or database query.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedObservation;
use crate::services::claude_tui::host_input::busy::observed;
use crate::services::turn_host::HerdrTurnPlan;

/// A managed caller already observed this Bound pane busy; its actual front handoff must use the
/// common operation once and settle at Enter while the native transcript remains busy.
#[test]
fn an_observed_busy_herdr_front_uses_the_common_operation_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 8_114_601);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let mut busy = claude_ready();
    let screen = busy["read"]["text"].as_str().unwrap().to_string();
    busy["read"]["text"] = screen
        .replace("✻ Baked for 2s", "✻ Thinking… (12s · esc to interrupt)")
        .into();
    owner.rig.answer("pane.read", busy);
    let native = uuid::Uuid::new_v4().to_string();
    let transcript = owner._dir.path().join(format!("{native}.jsonl"));
    std::fs::write(
        &transcript,
        concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n"
        ),
    )
    .unwrap();
    let record = owner.record("claude", HostedState::Bound);
    let host = TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: owner.endpoint(),
        row: Some(HostedObservation::known_fixture(record)),
    }));
    let _entered = owner.runtime.enter();
    let live = (transcript.to_str().unwrap(), Some(native.as_str()));
    let registration = Registration::observed(
        owner.mailbox.clone(),
        ("claude", InputOrigin::Discord, true),
        (&host, owner.logical.as_str(), Some(live)),
        &owner.original,
        None,
    )
    .unwrap();
    assert_eq!(registration.phase, target::Phase::Busy);
    assert_eq!(registration.attempt["source"]["kind"], "existing");
    assert_eq!(registration.attempt["source"]["session_id"], native);
    assert_eq!(
        registration.attempt["target"]["execution_nonce"],
        owner.execution
    );
    let source = registration.attempt["source"].clone();
    let target = registration.attempt["target"].clone();
    let nonce = registration.attempt["nonce"].as_str().unwrap().to_string();
    let frame = busy_inject::frame(
        "discord",
        &registration.author,
        &registration.name,
        &nonce,
        PROMPT,
    );
    assert_eq!(observed(&nonce), 0);
    let front = owner
        .runtime
        .block_on(owner.mailbox.v2_request(|slot| {
            let actor = slot.as_mut().ok_or("no owner")?;
            let front = actor.front().ok_or("no front")?;
            actor.grant_front(&front)?;
            Ok(front)
        }))
        .unwrap();
    let claim = owner
        .runtime
        .block_on(claim_turn(&owner.mailbox, front.1.message_id, &front))
        .unwrap()
        .expect("the observed caller claims the granted front");
    assert_eq!(
        owner.runtime.block_on(owner.mailbox.has_active_turn()),
        Ok(false)
    );
    let heads = owner.heads_at_send();
    let pool = unused_pool(&owner);
    let provider = ProviderKind::Claude;
    let turn = StreamingTurn {
        pool: Some(&pool),
        shared: None,
        provider: &provider,
        prompt: PROMPT,
        session_id: Some(&native),
        working_dir: owner._dir.path().to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        cancel: Arc::new(CancelToken::new()),
        codex_observation: None,
        tmux_session_name: Some(&owner.logical),
        teardown: None,
        host: &host,
        channel_id: owner.channel,
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
    let (settled, completion) = tokio::sync::oneshot::channel();
    let (sender, _receiver) = std::sync::mpsc::channel();
    assert_eq!(
        execute_front(turn, sender, claim, registration, settled),
        Ok(())
    );
    assert_eq!(owner.runtime.block_on(completion).unwrap(), Ok(()));
    assert_eq!(
        observed(&nonce),
        1,
        "the actual caller uses the host boundary"
    );
    owner.delivered_once(&frame);
    let heads = heads.lock().unwrap();
    assert_eq!(heads.len(), 2, "one paste and one Enter reached the pane");
    for head in heads.iter() {
        assert_eq!(head["attempt"]["source"], source);
        assert_eq!(head["attempt"]["target"], target);
    }
    assert_eq!(pool.size(), 0, "the busy path queried no database");
    assert!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript).is_busy(),
        "the input settled without the native turn ending"
    );
}
