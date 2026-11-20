use super::*;
use crate::services::codex_tui::host_input::trust::{Pane, TrustHost, TrustTransport};
use crate::services::session_host::HostKey;
use crate::services::tui_o::channel_policy::{first_prompt_digest, output_disposition::Identity};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::PreparedIncarnation};
use std::{collections::VecDeque, path::PathBuf};

const TRUST: &str = "Do you trust this folder?\n› 1. Yes, proceed\n  2. No, exit";

struct Host {
    cwd: PathBuf,
    panes: VecDeque<String>,
    keys: Vec<Vec<HostKey>>,
    invalidate: Option<String>,
}
impl TrustTransport for Host {
    fn observe(&mut self, _: &str, budget: Duration) -> Option<Pane> {
        assert!(!budget.is_zero() && budget <= Duration::from_millis(200));
        self.panes.pop_front().map(|text| Pane {
            cwd: self.cwd.clone(),
            text,
        })
    }
    fn enter(&mut self, _: &str, budget: Duration) -> bool {
        assert!(!budget.is_zero() && budget <= Duration::from_millis(200));
        self.keys.push(vec![HostKey::Enter]);
        if let Some(path) = self.invalidate.take() {
            std::fs::write(path, "different-execution").unwrap();
        }
        true
    }
}

fn context(root: &Path) -> BindingContext {
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: format!("trust-fixture-{}", uuid::Uuid::new_v4()),
        channel_id: Some(7651),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.join("sessions")),
        first_prompt_digest: Some(first_prompt_digest("original prompt")),
        source_policy: None,
    };
    let prepared = PreparedIncarnation::create(context.clone()).unwrap();
    crate::services::discord::stamp_spawn_markers(&context.tmux_session, Some(&prepared)).unwrap();
    dedupe::register_tmux_channel(&context.tmux_session, 7651);
    context
}
fn output(context: &BindingContext) -> Output {
    Output::new(
        Identity {
            channel: 7651,
            input: 1,
            nonce: first_prompt_digest("turn"),
            execution: first_prompt_digest(&context.execution_nonce),
            source: first_prompt_digest("source"),
        },
        tokio::time::Instant::now(),
    )
}
fn host(cwd: &Path, pane: &str) -> Host {
    Host {
        cwd: cwd.into(),
        panes: [pane.into(), "ready".into()].into(),
        keys: vec![],
        invalidate: None,
    }
}

#[test]
fn managed_current_trust_menu_gets_one_action_and_a_fresh_capture() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = bc::tests::fixture_after_shared_test_env_lock();
    let cwd = root.path().join("workspaces/project");
    std::fs::create_dir_all(&cwd).unwrap();
    let context = context(root.path());
    let output = output(&context);
    let mut host = host(&cwd, TRUST);
    assert_eq!(
        repair_once(&output, &context, &cwd, &mut host),
        Ok("ready".into())
    );
    assert_eq!(host.keys, vec![vec![HostKey::Enter]]);
    host.panes = [TRUST.into(), "ready".into()].into();
    assert_eq!(
        repair_once(&output, &context, &cwd, &mut host),
        Err(Reason::TrustUnavailable)
    );
    assert_eq!(host.keys.len(), 1);
    assert_eq!(output.phase(), Phase::Normalizing);
}

#[test]
fn non_trust_or_unmanaged_or_changed_execution_has_no_action() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = bc::tests::fixture_after_shared_test_env_lock();
    let cwd = root.path().join("workspaces/project");
    std::fs::create_dir_all(&cwd).unwrap();
    let context = context(root.path());
    for pane in [
        "› 1. Sign in with ChatGPT\n2. Use an API key",
        "Unknown\n› 1. Continue\n2. Exit",
        "Do you trust this folder?\n 1. Yes, proceed\n› 2. No, exit",
        "Do you trust this folder?\n› 1. Yes, proceed\n2. No, exit\n›",
    ] {
        let mut host = host(&cwd, pane);
        assert!(repair_once(&output(&context), &context, &cwd, &mut host).is_err());
        assert!(host.keys.is_empty());
    }
    let outside = tempfile::tempdir().unwrap();
    let escaped = root.path().join("workspaces/escaped");
    std::os::unix::fs::symlink(outside.path(), &escaped).unwrap();
    for denied in [outside.path(), &escaped] {
        let mut host = host(denied, TRUST);
        assert_eq!(
            repair_once(&output(&context), &context, denied, &mut host),
            Err(Reason::TrustUnavailable)
        );
        assert!(host.keys.is_empty());
    }
    let mut stale = context.clone();
    stale.owner_runtime_root.push_str("/foreign");
    let mut host = host(&cwd, TRUST);
    assert_eq!(
        repair_once(&output(&stale), &stale, &cwd, &mut host),
        Err(Reason::Invalidated)
    );
    assert!(host.keys.is_empty());
}

#[test]
fn invalidation_during_repair_cannot_turn_the_old_execution_into_success() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = bc::tests::fixture_after_shared_test_env_lock();
    let cwd = root.path().join("worktrees/project");
    std::fs::create_dir_all(&cwd).unwrap();
    let context = context(root.path());
    let output = output(&context);
    let mut host = host(&cwd, TRUST);
    host.invalidate = Some(crate::services::tmux_common::session_temp_path(
        &context.tmux_session,
        "spawn_nonce",
    ));
    assert_eq!(
        repair_once(&output, &context, &cwd, &mut host),
        Err(Reason::Invalidated)
    );
    assert_eq!(host.keys, vec![vec![HostKey::Enter]]);
    assert_eq!(output.phase(), Phase::Normalizing);
}

#[test]
fn real_trust_adapter_bounds_cwd_and_pane_observation() {
    use crate::config::TestEnvVarGuard as Guard;
    use std::os::unix::fs::PermissionsExt;
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = bc::tests::fixture_after_shared_test_env_lock();
    let program = root.path().join("tmux");
    std::fs::write(&program, "#!/bin/sh\ncase \" $* \" in\n*display-message*) printf '0\\n/fixture/cwd\\n';;\n*capture-pane*) printf 'fresh pane';;\nesac\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _path = Guard::prepend_path_after_shared_test_env_lock(root.path());
    // A freshly written fixture program can take over 100 ms to start on a loaded macOS host.
    let observed = TrustHost
        .observe("trust-adapter-fixture", Duration::from_secs(4))
        .unwrap();
    assert_eq!(observed.cwd, PathBuf::from("/fixture/cwd"));
    assert_eq!(observed.text, "fresh pane");
    std::fs::write(&program, "#!/bin/sh\nprintf '1\\n/fixture/cwd\\n'\n").unwrap();
    assert!(
        TrustHost
            .observe("trust-adapter-fixture", Duration::from_secs(4))
            .is_none(),
        "a dead pane has no cwd to trust"
    );
    std::fs::write(&program, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
    let started = std::time::Instant::now();
    assert!(
        TrustHost
            .observe("trust-adapter-fixture", Duration::from_millis(40))
            .is_none()
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded adapter did not reap its query"
    );
}
