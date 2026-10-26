use super::super::{MigrationRow, RowSource, binding_digest, select};
use super::*;
use crate::config::session_hosts::approval_store::{
    ApprovalStore, FAIL_CURRENT_SYNC, FileApprovalStore,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLocation, HostedRecord, HostedState,
    tests::{owner, record},
};
use crate::services::session_host::ServerWitness;
use crate::services::session_host::policy::consent::{
    ConsentKey, IntentRef, Preview, Question, settlement::Resolution,
};

struct Rows(bool);
impl RowSource for Rows {
    async fn rows(&self, _: &BTreeSet<u64>) -> Result<Vec<MigrationRow>, String> {
        let mut bound = record(&owner("41"), "return-execution", HostedState::Bound);
        bound.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: "node".into(),
            endpoint_config_key: "node-endpoint".into(),
            socket_addr: "/adk/herdr/agentdesk.sock".into(),
            named_session: "agentdesk".into(),
            pane_id: "pane-1".into(),
        });
        let row = MigrationRow {
            id: 7,
            channel: 41,
            record: HostedRecord::Known(bound),
        };
        Ok(if self.0 { vec![row] } else { Vec::new() })
    }
}

fn hosts(mode: EndpointMode, socket: &str) -> BootSessionHosts {
    let forced =
        crate::config::session_hosts::force_for_test(Some("node"), &[(41, "node")]).with_mode(mode);
    let mut hosts = crate::config::session_hosts::with_boot(|boot| boot.unwrap().clone());
    drop(forced);
    hosts.channels.get_mut(&41).unwrap().socket_path = socket.into();
    let endpoint = hosts
        .config
        .herdr
        .endpoints
        .get_mut("node-endpoint")
        .unwrap();
    endpoint.socket_path = socket.into();
    hosts
}

fn question() -> Question {
    Question {
        key: ConsentKey {
            node: "node".into(),
            user: 501,
            socket: "/adk/herdr/agentdesk.sock".into(),
            named_session: "agentdesk".into(),
            config: "/fixture/config.toml".into(),
            target: "/fixture/config.toml".into(),
        },
        preview: Preview {
            revision: 1,
            witness: ServerWitness::for_test(7),
            target_hash: [1; 32],
            chain_hash: [2; 32],
            checker_hash: [3; 32],
        },
        owner: 1,
        bot: 2,
        provider: "claude".into(),
        channel: 41,
        message: 4,
        intent: IntentRef {
            store: "mailbox".into(),
            id: "81:7".into(),
            provider: "claude".into(),
            channel: 41,
            source_message: 81,
            fifo: 7,
        },
    }
}

async fn boot(
    approvals: &FileApprovalStore,
    candidate: &BootSessionHosts,
    rows: bool,
) -> (BootSessionHosts, Option<&'static str>, Vec<String>) {
    let (hosts, metadata) = select(
        candidate.clone(),
        approvals,
        &Rows(rows),
        &ConsentHolds,
        false,
    )
    .await
    .into_parts();
    let diagnostic = metadata.migration().cloned();
    let blockers = diagnostic.iter().flat_map(|d| d.blockers.clone()).collect();
    (hosts, diagnostic.map(|d| d.reason), blockers)
}

// A held channel rolls back only to the binding its input was returned to, and only after the
// approval is durable does that return count as adopted; re-switching waits for its closure.
#[tokio::test(flavor = "current_thread")]
async fn migration_returned_rollback_passes_only_its_binding_and_adopts_after_durable_approval() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let approvals_dir = tempfile::tempdir().unwrap();
    let approvals = FileApprovalStore(Some(approvals_dir.path().into()));
    let user = hosts(EndpointMode::UserDefault, "/adk/herdr/agentdesk.sock");
    let legacy = hosts(EndpointMode::LegacyDedicated, "/adk/herdr/agentdesk.sock");
    let other = hosts(EndpointMode::LegacyDedicated, "/adk/herdr/other.sock");
    let digest = |hosts: &BootSessionHosts| binding_digest(hosts.config(), 41).unwrap();
    approvals
        .save(&user)
        .expect("a user-default approval decodes and saves");
    let consents = store();
    let held = consents.hold(question(), 100).unwrap().into_consent();
    let denied = consents.decide(&held, 1, false, 101).unwrap();
    let a = denied.intents[0].clone();
    let state = || consents.load(&denied.key).unwrap().unwrap().resolution(&a);
    let refused = (user.clone(), Some("session_hosts_migration_refused"));
    let (installed, reason, blockers) = boot(&approvals, &legacy, false).await;
    assert_eq!((installed, reason), refused.clone(), "an unreturned hold");
    assert_eq!(blockers, vec!["channel 41 unresolved hold".to_string()]);
    let reserve = |target| vec![(a.clone(), Resolution::Returned { target })];
    let returned = consents.settle(&denied, &reserve(digest(&other))).unwrap();
    let (installed, reason, _) = boot(&approvals, &legacy, false).await;
    assert_eq!((installed, reason), refused.clone(), "another binding");
    consents
        .settle(&returned, &reserve(digest(&legacy)))
        .unwrap();
    let (installed, reason, blockers) = boot(&approvals, &legacy, true).await;
    assert_eq!((installed, reason), refused, "a live row still refuses");
    assert_eq!(blockers, vec!["channel 41 row 7".to_string()]);
    FAIL_CURRENT_SYNC.with(|fault| fault.set(true));
    let (installed, reason, _) = boot(&approvals, &legacy, false).await;
    assert_eq!(
        (installed, reason),
        (user.clone(), Some("approval_snapshot_not_durable"))
    );
    assert_eq!(
        state(),
        Some(Resolution::Returned {
            target: digest(&legacy)
        }),
        "an approval that is not durable adopts nothing"
    );
    let (installed, reason, _) = boot(&approvals, &legacy, false).await;
    assert_eq!((installed, reason), (legacy.clone(), None));
    let adopted = Resolution::Adopted {
        target: digest(&legacy),
    };
    assert_eq!(state(), Some(adopted));
    let (installed, reason, _) = boot(&approvals, &user, false).await;
    assert_eq!(
        (installed, reason),
        (legacy.clone(), Some("session_hosts_migration_refused")),
        "an open return refuses the user-default re-switch"
    );
    let latest = consents.load(&denied.key).unwrap().unwrap();
    let closed = Resolution::Closed {
        target: digest(&legacy),
    };
    consents.settle(&latest, &[(a.clone(), closed)]).unwrap();
    let (installed, reason, _) = boot(&approvals, &user, false).await;
    assert_eq!((installed, reason), (user, None));
}
