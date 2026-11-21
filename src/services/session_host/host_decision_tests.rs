use super::*;
use crate::config::session_hosts::force_for_test as force_hosts;
use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};
use crate::services::session_host::thread_host::{Execution, current_execution_cached};
use crate::services::turn_host::tests::{bound_on_forced_endpoint, seed_channel};

const TOKEN: &str = "discord_0123456789abcdef";
/// The tmux tail of the shared test key the seeded rows use.
const TMUX: &str = "AgentDesk-claude-hosted";

fn held(decision: &HostDecision) -> bool {
    matches!(decision, HostDecision::Hold(_))
}

// A channel's own boot entry, a plain channel and a thread under a tmux parent are decided as the
// old exact boot read decided them, with no row or tmux read and with the same refusal text.
#[tokio::test]
async fn boot_and_plain_channels_and_tmux_threads_decide_as_before_unread() {
    let (boot, plain, parent, tmux_child) = (8_118_400, 8_118_401, 8_118_402, 8_118_403);
    let _hosts = force_hosts(Some("node"), &[(boot, "node")]);
    let _tmux = force_relation_for_test(tmux_child, Relation::Child { parent });
    force_presence_for_test(TMUX, SessionPresence::ProbeFailed);
    let probes = forced_probes(TMUX);
    let decided = host_decision(None, None, None, boot).await;
    assert_eq!(
        decided,
        HostDecision::Herdr {
            endpoint_key: "node-endpoint".into(),
            execution_node: "node".into()
        }
    );
    assert_eq!(
        decided.tmux_refusal().as_deref(),
        Some("Herdr 설정 채널(endpoint `node-endpoint`)이라 tmux 세션을 바꾸지 않아요")
    );
    for channel in [plain, tmux_child] {
        let decided = host_decision(None, None, None, channel).await;
        assert_eq!(decided, HostDecision::Tmux, "{channel}");
        assert_eq!(decided.tmux_refusal(), None);
    }
    assert_eq!(forced_probes(TMUX), probes, "no tmux probe");
}

// An unresolved thread, a thread under a Herdr parent without a provider, and one whose rows
// cannot be read are held: neither host is guessed.
#[tokio::test]
async fn an_unresolved_thread_an_unknown_provider_and_an_unread_thread_hold() {
    let (parent, child, unresolved) = (8_118_410, 8_118_411, 8_118_412);
    let _hosts = force_hosts(Some("node"), &[(parent, "node")]);
    let _child = force_relation_for_test(child, Relation::Child { parent });
    let _held = force_relation_for_test(unresolved, Relation::Unresolved);
    let claude = Some(&ProviderKind::Claude);
    assert_eq!(
        host_decision(None, Some(TOKEN), claude, unresolved).await,
        HostDecision::Hold(HoldReason::RelationUnresolved)
    );
    assert_eq!(
        host_decision(None, Some(TOKEN), None, child).await,
        HostDecision::Hold(HoldReason::ProviderUnknown)
    );
    let unread = host_decision(None, Some(TOKEN), claude, child).await;
    assert!(held(&unread), "{unread:?}");
    assert!(
        unread
            .tmux_refusal()
            .unwrap()
            .starts_with("실행 호스트를 확인하지 못해 tmux 세션을 바꾸지 않아요(")
    );
    assert!(!input_runs_herdr(None, TOKEN, &ProviderKind::Claude, child).await);
}

// A thread under a Herdr parent is decided from its row and tmux on every call: live tmux is tmux
// with nothing judged, a failed probe holds, a Bound pane at the parent's endpoint is Herdr.
#[tokio::test(flavor = "multi_thread")]
async fn a_thread_under_a_herdr_parent_is_decided_by_its_execution_now_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (parent, child) = (8_118_420, 8_118_421);
    let _hosts = force_hosts(Some("test-node"), &[(parent, "test-node")]);
    let _child = force_relation_for_test(child, Relation::Child { parent });
    let claude = ProviderKind::Claude;
    let decide = async |provider: &ProviderKind| {
        host_decision(Some(&pool), Some(TOKEN), Some(provider), child).await
    };
    let input = async || input_runs_herdr(Some(&pool), TOKEN, &claude, child).await;

    assert_eq!(decide(&claude).await, HostDecision::Tmux, "no sessions row");
    seed_channel(&pool, child, None).await;
    force_presence_for_test(TMUX, SessionPresence::Present);
    assert_eq!(
        current_execution_cached(child),
        Execution::Unknown,
        "nothing judged"
    );
    assert_eq!(
        decide(&claude).await,
        HostDecision::Tmux,
        "a live tmux thread"
    );
    force_presence_for_test(TMUX, SessionPresence::Missing);
    assert_eq!(decide(&claude).await, HostDecision::Tmux, "nothing runs");
    force_presence_for_test(TMUX, SessionPresence::ProbeFailed);
    let unknown = decide(&claude).await;
    assert!(held(&unknown), "{unknown:?}");
    assert!(!input().await);
    assert_eq!(forced_probes(TMUX), 1, "the input decision probes no tmux");

    seed_channel(&pool, child, bound_on_forced_endpoint(child, "test-node")).await;
    assert_eq!(
        decide(&claude).await,
        HostDecision::Herdr {
            endpoint_key: "test-node-endpoint".into(),
            execution_node: "test-node".into()
        }
    );
    assert!(input().await);
    assert_eq!(
        decide(&ProviderKind::Codex).await,
        HostDecision::Tmux,
        "a Codex thread has no row of its own here"
    );
    seed_channel(&pool, child, bound_on_forced_endpoint(child, "other-node")).await;
    assert!(
        held(&decide(&claude).await),
        "a pane off the parent's endpoint"
    );
    seed_channel(&pool, child, Some(serde_json::json!({"schema": 99}))).await;
    assert!(held(&decide(&claude).await), "an unreadable record");
    pool.close().await;
    db.drop().await;
}
