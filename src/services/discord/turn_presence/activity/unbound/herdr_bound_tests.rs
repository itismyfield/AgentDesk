use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, tests::record};
use crate::services::discord::turn_presence::activity::tests::{
    bind_turn_mode_transcript, settled_reason,
};
use crate::services::discord::turn_presence::activity::unbound::live::review_tests::Rig;
use crate::services::discord::{mailbox_snapshot, mailbox_take_next_automatic_intervention};
use crate::services::tui_prompt_dedupe::{admit_herdr_execution, withhold_herdr_execution};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{MessageId, UserId};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    Admitted,
    Open,
    Withheld,
    Pending,
    OtherNode,
    OtherExecutionNode,
    Retired,
    Missing,
}

fn queued(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: Vec::new(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// The restarted channel's hosted row for `case`, as the reconnect pass leaves it.
async fn seed_row(rig: &Rig, name: &str, case: Case) {
    if case == Case::Missing {
        return;
    }
    let endpoint = crate::config::session_hosts::herdr_endpoint(rig.channel.get()).unwrap();
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: rig.shared.token_hash.clone(),
        channel_id: rig.channel.to_string(),
        logical_key: name.into(),
        owner_node: if case == Case::OtherNode {
            "other-node"
        } else {
            "test-node"
        }
        .into(),
        runtime_root: rig.root.path().display().to_string(),
    };
    let state = match case {
        Case::Pending => HostedState::Pending,
        Case::Retired => HostedState::Retired,
        _ => HostedState::Bound,
    };
    let mut execution = record(&owner, "restarted", HostedState::Bound);
    execution.state = state;
    execution.location = Some(HostedLocation {
        host: "herdr".into(),
        execution_node: match case {
            Case::OtherExecutionNode => "other-node".into(),
            _ => endpoint.execution_node.clone(),
        },
        endpoint_config_key: endpoint.key.clone(),
        socket_addr: endpoint.socket_path.display().to_string(),
        named_session: endpoint.herdr_session.clone(),
        pane_id: "p-1".into(),
    });
    let key = adk_session::build_namespaced_session_key(
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        name,
    );
    crate::services::discord::inflight::seed_session_row_hashed(
        rig.shared.pg_pool.as_ref().unwrap(),
        &key,
        rig.channel.get(),
        &rig.shared.token_hash,
        Some(serde_json::to_value(execution).unwrap()),
    )
    .await;
}

/// A restarted Herdr channel with no watcher yet, a queued head and the transcript `case` leaves;
/// returns the reason presence settles on and whether the queue gate took the head.
async fn drained(id: u64, case: Case) -> (&'static str, bool) {
    let rig = Rig::new(id, true, false).await;
    let channel = rig.channel;
    // A restart restores no core session for a Herdr channel; its configured name is the basis.
    rig.shared.core.lock().await.sessions.remove(&channel);
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("s1e-{id}"));
    seed_row(&rig, &name, case).await;
    match case {
        Case::Withheld => withhold_herdr_execution(&name, Some("restarted")),
        _ => admit_herdr_execution(&name, "restarted"),
    }
    let provider = ProviderKind::Claude;
    // A retired or never created run leaves no binding; were it judged Bound it would read no turn.
    let _turn = if matches!(case, Case::Retired | Case::Missing) {
        crate::services::tui_o::turn_mode::TestConfirmation::new(id)
    } else {
        let path = rig.root.path().join("parent.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","uuid":"a","message":{{"content":"turn"}}}}"#
        )
        .unwrap();
        if case != Case::Open {
            writeln!(file, r#"{{"type":"system","subtype":"turn_duration"}}"#).unwrap();
        }
        let turn = bind_turn_mode_transcript(&rig.shared, &provider, id, &name, &path);
        assert!(rig.shared.tmux_watchers.remove(&channel).is_some());
        turn
    };
    let reason = settled_reason(&rig.shared, &provider, id).await;
    let context =
        crate::services::discord::queue_persistence_context(&rig.shared, &provider, channel);
    let head = id + 1_000_000;
    rig.shared
        .mailbox(channel)
        .replace_queue(vec![queued(head), queued(head + 1)], context)
        .await;
    let take = mailbox_take_next_automatic_intervention(&rig.shared, &provider, channel).await;
    let taken = take.intervention.as_ref().map(|i| i.message_id.get());
    assert!(
        taken.is_none_or(|taken| taken == head),
        "{case:?}: took {taken:?}"
    );
    let left = mailbox_snapshot(&rig.shared, channel)
        .await
        .intervention_queue
        .len();
    assert_eq!(left, if taken.is_some() { 1 } else { 2 }, "{case:?}");
    (reason, taken.is_some())
}

// After a restart, a Herdr channel's admitted Bound run is judged on its own transcript before any
// watcher claims it: a closed turn drains the queue, an open one keeps it, and every row the
// reconnect did not admit as this node's own Bound execution keeps the Unbound answer.
#[tokio::test(flavor = "current_thread")]
async fn restarted_herdr_channel_drains_only_an_admitted_bound_run_without_a_watcher_pg() {
    let cases = [
        (Case::Admitted, "closed", true),
        (Case::Open, "open", false),
        (Case::Withheld, "session_identity_unresolved", false),
        (Case::Pending, "session_identity_unresolved", false),
        (Case::OtherNode, "session_identity_unresolved", false),
        (
            Case::OtherExecutionNode,
            "session_identity_unresolved",
            false,
        ),
        (Case::Retired, "no_session", true),
        (Case::Missing, "no_session", true),
    ];
    let mut wrong = Vec::new();
    for (index, (case, reason, taken)) in cases.into_iter().enumerate() {
        let observed = drained(71_340_100 + index as u64, case).await;
        if observed != (reason, taken) {
            wrong.push((case, observed));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}
