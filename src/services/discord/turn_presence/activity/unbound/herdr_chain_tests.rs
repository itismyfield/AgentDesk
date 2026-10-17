//! A restarted Herdr channel through the restart pass rehydration runs: only the pass's own
//! reconnect of the Bound pane admits the run whose transcript then lets the queue gate drain.

use super::*;
use crate::services::discord::recovery_engine::herdr_reader::{
    HerdrRig, ReconnectCounts, reconnect_counts, reconnect_restarted_herdr_panes,
};
use crate::services::discord::turn_presence::activity::tests::{BindingRoot, log_turn_source};
use crate::services::session_host::herdr_socket_rig_tests::PANE;

const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// The restart pass as rehydration runs it for Claude, on a new thread as a restarted process has
/// no earlier pass; the thread carries this node's hosts, the binding log root and the rig as the
/// channel's configured endpoint.
async fn restart_pass(rig: &Rig, herdr: HerdrRig) -> ReconnectCounts {
    let (id, pool) = (rig.channel.get(), rig.shared.pg_pool.clone());
    let root = rig.root.path().to_owned();
    let runtime = tokio::runtime::Handle::current();
    let pass = move || {
        let _runtime = runtime.enter();
        let _binding = BindingRoot::enter(&root);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some("test-node"), &[(id, "test-node")]);
        let at = crate::config::session_hosts::herdr_endpoint(id).unwrap();
        let socket = at.socket_path.as_path();
        let node = at.execution_node.as_str();
        let _registry = herdr.registry_as_on_this_thread(node, &at.key, socket, &at.herdr_session);
        reconnect_restarted_herdr_panes(pool.as_ref(), &ProviderKind::Claude);
        reconnect_counts()
    };
    let joined = tokio::task::spawn_blocking(|| std::thread::spawn(pass).join());
    joined.await.unwrap().unwrap()
}

/// A restarted channel as a restart leaves it: its Bound row and the pane still running that
/// execution, the source the launch logged, a queued head, and no watcher, core session or
/// admission. Runs the restart pass when `reconnect`, then settles presence and the queue gate.
async fn restarted(
    id: u64,
    reconnect: bool,
    closed: bool,
) -> (ReconnectCounts, &'static str, bool) {
    let rig = Rig::new(id, true, false).await;
    rig.shared.core.lock().await.sessions.remove(&rig.channel);
    let provider = ProviderKind::Claude;
    let name = provider.build_tmux_session_name(&format!("s1e-{id}"));
    let herdr = HerdrRig::start();
    herdr.run_provider(&herdr.context(NONCE), false);
    herdr.show_panes(&[PANE]);
    let expected = herdr.expected(NONCE);
    seed_row_with(&rig, &name, Case::Admitted, NONCE, |execution| {
        execution.location.as_mut().unwrap().pane_id = PANE.into();
        execution.expected = Some(expected);
    })
    .await;
    let path = rig.root.path().join("parent.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"type":"user","uuid":"a","message":{{"content":"turn"}}}}"#
    )
    .unwrap();
    if closed {
        writeln!(file, r#"{{"type":"system","subtype":"turn_duration"}}"#).unwrap();
    }
    log_turn_source(&provider, id, &name, NONCE, &path);
    // The launch's host marker, which a restart leaves on disk.
    let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
    std::fs::write(
        marker,
        crate::services::session_host::HostKind::Herdr.as_str(),
    )
    .unwrap();
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
    assert!(
        crate::services::tui_prompt_dedupe::herdr_claim_admission(&name, true).is_err(),
        "nothing admits the run before the pass"
    );
    let counts = match reconnect {
        true => restart_pass(&rig, herdr).await,
        false => ReconnectCounts::default(),
    };
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_none()
    );
    let (reason, taken) = settle_and_take(&rig, (reconnect, closed)).await;
    (counts, reason, taken)
}

// After a restart only the pass's own reconnect admits the Bound run: the same channel without
// it stays held, and an open turn stays held after it.
#[tokio::test(flavor = "current_thread")]
async fn a_restarted_herdr_channel_drains_its_queue_only_after_the_restart_pass_reconnects_it_pg() {
    let published = ReconnectCounts {
        channels: 1,
        published: 1,
        ..ReconnectCounts::default()
    };
    let none = ReconnectCounts::default();
    let cases = [
        (true, true, (published, "closed", true)),
        (false, true, (none, "session_identity_unresolved", false)),
        (true, false, (published, "open", false)),
    ];
    let mut wrong = Vec::new();
    for (index, (reconnect, closed, expected)) in cases.into_iter().enumerate() {
        let observed = restarted(71_340_200 + index as u64, reconnect, closed).await;
        if observed != expected {
            wrong.push((reconnect, closed, observed));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}
