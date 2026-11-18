//! The admitted launch's trust watch: one Enter on a selected positive menu in the managed cwd,
//! spent from the admission's own deadline and repair, never adoption evidence.
#![cfg(unix)]
use super::*;
use crate::services::tui_o::channel_policy::FirstHold;
use crate::services::tui_o::channel_policy::output_disposition::BUDGET;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TRUST: &str = "Do you trust this folder?\n› 1. Yes, proceed\n  2. No, exit";
const NEGATIVE: &str = "Do you trust this folder?\n  1. Yes, proceed\n› 2. No, exit";

/// A tmux on PATH that shows `pane` in `cwd` and logs every key it is sent.
struct FakeTmux {
    dir: tempfile::TempDir,
    _path: crate::config::TestEnvVarGuard,
}

impl FakeTmux {
    fn new(cwd: &Path, pane: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (program, log) = (dir.path().join("tmux"), dir.path().join("keys.log"));
        // The managed-session check reads this runtime's identity before a key is sent.
        let identity = crate::services::session_identity_metadata::runtime_identity("trust")
            .unwrap()
            .environment_assignment()
            .unwrap();
        let identity = crate::services::process::shell_escape(&identity);
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*show-environment*) printf '%s\\n' {identity};;\n*display-message*) printf '0\\n{}\\n';;\n*capture-pane*) printf '%s' '{pane}';;\n*send-keys*) echo \"$*\" >> '{}';;\nesac\n",
            cwd.display(),
            log.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
        Self { dir, _path: path }
    }

    fn enters(&self) -> usize {
        let log = std::fs::read_to_string(self.dir.path().join("keys.log"));
        log.map_or(0, |log| log.lines().count())
    }
}

fn managed_cwd() -> PathBuf {
    let cwd = crate::config::runtime_root()
        .unwrap()
        .join("workspaces/project");
    std::fs::create_dir_all(&cwd).unwrap();
    cwd
}

/// Runs the watch on paused time with no other timer, so the clock moves only by its own looks
/// and the admission's deadline bounds it.
async fn watch(fixture: &Fixture, cwd: &Path) {
    let context = fixture.prepared.context.clone();
    let watch = crate::services::tui_o::writer::normalization::watch_trust(
        fixture.candidate.clone(),
        context,
        cwd.to_path_buf(),
    );
    watch.await;
}

#[tokio::test(start_paused = true)]
async fn first_trust_menu_gets_one_enter_and_the_proof_keeps_that_budget() {
    let fixture = Fixture::new(7_651_401);
    let admitted = tokio::time::Instant::now();
    let cwd = managed_cwd();
    let tmux = FakeTmux::new(&cwd, TRUST);
    watch(&fixture, &cwd).await;
    assert_eq!(
        tmux.enters(),
        1,
        "one Enter on the selected positive option"
    );
    assert_eq!(
        fixture.candidate.peek(),
        Adoption::Deferred,
        "the action adopts nothing"
    );
    let context = &fixture.prepared.context;
    assert!(
        fixture.candidate.first_trust_window(context).is_none(),
        "the repair is spent"
    );
    // The rollout proves the admission later: its output keeps the admission's budget.
    tokio::time::advance(Duration::from_secs(5)).await;
    fixture.candidate.normalize_first(&fixture.source);
    let nonce = first_nonce_id(fixture.token.turn_nonce().unwrap());
    let Some(FirstHold::Output(held)) = fixture.candidate.first_hold(nonce) else {
        panic!("the proved rollout starts the normalization");
    };
    let (output, proof) = *held;
    assert!(
        !output.take_repair(),
        "the binding repair is the spent trust repair"
    );
    assert_eq!(
        output.remaining(),
        BUDGET.saturating_sub(admitted.elapsed())
    );
    assert!(
        output.adopt(&proof, || Ok(())).is_ok(),
        "the proved output is adopted"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_unmanaged_changed_or_late_trust_menu_gets_no_enter() {
    for (channel, case) in (7_651_402..).zip(["negative", "outside", "changed", "late"]) {
        let fixture = Fixture::new(channel);
        let cwd = managed_cwd();
        let elsewhere = tempfile::tempdir().unwrap();
        let tmux = match case {
            "negative" => FakeTmux::new(&cwd, NEGATIVE),
            "outside" => FakeTmux::new(elsewhere.path(), TRUST),
            _ => FakeTmux::new(&cwd, TRUST),
        };
        match case {
            "changed" => {
                let tmux = &fixture.prepared.context.tmux_session;
                let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
                std::fs::write(marker, "another-execution").unwrap();
            }
            "late" => tokio::time::advance(BUDGET).await,
            _ => {}
        }
        watch(&fixture, &cwd).await;
        assert_eq!(tmux.enters(), 0, "{case}: no Enter");
        assert_eq!(fixture.candidate.peek(), Adoption::Deferred, "{case}");
    }
}
