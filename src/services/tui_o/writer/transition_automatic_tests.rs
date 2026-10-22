//! The periodic policy uses the same running actor and scoped Apply as the retained tests.

use super::*;
use crate::services::tui_o::writer::transition_port::Idle;

fn aged_chain(age: i64, known: bool) -> Chain {
    let at = Utc::now() - TimeDelta::days(age);
    chain_named(2, true, |n| {
        let mut value: serde_json::Value =
            serde_json::from_slice(&row(&format!("n{n}"), "old dialogue")).unwrap();
        if known {
            value["timestamp"] = serde_json::json!(at.to_rfc3339());
        }
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        (format!("h{n}"), bytes)
    })
}

async fn automatic(chain: &Chain, actor: &Run, idle: Idle) -> Reply {
    let request = chain.request("automatic-retention-test");
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
    automatic(&chain, &actor, idle(true)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:recent] six days must survive"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_active_turn_or_pending_input() {
    let chain = aged_chain(8, true);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    automatic(&chain, &actor, idle(false)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:active] runtime work must survive"
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_retention_keeps_unknown_dialogue_age() {
    let chain = aged_chain(8, false);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    automatic(&chain, &actor, idle(true)).await;
    assert!(
        !chain.cursor(1).retired,
        "[auto:unknown] absent timestamp must survive"
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
