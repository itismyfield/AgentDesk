//! The Codex observation branch reaches the real actor mailbox without acquiring Apply rights.

use super::*;
use crate::services::tmux_common::TmuxSourceAuthority;
use crate::services::tui_o::writer::codex_snapshot::{
    Coverage, HostPath, Inputs, Observation, Observer, Pane, PaneState, Target,
};
use crate::services::tui_o::writer::transition_cleanup::scope::codex::identity;
use crate::services::tui_o::writer::transition_port::Idle;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events as p5;

const NONCE: &str = "11111111111111111111111111111111";

#[derive(Default)]
struct Delta {
    tail: bool,
    context: bool,
    census: bool,
}

struct Observed {
    pane: String,
    sources: Vec<SourceId>,
    at: DateTime<Utc>,
    delta: std::sync::Mutex<Delta>,
}

impl Observer for Observed {
    fn observe(&self, _authority: &TmuxSourceAuthority<'_>) -> Result<Inputs, String> {
        let delta = self.delta.lock().unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: self.at,
            execution_nonce: NONCE.into(),
            tmux_session: self.pane.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: "/owner".into(),
            host: Some("mac-mini".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: self.sources[0].path.parent().map(Path::to_path_buf),
            first_prompt_digest: Some(if delta.context { "changed" } else { "launch" }.into()),
            source_policy: Some("shadow".into()),
        };
        let event = |seq, pane: &str, old, new| p5::BindingEvent {
            seq,
            channel_id: CHANNEL,
            provider: "codex".into(),
            tmux_session: pane.into(),
            execution_nonce: Some(NONCE.into()),
            old,
            new: p5::BindingTarget::Source(new),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: self.at,
            },
            committed_at: self.at + TimeDelta::seconds(seq as i64),
        };
        let mut events: Vec<_> = self
            .sources
            .windows(2)
            .enumerate()
            .map(|(index, pair)| {
                event(
                    index as u64 + 1,
                    &self.pane,
                    Some(pair[0].clone()),
                    pair[1].clone(),
                )
            })
            .collect();
        let mut panes = vec![Pane {
            name: self.pane.clone(),
            state: PaneState::Live,
        }];
        if delta.tail {
            events.push(event(
                events.len() as u64 + 1,
                "historical-other-pane",
                None,
                self.sources[0].clone(),
            ));
            panes.push(Pane {
                name: "historical-other-pane".into(),
                state: PaneState::Dead,
            });
        }
        Ok(Inputs {
            target: Target {
                channel: CHANNEL,
                tmux: self.pane.clone(),
                nonce: NONCE.into(),
                host: "mac-mini".into(),
                owner_runtime_root: "/owner".into(),
                host_path: HostPath::Legacy,
            },
            context,
            current: self.sources.last().unwrap().clone(),
            tail: events.len() as u64,
            events,
            log_digest: if delta.tail {
                "full-tail-3"
            } else {
                "full-tail-2"
            }
            .into(),
            census_digest: if delta.census { "census-2" } else { "census-1" }.into(),
            panes,
            coverage: Coverage::Complete,
        })
    }
}

fn codex_chain() -> Chain {
    let chain = chain_named(2, false, |number| {
        let row = serde_json::json!({"type":"response_item", "payload": {
            "type":"message", "role":"assistant", "id":format!("msg_{number}"),
            "content":[{"type":"output_text", "text":format!("hop {number}")}]
        }});
        (format!("h{number}"), codex_line(row))
    });
    for event in chain.bindings.events.lock().unwrap().iter_mut() {
        event.provider = ShadowProvider::Codex;
    }
    chain
}

fn observed(chain: &Chain) -> Arc<Observed> {
    Arc::new(Observed {
        pane: chain.pane.clone(),
        sources: chain.sources.clone(),
        at: Utc::now() - TimeDelta::days(1),
        delta: std::sync::Mutex::default(),
    })
}

fn context(chain: &Chain, observer: &Arc<Observed>) -> Context {
    Context {
        tmux: chain.pane.clone(),
        nonce: NONCE.into(),
        lock: Arc::default(),
        codex: Some(observer.clone()),
    }
}

fn inspected(
    reply: &Reply,
) -> (
    &crate::services::tui_o::writer::codex_snapshot::Snapshot,
    String,
) {
    assert_eq!(reply.refused, None, "Codex Inspect reached its actor");
    assert_eq!(reply.operation, Some(Kind::Inspect));
    let diagnostics = reply
        .diagnostics
        .as_ref()
        .expect("the actor returned its snapshot");
    let observation = diagnostics.codex.as_ref().expect("typed Codex diagnostic");
    let Observation::Complete(snapshot) = observation else {
        panic!("the known complete fixture was not Complete: {observation:?}");
    };
    let shown = diagnostics.observation_id.clone().unwrap().unwrap();
    assert_eq!(shown, identity(observation).unwrap());
    (snapshot, shown)
}

#[tokio::test(start_paused = true)]
async fn bo_codex_inspect_returns_complete_but_automatic_apply_keeps_the_backlog() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let chain = codex_chain();
    let observer = observed(&chain);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let before = files(&chain.dir());
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request("bo-inspect")),
        context(&chain, &observer),
    )
    .await;
    let (snapshot, _) = inspected(&reply);
    assert_eq!(snapshot.current, *chain.sources.last().unwrap());
    assert_eq!(snapshot.held, chain.sources[1..]);
    assert_eq!(reply.execution, Some(Ok(chain.sources[1..].to_vec())));
    assert_eq!(snapshot.tail, 2);
    assert_eq!(reply.hops.len(), 2);
    assert!(reply.hops.iter().all(|hop| {
        hop.judged.verdict == Verdict::Refused(Refusal::CodexDeferred)
            && hop.outcome == Outcome::Unchanged
    }));
    let automatic = Operation::AutomaticApply(
        chain.request("bo-automatic"),
        ScopeDigest::parse(UNSCOPED).unwrap(),
        Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
    );
    let reply = ask(&actor, automatic, context(&chain, &observer)).await;
    assert_eq!(reply.refused, Some(Refused::CodexDeferred));
    assert_eq!(files(&chain.dir()), before, "BO did not write the store");
    assert!((0..2).all(|number| !chain.cursor(number).retired && chain.audit(number).is_none()));
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn bo_codex_identity_observes_full_tail_context_and_census_without_writing() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let chain = codex_chain();
    let observer = observed(&chain);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let before = files(&chain.dir());
    let mut ids = std::collections::BTreeSet::new();
    for case in 0..4 {
        *observer.delta.lock().unwrap() = Delta {
            tail: case == 1,
            context: case == 2,
            census: case == 3,
        };
        let request = chain.request(&format!("bo-identity-{case}"));
        let reply = ask(
            &actor,
            Operation::Inspect(request),
            context(&chain, &observer),
        )
        .await;
        let (snapshot, shown) = inspected(&reply);
        assert_eq!(snapshot.current, *chain.sources.last().unwrap());
        assert_eq!(snapshot.held, chain.sources[1..]);
        assert!(
            ids.insert(shown),
            "the changed component must move the observation identity: {case}"
        );
        assert_eq!(files(&chain.dir()), before);
    }
    halt(actor.stop, actor.task).await;
}
