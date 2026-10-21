use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Instant, SystemTime};

use tempfile::TempDir;

use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;

struct Fixture {
    _dir: TempDir,
    peer: PeerSnapshot,
}

fn fixture(body: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("peer-home");
    let config = home.join(".config/herdr/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, b"[session]\nresume_agents_on_restore = false\n").unwrap();
    let checker = dir.path().join("fake-local-checker");
    fs::write(&checker, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
    let image = file::image(&checker).unwrap();
    Fixture {
        _dir: dir,
        peer: PeerSnapshot {
            witness: ServerWitness::for_test(71),
            started: SystemTime::now(),
            home,
            config,
            checker,
            image,
        },
    }
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[tokio::test]
async fn checker_isolates_environment_and_uses_exact_local_arguments() {
    let inherited_path = std::env::var("PATH").unwrap();
    let fixture = fixture(&format!(
        "[ \"$#\" -eq 2 ] && [ \"$1\" = config ] && [ \"$2\" = check ] || exit 17\n\
         [ \"${{PATH-}}\" != {} ] || exit 18\n\
         [ \"$HERDR_CONFIG_PATH\" = \"$HOME/.config/herdr/config.toml\" ] || exit 19\n\
         [ -f \"$HERDR_CONFIG_PATH\" ] || exit 20\n\
         printf 'config: ok\\n'",
        shell_literal(&inherited_path),
    ));
    let config_before = fs::read(&fixture.peer.config).unwrap();
    let checker_before = fs::read(&fixture.peer.checker).unwrap();
    let result = run(&fixture.peer).await;
    assert_eq!(result, Ok(()), "checker_environment_isolation_contract");
    assert_eq!(fs::read(&fixture.peer.config).unwrap(), config_before);
    assert_eq!(fs::read(&fixture.peer.checker).unwrap(), checker_before);
}

#[tokio::test]
async fn checker_rejects_stdout_diagnostics_even_when_exit_is_successful() {
    for output in [
        "config: ok\\nunknown key: theme.typo\\n",
        "using defaults\\nconfig: ok\\n",
        "config: ok",
        "",
    ] {
        let fixture = fixture(&format!("printf '{output}'"));
        assert_eq!(
            run(&fixture.peer).await,
            Err(Unverified::Diagnostics),
            "checker_stdout_diagnostic_contract: {output:?}"
        );
    }
}

#[tokio::test]
async fn checker_rejects_stderr_diagnostics_and_failed_exit() {
    let stderr = fixture("printf 'config: ok\\n'; printf 'unknown config key\\n' >&2");
    assert_eq!(
        run(&stderr.peer).await,
        Err(Unverified::Diagnostics),
        "checker_stderr_diagnostic_contract"
    );
    let failed = fixture("printf 'config: ok\\n'; exit 7");
    assert_eq!(
        run(&failed.peer).await,
        Err(Unverified::Checker),
        "checker_exit_status_contract"
    );
}

#[tokio::test]
async fn checker_bounds_each_output_stream() {
    for redirect in ["", " >&2"] {
        let fixture = fixture(&format!("printf '{}'{redirect}", "x".repeat(1024)));
        assert_eq!(
            run_with_limits(&fixture.peer, Duration::from_secs(1), 64).await,
            Err(Unverified::Diagnostics),
            "checker_output_bound_contract: {redirect:?}"
        );
    }
}

#[tokio::test]
async fn checker_timeout_returns_without_waiting_for_busy_child() {
    let fixture = fixture("while :; do :; done");
    let began = Instant::now();
    assert_eq!(
        run_with_limits(&fixture.peer, Duration::from_millis(80), OUTPUT_LIMIT).await,
        Err(Unverified::Timeout),
        "checker_timeout_contract"
    );
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "checker_timeout_does_not_block_turn_contract"
    );
}

#[tokio::test]
async fn checker_rejects_another_executable_and_relative_selection() {
    let mut selected = fixture("printf 'config: ok\\n'");
    let other = fixture("printf 'config: ok\\n'");
    selected.peer.image = other.peer.image.clone();
    assert_eq!(
        run(&selected.peer).await,
        Err(Unverified::Identity),
        "checker_peer_image_identity_contract"
    );
    selected.peer.checker = "fake-local-checker".into();
    assert_eq!(
        run(&selected.peer).await,
        Err(Unverified::Ambiguous),
        "checker_no_path_fallback_contract"
    );
}

#[tokio::test]
async fn checker_rejects_replacement_before_execution() {
    let fixture = fixture("printf 'config: ok\\n'");
    fs::write(
        &fixture.peer.checker,
        "#!/bin/sh\nprintf 'config: ok\\n'\n# replaced",
    )
    .unwrap();
    assert_eq!(
        run(&fixture.peer).await,
        Err(Unverified::Identity),
        "checker_pre_execution_identity_contract"
    );
}

#[tokio::test]
async fn checker_execution_failure_is_unverified() {
    let mut fixture = fixture("printf 'config: ok\\n'");
    fs::set_permissions(&fixture.peer.checker, fs::Permissions::from_mode(0o400)).unwrap();
    fixture.peer.image = file::image(&fixture.peer.checker).unwrap();
    assert_eq!(
        run(&fixture.peer).await,
        Err(Unverified::Checker),
        "checker_execution_failure_contract"
    );
}

#[tokio::test]
async fn checker_rejects_replacement_during_execution() {
    let fixture = fixture("printf '# changed\\n' >> \"$0\"; printf 'config: ok\\n'");
    assert_eq!(
        run(&fixture.peer).await,
        Err(Unverified::Changed),
        "checker_post_execution_identity_contract"
    );
}
