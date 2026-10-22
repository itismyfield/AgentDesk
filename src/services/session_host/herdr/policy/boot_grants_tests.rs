use super::*;
use crate::config::session_hosts::{EndpointConfig, SessionHostsConfig};
use crate::services::{herdr_launch::policy::LaunchGrantStore, session_host::ServerWitness};

fn configured(mode: EndpointMode) -> SessionHostsConfig {
    let mut config = SessionHostsConfig::default();
    config.herdr.endpoints.insert(
        "default".into(),
        EndpointConfig {
            mode,
            execution_node: "node".into(),
            socket_path: "/fixture/herdr.sock".into(),
            herdr_home: "/fixture/home".into(),
            herdr_session: "agentdesk".into(),
        },
    );
    config
}
fn endpoint() -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: "node".into(),
        config_key: "default".into(),
        socket_addr: "/fixture/herdr.sock".into(),
        herdr_session: "agentdesk".into(),
    }
}
#[tokio::test]
async fn legacy_boot_never_reads_even_an_unavailable_store_or_creates_policy_directories() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("runtime/herdr_policy/grants");
    let config = configured(EndpointMode::LegacyDedicated);
    assert_eq!(
        load(&config, Some("node"), &FileGrantStore::new(None)).await,
        Ok(GrantRestoration::default()),
        "legacy_boot_store_io_zero_contract"
    );
    assert_eq!(
        load(&config, Some("node"), &FileGrantStore::new(Some(path))).await,
        Ok(GrantRestoration::default())
    );
    assert!(
        !root.path().join("runtime").exists(),
        "legacy_boot_policy_directory_creation_zero_contract"
    );
}
#[tokio::test]
async fn user_boot_restores_exact_historical_grants_and_distinguishes_absence_from_io_failure() {
    let root = tempfile::tempdir().unwrap();
    let store = FileGrantStore::new(Some(root.path().join("grants")));
    let config = configured(EndpointMode::UserDefault);
    assert_eq!(
        load(&config, Some("node"), &store).await,
        Ok(GrantRestoration::default()),
        "missing_grant_is_absent_authority_contract"
    );
    let mut witness = ServerWitness::for_test(7);
    witness.socket = endpoint().socket_addr.into();
    let grant = super::super::permit::grant_for_test(&endpoint(), "nonce", witness);
    store.save(grant.clone()).await.unwrap();
    assert_eq!(
        load(&config, Some("node"), &store).await,
        Ok(GrantRestoration::from_grants(vec![grant])),
        "boot_restores_grants_before_synchronous_gate_contract"
    );
    assert_eq!(
        load(&config, Some("node"), &FileGrantStore::new(None)).await,
        Err(Unverified::Cache),
        "store_read_failure_is_cache_not_absence_contract"
    );
    assert_eq!(
        load(&config, Some("other-node"), &FileGrantStore::new(None)).await,
        Ok(GrantRestoration::default()),
        "remote_endpoint_store_io_zero_contract"
    );
}
#[test]
fn synchronous_mutation_evidence_does_not_read_disk_or_block_on_async_owners() {
    let source = include_str!("../launch_host.rs");
    let evidence = source
        .split("pub(crate) fn mutation_evidence(")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    for forbidden in ["std::fs", ".load(", "block_on", "sqlx", "spawn_blocking"] {
        assert!(
            !evidence.contains(forbidden),
            "mutation_gate_memory_only_contract: {forbidden}"
        );
    }
    assert!(
        include_str!("../../../../bootstrap.rs")
            .contains("boot_grants::prepare(selection.hosts()).await"),
        "async_boot_owner_restores_before_install_contract"
    );
}
