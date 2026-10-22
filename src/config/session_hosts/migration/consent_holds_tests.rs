use super::*;
use crate::config::session_hosts;
use crate::services::session_host::ServerWitness;
use crate::services::session_host::policy::consent::{
    ConsentKey, ConsentStore, IntentRef, Preview, Question,
};

// UserDefault cannot decode through the file validator until activation; supply the approved projection directly.
struct Approved(BootSessionHosts);
impl super::super::ApprovalStore for Approved {
    fn load(&self) -> super::super::Loaded {
        super::super::Loaded::Known(self.0.clone(), None)
    }
    fn save(&self, _: &BootSessionHosts) -> Result<(), String> {
        Ok(())
    }
}

struct Rows;
impl super::super::RowSource for Rows {
    async fn rows(&self, _: &BTreeSet<u64>) -> Result<Vec<super::super::MigrationRow>, String> {
        Ok(Vec::new())
    }
}

fn question() -> Question {
    Question {
        key: ConsentKey {
            node: "node".into(),
            user: 501,
            socket: "/fixture/socket".into(),
            named_session: "agentdesk".into(),
            config: "/fixture/config.toml".into(),
            target: "/fixture/target.toml".into(),
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

fn boot(mode: EndpointMode) -> BootSessionHosts {
    let _forced = session_hosts::force_for_test(Some("node"), &[(41, "node")]).with_mode(mode);
    session_hosts::with_boot(|boot| boot.unwrap().clone())
}

#[tokio::test(flavor = "current_thread")]
async fn real_consent_holds_distinguish_missing_unresolved_and_unreadable() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let directory = crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_policy/consents");
    assert_eq!(
        ConsentHolds
            .unresolved(&BTreeSet::from([41]))
            .await
            .unwrap(),
        Vec::<u64>::new()
    );
    assert!(
        !directory.exists(),
        "read-only absence must not create consent storage"
    );
    let store = ConsentStore::new(Some(directory.clone()));
    let held = store.hold(question(), 100).unwrap().into_consent();
    assert_eq!(
        ConsentHolds
            .unresolved(&BTreeSet::from([41]))
            .await
            .unwrap(),
        vec![41]
    );
    assert!(
        ConsentHolds
            .unresolved(&BTreeSet::from([42]))
            .await
            .unwrap()
            .is_empty()
    );
    store.acknowledge_discard(&held, &held.intents[0]).unwrap();
    assert!(
        ConsentHolds
            .unresolved(&BTreeSet::from([41]))
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::write(directory.join("unknown.json"), b"not json").unwrap();
    assert!(
        ConsentHolds
            .unresolved(&BTreeSet::from([41]))
            .await
            .is_err(),
        "unknown custody must never become an empty live set"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn migration_real_hold_source_is_mode_first_and_unchanged_is_read_free() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let directory = crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_policy/consents");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("unknown.json"), b"not json").unwrap();
    for mode in [EndpointMode::LegacyDedicated, EndpointMode::UserDefault] {
        let approved = boot(mode);
        let store = Approved(approved.clone());
        let unchanged =
            super::super::select(approved.clone(), &store, &Rows, &ConsentHolds, false).await;
        assert!(
            unchanged.into_parts().1.migration().is_none(),
            "unchanged boot does not consult corrupt holds: {mode:?}"
        );
        let mut candidate = approved.clone();
        candidate.channels.get_mut(&41).unwrap().socket_path = "/fixture/new.sock".into();
        candidate
            .config
            .herdr
            .endpoints
            .get_mut("node-endpoint")
            .unwrap()
            .socket_path = "/fixture/new.sock".into();
        let selected =
            super::super::select(candidate.clone(), &store, &Rows, &ConsentHolds, false).await;
        let (installed, metadata) = selected.into_parts();
        if mode == EndpointMode::LegacyDedicated {
            assert_eq!(
                installed, candidate,
                "Legacy migration performs no consent IO"
            );
            assert!(metadata.migration().is_none());
        } else {
            assert_eq!(
                installed, approved,
                "unreadable UserDefault holds keep approved mapping"
            );
            assert_eq!(
                metadata.migration().unwrap().reason,
                "session_hosts_migration_unverified"
            );
        }
    }
}

#[test]
fn hold_selection_checks_old_and_new_user_default_bindings_only() {
    let legacy = boot(EndpointMode::LegacyDedicated);
    let user = boot(EndpointMode::UserDefault);
    let changes = BTreeSet::from([41, 42]);
    assert!(affected_user_default(&legacy, &legacy, &changes).is_empty());
    assert_eq!(
        affected_user_default(&legacy, &user, &changes),
        BTreeSet::from([41])
    );
    assert_eq!(
        affected_user_default(&user, &legacy, &changes),
        BTreeSet::from([41])
    );
    assert!(affected_user_default(&user, &user, &BTreeSet::new()).is_empty());
}
