use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, tests::record,
};
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
    seed_row_with(rig, name, case, "restarted", |_| {}).await;
}

/// `seed_row` of execution `nonce`, edited by `change` before it is stored.
async fn seed_row_with(
    rig: &Rig,
    name: &str,
    case: Case,
    nonce: &str,
    change: impl FnOnce(&mut HostedExecution),
) {
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
    let mut execution = record(&owner, nonce, HostedState::Bound);
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
    change(&mut execution);
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

/// Registers the channel by id alone, live and on disk, as a config without name or aliases does.
fn unname(rig: &Rig, id: u64) {
    let yaml = format!(
        "server: {{}}\ncluster: {{instance_id: test-node}}\nagents:\n  - id: fixture\n    name: fixture\n    provider: claude\n    channels:\n      claude: {{id: '{id}', runtime: tui}}\n"
    );
    let mut config: crate::config::Config = serde_yaml::from_str(&yaml).unwrap();
    config.session_hosts =
        crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
    crate::config_live_reload::install(config.clone());
    let path = crate::runtime_layout::config_file_path(rig.root.path());
    std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
}

/// A restarted Herdr channel with no watcher yet, a queued head and the transcript `case` leaves;
/// returns the reason presence settles on and whether the queue gate took the head.
async fn drained(id: u64, case: Case, named: bool) -> (&'static str, bool) {
    let rig = Rig::new(id, true, false).await;
    let channel = rig.channel;
    // A restart restores no core session for a Herdr channel.
    rig.shared.core.lock().await.sessions.remove(&channel);
    let basis = if named { "s1e" } else { "unnamed" };
    if !named {
        unname(&rig, id);
    }
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("{basis}-{id}"));
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
    settle_and_take(&rig, case).await
}

/// The reason presence settles on for the rig's channel, then whether the queue gate took the head
/// of a two-item queue; the second item always stays.
async fn settle_and_take(rig: &Rig, label: impl std::fmt::Debug) -> (&'static str, bool) {
    let (provider, channel, id) = (ProviderKind::Claude, rig.channel, rig.channel.get());
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
        "{label:?}: took {taken:?}"
    );
    let left = mailbox_snapshot(&rig.shared, channel)
        .await
        .intervention_queue
        .len();
    assert_eq!(left, if taken.is_some() { 1 } else { 2 }, "{label:?}");
    (reason, taken.is_some())
}

// Before a watcher claims it, only this node's admitted Bound Herdr run is judged on its transcript;
// every other row keeps the Unbound answer.
#[tokio::test(flavor = "current_thread")]
async fn restarted_herdr_channel_drains_only_an_admitted_bound_run_without_a_watcher_pg() {
    let cases = [
        (Case::Admitted, true, "closed", true),
        (Case::Open, true, "open", false),
        (Case::Withheld, true, "session_identity_unresolved", false),
        (Case::Pending, true, "session_identity_unresolved", false),
        (Case::OtherNode, true, "session_identity_unresolved", false),
        (
            Case::OtherExecutionNode,
            true,
            "session_identity_unresolved",
            false,
        ),
        (Case::Retired, true, "no_session", true),
        (Case::Missing, true, "no_session", true),
        (Case::Admitted, false, "closed", true),
        (Case::Open, false, "open", false),
        (Case::Withheld, false, "session_identity_unresolved", false),
    ];
    let mut wrong = Vec::new();
    for (index, (case, named, reason, taken)) in cases.into_iter().enumerate() {
        let observed = drained(71_340_100 + index as u64, case, named).await;
        if observed != (reason, taken) {
            wrong.push((case, named, observed));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

#[path = "herdr_chain_tests.rs"]
mod chain;
