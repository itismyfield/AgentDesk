//! The actual boot snapshot entry leaves input-off TUI channels on the Legacy path.

use crate::services::discord::input_runtime::{fence, supervisor};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::ledger::{Ledger, OPENS, Presence};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TEST: &str = "b_runtime_adapter_has_zero_production_starts";
const CHILD: &str = "ADK_G2B_DORMANT_CHILD";
const TMUX_LOG: &str = "ADK_G2B_DORMANT_TMUX_LOG";
const CONFIRMED: &str = "G2B_DORMANT_BOOT_CONFIRMED";
const CHANNELS: [u64; 2] = [6_326_360, 6_326_361];

fn isolated() -> bool {
    if std::env::var(CHILD).as_deref() == Ok(TEST) {
        return true;
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/g2b-dormant");
    std::fs::create_dir_all(&base).unwrap();
    let sandbox = tempfile::tempdir_in(base).unwrap();
    let runtime = sandbox.path().join("runtime");
    let temporary = sandbox.path().join("tmp");
    let bin = sandbox.path().join("bin");
    for path in [&runtime, &temporary, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    let stub = bin.join("tmux");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ADK_G2B_DORMANT_TMUX_LOG\"\nexit 97\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let qualified = format!("{}::{TEST}", module_path!().split_once("::").unwrap().1);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture"])
        .env(CHILD, TEST)
        .env("AGENTDESK_ROOT_DIR", &runtime)
        .env("TMPDIR", &temporary)
        .env(TMUX_LOG, sandbox.path().join("tmux-calls"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env_remove("DATABASE_URL")
        .env_remove("POSTGRES_TEST_DATABASE_URL_BASE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("isolated dormant boot fixture exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    assert!(
        stdout.contains(CONFIRMED),
        "the selected boot fixture did not run: {stdout}"
    );
    false
}

fn input_off_config() -> crate::config::Config {
    let binding = |provider: &str, channel: u64| {
        serde_json::json!({"id": format!("dormant-{provider}"), "name": "Dormant TUI",
            "channels": {provider: {"id": channel.to_string(), "runtime": "tui"}}})
    };
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": "dormant", "gateway_preferred_instance_id": "dormant"},
        "agents": [binding("claude", CHANNELS[0]), binding("codex", CHANNELS[1])],
        "tui_o": {"writer": {"all_tui": true}, "turn": {"channels": CHANNELS}},
    }))
    .unwrap()
}

fn empty(path: &Path) {
    let contents: Vec<PathBuf> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(
        contents.is_empty(),
        "dormant boot created input resources: {contents:?}"
    );
}

#[tokio::test]
async fn b_runtime_adapter_has_zero_production_starts() {
    if !isolated() {
        return;
    }
    let root = crate::config::runtime_root().unwrap();
    let temporary = PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    let tmux_log = PathBuf::from(std::env::var_os(TMUX_LOG).unwrap());
    let config = input_off_config();
    assert!(
        !supervisor::REGISTRY.used(),
        "the child starts with an unused input registry"
    );
    assert_eq!(OPENS.with(|opens| opens.get()), 0);
    let selection = supervisor::activation::plan::InputSelection::default();
    assert!(
        selection.validate(&config).unwrap().is_empty(),
        "input stays Legacy while O turn channels are selected"
    );
    for _ in 0..2 {
        crate::bootstrap::install_boot_snapshots(&config).unwrap();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let writer = crate::services::tui_o::channel_policy::boot().unwrap();
        assert_eq!(
            writer.selected().iter().copied().collect::<Vec<_>>(),
            CHANNELS
        );
        assert!(
            !supervisor::REGISTRY.used(),
            "boot must not reserve or start a B supervisor"
        );
        assert!(supervisor::REGISTRY.health_reasons().is_empty());
        for channel in CHANNELS {
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                assert!(
                    fence::lookup(&provider, channel).is_none(),
                    "input-off installs no protected gate"
                );
                assert!(
                    fence::effect::admit(&provider, channel).unwrap().is_none(),
                    "Legacy admission retains its off-channel shape"
                );
            }
            assert_eq!(Ledger::probe(&root, channel), Presence::Absent);
        }
        assert_eq!(
            OPENS.with(|opens| opens.get()),
            0,
            "boot does not open an input ledger"
        );
        empty(&root);
        empty(&temporary);
        assert!(
            !tmux_log.exists(),
            "boot created or submitted input to a tmux pane"
        );
    }
    println!("{CONFIRMED}");
}
