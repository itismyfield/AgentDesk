use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;
use crate::services::session_host::herdr::policy::{Unverified, file};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::SystemTime;

pub(crate) struct Source(pub Mutex<PeerSnapshot>);
impl PeerSource for Source {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        Ok(self.0.lock().unwrap().clone())
    }
}
pub(crate) fn fixture(body: &str) -> (tempfile::TempDir, Arc<Source>) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, "[session]\nresume_agents_on_restore=true\n").unwrap();
    let checker = dir.path().join("fake-local-checker");
    fs::write(&checker, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
    let mut witness = ServerWitness::for_test(71);
    witness.socket = dir.path().join("fixture.sock");
    let source = Arc::new(Source(Mutex::new(PeerSnapshot {
        witness,
        socket_dir: file::directory_identity(dir.path()).unwrap(),
        started: SystemTime::now(),
        home: dir.path().into(),
        config,
        image: file::image(&checker).unwrap(),
        checker,
    })));
    (dir, source)
}

#[tokio::test]
async fn consent_preview_binds_existing_missing_and_symlink_cas() {
    let (_dir, source) = fixture("printf 'config: ok\\n'");
    let existing = prepare(source.clone(), 1).await.unwrap();
    let config = source.snapshot().unwrap().config;
    assert!(existing.before_hash().is_some());
    let mut key = ConsentKey {
        node: "one".into(),
        user: 501,
        socket: source.snapshot().unwrap().witness.socket,
        named_session: "adk".into(),
        config: config.clone(),
        target: existing.target().into(),
    };
    assert!(existing.matches_key(&key));
    for field in 0..3 {
        let mut changed = key.clone();
        match field {
            0 => changed.config.push("other"),
            1 => changed.target.push("other"),
            _ => changed.socket.push("other"),
        }
        assert!(
            !existing.matches_key(&changed),
            "preview_endpoint_path_binding_contract"
        );
    }
    fs::remove_file(&config).unwrap();
    let missing = prepare(source.clone(), 1).await.unwrap();
    assert!(missing.before_hash().is_none());
    assert_ne!(
        existing.token.chain_hash, missing.token.chain_hash,
        "preview_absence_cas_contract"
    );
    let target = config.with_file_name("target.toml");
    fs::write(&target, "[session]\nresume_agents_on_restore=true\n").unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();
    let linked = prepare(source.clone(), 1).await.unwrap();
    key.target = target.canonicalize().unwrap();
    assert!(linked.matches_key(&key));
    assert_ne!(
        linked.token.chain_hash, existing.token.chain_hash,
        "preview_symlink_cas_contract"
    );
    assert_eq!(linked.logical(), config);
    assert_eq!(linked.target(), target.canonicalize().unwrap());
}

#[test]
fn consent_peer_factory_selects_registered_endpoint_without_dialling() {
    use crate::services::herdr_launch::HerdrLaunchEndpoint;
    use crate::services::session_host::herdr::{
        launch_host::SocketHerdrLaunchHost, model::HerdrEndpoint, transport::HerdrSocketConfig,
    };
    let endpoint =
        HerdrEndpoint::new("one", "primary", Path::new("/fixture/socket"), "adk").unwrap();
    let host = SocketHerdrLaunchHost::new(vec![endpoint], HerdrSocketConfig::default());
    let mut launch = HerdrLaunchEndpoint {
        execution_node: "one".into(),
        config_key: "primary".into(),
        socket_addr: "/fixture/socket".into(),
        herdr_session: "adk".into(),
    };
    assert!(
        host.consent_source(&launch).is_some(),
        "consent_current_peer_factory_contract"
    );
    launch.config_key = "other".into();
    assert!(
        host.consent_source(&launch).is_none(),
        "consent_current_peer_factory_contract"
    );
}

#[tokio::test]
async fn consent_preview_checks_whole_candidate_without_publication() {
    let (dir, source) = fixture(
        "if /usr/bin/grep -q theme_typo \"$HERDR_CONFIG_PATH\"; then printf 'config: ok\\nunknown key: theme_typo\\n'; else printf 'config: ok\\n'; fi",
    );
    let config = dir.path().join("config.toml");
    let original = "theme_typo=true\n[session]\nresume_agents_on_restore=true\n";
    fs::write(&config, original).unwrap();
    let result = prepare(source.clone(), 1).await;
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.contains("Diagnostics")),
        "preview_full_checker_before_card_contract: {result:?}"
    );
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        original,
        "preview_never_applies_contract"
    );
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        2,
        "preview_temp_cleanup_contract"
    );
    fs::write(&config, "[session]\nresume_agents_on_restore=true\n").unwrap();
    let before = fs::read(&config).unwrap();
    assert!(prepare(source, 1).await.is_ok());
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        2,
        "preview_temp_cleanup_contract"
    );
}

#[tokio::test]
async fn consent_preview_rechecks_original_and_candidate_after_checker() {
    for body in [
        "printf '# changed\\n' >> \"$HOME/config.toml\"; printf 'config: ok\\n'",
        "printf '# changed\\n' >> \"$HERDR_CONFIG_PATH\"; printf 'config: ok\\n'",
    ] {
        let (dir, source) = fixture(body);
        let result = prepare(source, 1).await;
        assert!(
            result.is_err(),
            "preview_post_checker_cas_contract: {result:?}"
        );
        assert!(
            fs::read_to_string(dir.path().join("config.toml"))
                .unwrap()
                .contains("=true")
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
