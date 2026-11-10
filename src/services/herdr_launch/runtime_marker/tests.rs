use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::services::platform::binary_resolver::runtime_command;

const NONCE: &str = "11111111111111111111111111111111";

struct Rig {
    owner: HostedOwner,
    _namespace: crate::config::TestEnvVarGuard,
    _temp: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}
impl Rig {
    fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let temp = crate::config::test_env::kept_test_tmpdir("herdr-runtime-marker");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let temp_guard = set("TMPDIR", &temp);
        let namespace = set("TMUX_TMPDIR", &root.path().canonicalize().unwrap());
        let mut owner = owner("698069001");
        owner.provider = "codex".into();
        owner.logical_key = "codex-marker-fixture".into();
        owner.runtime_root = root.path().display().to_string();
        let rig = Self {
            owner,
            _namespace: namespace,
            _temp: temp_guard,
            _root: root,
        };
        rig.write("host_kind", "herdr");
        rig.write("spawn_nonce", NONCE);
        rig.write(tc::TMUX_RUNTIME_KIND_TEMP_EXT, "codex_tui");
        rig
    }
    fn path(&self, ext: &str) -> PathBuf {
        tc::session_temp_path(&self.owner.logical_key, ext).into()
    }
    fn write(&self, ext: &str, value: &str) {
        std::fs::write(self.path(ext), value).unwrap();
    }
    fn retired(&self) -> HostedExecution {
        let mut execution = record(&self.owner, NONCE, HostedState::Retired);
        execution.expected.as_mut().unwrap().binding_provider = "codex".into();
        execution
    }
    fn runtime(&self) -> String {
        std::fs::read_to_string(self.path(tc::TMUX_RUNTIME_KIND_TEMP_EXT)).unwrap()
    }
}

#[tokio::test]
async fn i8069_retired_cleans_only_runtime_hint_and_both_locations() {
    let rig = Rig::new();
    let legacy = PathBuf::from(tc::legacy_tmp_session_path(
        &rig.owner.logical_key,
        tc::TMUX_RUNTIME_KIND_TEMP_EXT,
    ));
    std::fs::write(&legacy, "codex_tui\n").unwrap();
    clear_retired(&rig.retired()).await.unwrap();
    assert!(!rig.path(tc::TMUX_RUNTIME_KIND_TEMP_EXT).exists());
    assert!(
        !legacy.exists(),
        "legacy fallback must not resurrect the hint"
    );
    assert_eq!(
        std::fs::read_to_string(rig.path("spawn_nonce")).unwrap(),
        NONCE
    );
    assert_eq!(
        std::fs::read_to_string(rig.path("host_kind")).unwrap(),
        "herdr"
    );
    clear_retired(&rig.retired()).await.unwrap();
}

#[tokio::test]
async fn i8069_live_herdr_and_successor_markers_are_preserved() {
    let rig = Rig::new();
    for state in [HostedState::Pending, HostedState::Bound] {
        let mut live = rig.retired();
        live.state = state;
        assert!(clear_retired(&live).await.is_err());
        assert_eq!(rig.runtime(), "codex_tui");
    }
    rig.write("spawn_nonce", "22222222222222222222222222222222");
    assert!(clear_retired(&rig.retired()).await.is_err());
    assert_eq!(rig.runtime(), "codex_tui");
}

#[tokio::test(flavor = "multi_thread")]
async fn i8069_successor_publication_wins_before_old_cleanup_source_lock() {
    let rig = Rig::new();
    let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (logical, started, finish) = (
        rig.owner.logical_key.clone(),
        ready.clone(),
        release.clone(),
    );
    let publisher = std::thread::spawn(move || {
        tc::with_tmux_source_authority(&logical, |_| {
            started.wait();
            finish.wait();
            std::fs::write(
                tc::session_temp_path(&logical, "spawn_nonce"),
                "successor-nonce",
            )
            .unwrap();
        });
    });
    ready.wait();
    let old = rig.retired();
    let cleanup = tokio::spawn(async move { clear_retired(&old).await });
    release.wait();
    crate::test_support::wait::join_within(publisher, crate::test_support::wait::WAIT_CAP);
    assert!(cleanup.await.unwrap().is_err());
    assert_eq!(rig.runtime(), "codex_tui");
    assert_eq!(
        std::fs::read_to_string(rig.path("spawn_nonce")).unwrap(),
        "successor-nonce"
    );
}

#[tokio::test]
async fn i8069_foreign_host_runtime_and_unreadable_marker_are_preserved() {
    let rig = Rig::new();
    for host in ["tmux", "process", "unknown-host"] {
        rig.write("host_kind", host);
        assert!(clear_retired(&rig.retired()).await.is_err());
        assert_eq!(rig.runtime(), "codex_tui");
    }
    rig.write("host_kind", "herdr");
    for runtime in ["claude_tui", "process_backend", "unknown-runtime"] {
        rig.write(tc::TMUX_RUNTIME_KIND_TEMP_EXT, runtime);
        assert!(clear_retired(&rig.retired()).await.is_err());
        assert_eq!(rig.runtime(), runtime);
    }
    let runtime = rig.path(tc::TMUX_RUNTIME_KIND_TEMP_EXT);
    std::fs::remove_file(&runtime).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    assert!(clear_retired(&rig.retired()).await.is_err());
    assert!(runtime.is_dir());
}

#[tokio::test]
async fn i8069_foreign_legacy_fallback_keeps_canonical_runtime_too() {
    let rig = Rig::new();
    let legacy = PathBuf::from(tc::legacy_tmp_session_path(
        &rig.owner.logical_key,
        tc::TMUX_RUNTIME_KIND_TEMP_EXT,
    ));
    std::fs::write(&legacy, "process_backend").unwrap();
    assert!(clear_retired(&rig.retired()).await.is_err());
    assert_eq!(rig.runtime(), "codex_tui");
    assert_eq!(std::fs::read_to_string(legacy).unwrap(), "process_backend");
}

#[tokio::test]
async fn i8069_live_tui_marker_is_preserved_on_private_tmux() {
    let rig = Rig::new();
    let name = &rig.owner.logical_key;
    let run = |args: &[&str]| {
        let output = runtime_command("tmux")
            .unwrap()
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    };
    run(&["-f", "/dev/null", "new-session", "-d", "-s", name, "cat"]);
    assert_eq!(tmux::session_presence(name), SessionPresence::Present);
    let result = clear_retired(&rig.retired()).await;
    assert_eq!(tmux::session_presence(name), SessionPresence::Present);
    // Only the fixture's TMUX_TMPDIR namespace is touched.
    run(&["kill-session", "-t", &format!("={name}")]);
    assert!(result.is_err());
    assert_eq!(rig.runtime(), "codex_tui");
}

#[tokio::test]
async fn i8069_fresh_claim_clears_before_provider_and_keeps_new_nonce() {
    let rig = Rig::new();
    clear_fresh(&rig.owner, NONCE).await.unwrap();
    assert!(!rig.path(tc::TMUX_RUNTIME_KIND_TEMP_EXT).exists());
    assert_eq!(
        std::fs::read_to_string(rig.path("spawn_nonce")).unwrap(),
        NONCE
    );
}
