//! The periodic policy uses the same running actor and scoped Apply as the retained tests.

use super::*;
use crate::services::discord::health::o_transition::automatic::skip_key;
use crate::services::tui_o::writer::transition_port::Idle;

fn aged_chain(age: i64, known: bool) -> Chain {
    chain_named(2, true, |n| (format!("h{n}"), aged_row(n, age, known)))
}

fn aged_row(n: usize, age: i64, known: bool) -> Vec<u8> {
    let at = Utc::now() - TimeDelta::days(age);
    let mut value: serde_json::Value =
        serde_json::from_slice(&row(&format!("n{n}"), "old dialogue")).unwrap();
    if known {
        value["timestamp"] = serde_json::json!(at.to_rfc3339());
    }
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

async fn automatic(chain: &Chain, actor: &Run, idle: Idle) -> Reply {
    let request = chain.request("automatic-retention-test");
    automatic_request(chain, actor, request, idle).await
}

async fn automatic_request(chain: &Chain, actor: &Run, request: Request, idle: Idle) -> Reply {
    let inspected = ask(actor, Operation::Inspect(request.clone()), chain.context()).await;
    let digest = inspected.scope.unwrap().unwrap().digest;
    ask(
        actor,
        Operation::AutomaticApply(request, digest, idle),
        chain.context(),
    )
    .await
}

fn idle(value: bool) -> Idle {
    Idle(Arc::new(move || Box::pin(std::future::ready(value))))
}

fn source_skip(chain: &Chain, reply: &Reply, source: usize) -> Option<String> {
    let hop = reply
        .hops
        .iter()
        .find(|hop| hop.judged.key == chain.key(source))
        .expect("the actor reply must include the source");
    skip_key(hop)
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_latest_source() {
    let chain = aged_chain(8, true);
    let current = force_execution_for_tests(Ok(execution(&chain.sources[1], Utc::now())));
    let actor = run(&chain);
    settle(&chain).await;
    automatic(&chain, &actor, idle(true)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:latest] current pin must survive"
    );
    drop(current);
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_source_younger_than_seven_days() {
    let chain = aged_chain(6, true);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let reply = automatic(&chain, &actor, idle(true)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:recent] six days must survive"
    );
    assert_eq!(
        source_skip(&chain, &reply, 1).as_deref(),
        Some("held:Policy(Young)"),
        "[auto:key_young] the runtime reply must distinguish recent dialogue"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_active_turn_or_pending_input() {
    let chain = aged_chain(8, true);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let reply = automatic(&chain, &actor, idle(false)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:active] runtime work must survive"
    );
    assert_eq!(
        source_skip(&chain, &reply, 1).as_deref(),
        Some("held:Policy(NotIdle)"),
        "[auto:key_idle] the runtime reply must distinguish active work"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_unknown_dialogue_age() {
    let chain = aged_chain(8, false);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let reply = automatic(&chain, &actor, idle(true)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:unknown] absent timestamp must survive"
    );
    assert_eq!(
        source_skip(&chain, &reply, 1).as_deref(),
        Some("held:Policy(AgeUnknown)"),
        "[auto:key_unknown] the runtime reply must distinguish unknown dialogue age"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_retires_old_noncurrent_source() {
    let chain = aged_chain(8, true);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let reply = automatic(&chain, &actor, idle(true)).await;
    assert!(
        chain.cursor(1).retired,
        "[auto:old] eligible old source must retire: {:?}",
        reply.hops
    );
    assert_eq!(chain.audit(1).as_deref(), Some("automatic-retention-test"));
    assert!(
        !chain.cursor(2).retired,
        "[auto:current_successor] latest remains readable"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_eventually_retires_old_sources_past_unknown_budget() {
    let chain = chain_named(68, true, |n| (format!("h{n}"), aged_row(n, 8, true)));
    let target = (2..68).max_by_key(|n| chain.key(*n)).unwrap();
    let _current = chain.current();
    let actor = run(&chain);
    polls(chain.hops() as u32 + 40).await;
    assert_eq!(
        chain.harness.port.posts().len() as u64,
        chain.hops(),
        "fixture must finish its output"
    );
    let mut seed = chain.request("automatic-retention-receipt-seed");
    seed.cutoff = 3;
    ask(&actor, chain.apply(seed), chain.context()).await;
    assert!(
        chain.cursor(1).retired,
        "[auto:budget_seed] first job must create a retired receipt"
    );
    assert!(
        (2..68).all(|source| !chain.cursor(source).retired),
        "[auto:budget_seed] cutoff must leave all 66 later sources pending"
    );
    automatic(&chain, &actor, idle(true)).await;
    let retired = (2..68)
        .filter(|source| chain.cursor(*source).retired)
        .count();
    assert!(
        retired == 66 && chain.cursor(target).retired,
        "[auto:receipt_all] one automatic job must retire all 66 pending sources beside a receipt; retired={retired}"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_retires_every_source_from_two_historical_panes() {
    let chain = chain_named(257, true, |n| (format!("h{n}"), aged_row(n, 8, true)));
    let panes = [format!("{}-a", chain.pane), format!("{}-b", chain.pane)];
    for event in chain.bindings.events.lock().unwrap().iter_mut() {
        if event.seq >= 2 {
            let old = event.seq - 2;
            event.tmux_session = panes[usize::from(old > 128)].clone();
        }
    }
    let _current = chain.current();
    let actor = run(&chain);
    polls(chain.hops() as u32 + 40).await;
    assert_eq!(
        chain.harness.port.posts().len() as u64,
        chain.hops(),
        "fixture must finish its output"
    );
    for (pane, job) in panes.iter().zip(["automatic-pane-a", "automatic-pane-b"]) {
        let mut request = chain.request(job);
        let Some(Termination::Confirmed(end)) = &mut request.termination else {
            panic!("fixture must confirm the historical pane");
        };
        end.pane = pane.clone();
        automatic_request(&chain, &actor, request, idle(true)).await;
    }
    let retired = (1..257)
        .filter(|source| chain.cursor(*source).retired)
        .count();
    assert_eq!(
        retired, 256,
        "[auto:panes_all] each historical pane's single automatic job must retire all 128 sources"
    );
    assert!(
        !chain.cursor(257).retired,
        "[auto:panes_all] current source must remain readable"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_uses_the_superseded_nonce_of_a_live_herdr_logical_key() {
    use std::collections::BTreeSet;

    use crate::services::discord::health::o_transition::automatic::plan;
    use crate::services::discord::health::o_transition::automatic::termination::{
        Execution, for_hop,
    };
    use crate::services::session_host::HostKind;
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

    let _root = crate::config::TestRuntimeRootGuard::new();
    let chain = chain_named(4, true, |n| (format!("h{n}"), aged_row(n, 8, true)));
    let current_nonce = "nonce";
    let events = {
        let mut events = chain.bindings.events.lock().unwrap();
        for event in events.iter_mut() {
            event.tmux_session = chain.pane.clone();
            event.execution_nonce = if event.seq < 4 {
                "old-nonce"
            } else {
                current_nonce
            }
            .into();
        }
        events.clone()
    };
    let marker = crate::services::tmux_common::session_temp_path(&chain.pane, "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    assert_eq!(
        read_host_kind_marker(&chain.pane),
        HostKindMarker::Known(HostKind::Herdr)
    );
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;

    let mut request = chain.request("automatic-nonce-discovery");
    request.termination = None;
    let discovery = ask(&actor, Operation::Inspect(request), chain.context()).await;
    let ended_panes = BTreeSet::new();
    let evidence = Execution {
        channel: CHANNEL,
        logical_key: &chain.pane,
        nonce: current_nonce,
        host: "mac-mini",
        ended_panes: &ended_panes,
    };
    for (key, ended) in plan(&discovery, &events, &evidence) {
        if let Ok(end) = ended {
            assert_eq!(end.first_seq, end.last_seq, "one exact hop per retirement");
            let mut request = chain.request(&format!("automatic-nonce-hop-{key}"));
            request.cutoff = end.last_seq;
            request.termination = Some(Termination::Confirmed(end));
            automatic_request(&chain, &actor, request, idle(true)).await;
        }
    }
    assert!(
        chain.cursor(1).retired,
        "[auto:nonce_old] eight-day source of the superseded execution must retire despite the Herdr marker"
    );
    assert!(
        chain.cursor(2).retired,
        "[auto:nonce_boundary] the last source bound by the superseded execution must retire when its successor belongs to the current execution"
    );
    assert!(
        !chain.cursor(3).retired && !chain.cursor(4).retired,
        "[auto:nonce_current] the current execution's noncurrent hop and latest source must remain readable"
    );
    assert!(
        for_hop(&evidence, 99, &chain.pane, &events).is_err(),
        "[auto:nonce_unknown] missing binding evidence must preserve the source"
    );
    let missing_nonce = Execution {
        nonce: "",
        ..evidence
    };
    assert!(
        for_hop(&missing_nonce, 3, &chain.pane, &events).is_err(),
        "[auto:nonce_unknown] missing current nonce must preserve the source"
    );
    let missing_binder: Vec<_> = events
        .iter()
        .filter(|event| event.seq != 2)
        .cloned()
        .collect();
    assert!(
        matches!(
            for_hop(&evidence, 3, &chain.pane, &missing_binder),
            Err("binding_event_missing")
        ),
        "[auto:nonce_binder_missing] the source's missing bind must preserve it"
    );
    let mut duplicate_binder = events.clone();
    duplicate_binder.push(events.iter().find(|event| event.seq == 2).unwrap().clone());
    assert!(
        matches!(
            for_hop(&evidence, 3, &chain.pane, &duplicate_binder),
            Err("binding_event_unknown")
        ),
        "[auto:nonce_binder_duplicate] two binders at the latest sequence are ambiguous"
    );

    let mut pending = events.clone();
    for event in pending.iter_mut().filter(|event| event.seq >= 3) {
        event.seq += 1;
    }
    let mut resolved = {
        let binding = pending.iter_mut().find(|event| event.seq == 2).unwrap();
        let BindingRecord::Bound { new, .. } = &mut binding.record else {
            panic!("fixture source must have a Bound record");
        };
        *new = BindingTarget::Pending {
            payload_session_id: chain.sources[1].session_id.clone(),
            payload_transcript_path: chain.paths[1].clone(),
        };
        binding.clone()
    };
    resolved.seq = 3;
    resolved.execution_nonce = current_nonce.into();
    resolved.record = BindingRecord::Resolved {
        resolves_seq: 2,
        source: chain.sources[1].clone(),
    };
    pending.insert(2, resolved.clone());
    assert!(
        for_hop(&evidence, 4, &chain.pane, &pending).is_ok(),
        "[auto:nonce_pending] the Pending bind's old nonce owns the source even when Resolved has the current nonce"
    );
    let mut duplicate_resolution = pending.clone();
    for event in duplicate_resolution
        .iter_mut()
        .filter(|event| event.seq >= 4)
    {
        event.seq += 1;
    }
    resolved.seq = 4;
    duplicate_resolution.insert(3, resolved);
    assert!(
        matches!(
            for_hop(&evidence, 5, &chain.pane, &duplicate_resolution),
            Err("binding_event_unknown")
        ),
        "[auto:nonce_resolution_duplicate] two resolutions of one Pending bind are ambiguous"
    );

    let mut rebound = events.clone();
    let mut resume = events.iter().find(|event| event.seq == 2).unwrap().clone();
    resume.seq = 6;
    resume.execution_nonce = current_nonce.into();
    let BindingRecord::Bound { old, cause, .. } = &mut resume.record else {
        panic!("fixture resume must be Bound");
    };
    *old = Some(chain.sources[1].clone());
    *cause = BindingCause::Resume;
    rebound.push(resume);
    let mut later_hop = events.iter().find(|event| event.seq == 3).unwrap().clone();
    later_hop.seq = 7;
    later_hop.execution_nonce = current_nonce.into();
    rebound.push(later_hop);
    assert!(
        matches!(
            for_hop(&evidence, 7, &chain.pane, &rebound),
            Err("current_execution")
        ),
        "[auto:nonce_rebound] the latest same-source resume, with unique sequences, belongs to the current execution"
    );
    halt(actor.stop, actor.task).await;
}
