use super::prepare_and_create_claude_tui_session as launch;
use crate::config::TestEnvVarGuard as Guard;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const TEST: u64 = 61;
const OTHER: u64 = 62;
const HERDR: u64 = 63;

/// Real tmux on private `-L` sockets behind a call-recording shim; drop kills only these servers.
struct Lab {
    root: tempfile::TempDir,
    real: PathBuf,
    socket: String,
    sockets: tempfile::TempDir,
    _guards: Vec<Guard>,
    _env: [Guard; 2],
}

impl Lab {
    fn new() -> Self {
        let (root, env) = tests::fixture_after_shared_test_env_lock();
        let path = std::env::var("PATH").unwrap();
        let real = std::env::split_paths(&path)
            .map(|dir| dir.join("tmux"))
            .find(|path| path.is_file())
            .expect("a real tmux binary");
        let sockets = tempfile::tempdir().unwrap();
        let socket = format!("cga-{}", std::process::id());
        let executable = |name: &str, body: &str| {
            let file = root.path().join(name);
            std::fs::write(&file, body).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
            file
        };
        let real_call = format!("exec '{}' -L {socket} -f /dev/null \"$@\"", real.display());
        executable(
            "tmux",
            &format!(
                "#!/bin/bash\n[ \"$1\" = -u ] || {real_call}\n\
                 printf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n\
                 {real_call}\n"
            ),
        );
        let claude = executable(
            "claude",
            "#!/bin/bash\n[ \"$1\" = --version ] && echo '2.1.0 (Claude Code)' && exit 0\nexec sleep 600\n",
        );
        let shimmed = format!("{}:{path}", root.path().display());
        let tmux_env = Guard::capture_after_shared_test_env_lock("TMUX");
        unsafe { std::env::remove_var("TMUX") };
        let guards = vec![
            Guard::set_value_after_shared_test_env_lock("PATH", shimmed.as_ref()),
            Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude),
            Guard::set_path_after_shared_test_env_lock("TMUX_TMPDIR", sockets.path()),
            tmux_env,
        ];
        Self {
            root,
            real,
            socket,
            sockets,
            _guards: guards,
            _env: env,
        }
    }

    fn launch(&self, tmux: &str, channel: Option<u64>) -> Result<(), String> {
        let dir = self.root.path().to_str().unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        launch(
            tmux,
            dir,
            self.root.path(),
            id,
            None,
            None,
            "".into(),
            false,
            "",
            channel,
        )
        .map(|_| ())
    }

    fn real_on(&self, socket: &str, args: &[&str]) -> Output {
        Command::new(&self.real)
            .args(["-L", socket, "-f", "/dev/null"])
            .args(args)
            .env("TMUX_TMPDIR", self.sockets.path())
            .env_remove("TMUX")
            .output()
            .unwrap()
    }

    fn real(&self, args: &[&str]) -> Output {
        self.real_on(&self.socket, args)
    }

    /// Pane size, pane count and size mode, as read straight from the server.
    fn geometry(&self, name: &str) -> String {
        let format = "#{pane_width}x#{pane_height} #{window_panes} #{window-size}";
        let shown = self.real(&["display-message", "-p", "-t", &format!("={name}:"), format]);
        String::from_utf8_lossy(&shown.stdout).trim().to_string()
    }

    fn take_calls(&self) -> Vec<String> {
        let path = self.root.path().join("tmux.calls");
        let calls = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        calls.lines().map(str::to_string).collect()
    }

    /// Attach an 80×24 client from a pane of a second private server.
    /// The target is quoted because zsh, the default shell on macOS, expands a bare `=name`.
    fn attach_small_client(&self, name: &str) {
        let attach = format!(
            "env -u TMUX '{}' -L {} -f /dev/null attach -t '={name}'",
            self.real.display(),
            self.socket
        );
        let driver = format!("{}-driver", self.socket);
        let started = self.real_on(
            &driver,
            &[
                "new-session",
                "-d",
                "-s",
                "driver",
                "-x",
                "80",
                "-y",
                "24",
                &attach,
            ],
        );
        assert!(started.status.success(), "{started:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let clients = self.real(&["list-clients", "-F", "#{client_width}x#{client_height}"]);
            if String::from_utf8_lossy(&clients.stdout).trim() == "80x24" {
                return;
            }
            assert!(Instant::now() < deadline, "no 80x24 client: {clients:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let driver = format!("{}-driver", self.socket);
        self.real_on(&driver, &["kill-server"]);
        self.real(&["kill-server"]);
    }
}

// Guarded and other channels use the regular tmux launch; Herdr selection still refuses tmux.
#[test]
fn claude_launch_guarded_and_other_channels_use_regular_sessions() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let lab = Lab::new();
    let _hosts = crate::config::session_hosts::force_with_claude_guard_for_test(
        Some("mac-mini"),
        &[(HERDR, "mac-mini")],
        &[TEST],
    );

    let refused = lab.launch("AgentDesk-cga-herdr", Some(HERDR));
    assert_eq!(
        refused.err().as_deref(),
        Some(crate::services::herdr_launch::HERDR_NOT_ADMITTED)
    );
    assert!(lab.take_calls().is_empty(), "Herdr reaches no tmux");

    assert!(crate::config::session_hosts::claude_guarded(Some(TEST)));
    assert!(!crate::config::session_hosts::claude_guarded(Some(OTHER)));
    assert!(!crate::config::session_hosts::claude_guarded(None));

    for (name, channel) in [
        ("claude-launch-guarded", Some(TEST)),
        ("AgentDesk-cga-other", Some(OTHER)),
        ("AgentDesk-cga-none", None),
    ] {
        lab.launch(name, channel).unwrap();
        let launched = lab.take_calls();
        let geometry = lab.geometry(name);
        for evidence in [
            tc::tmux_owner_path(name),
            tc::session_temp_path(name, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT),
            tc::session_temp_path(name, tc::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT),
            tc::session_temp_path(name, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
            tc::session_temp_path(name, "host_kind"),
        ] {
            assert!(std::path::Path::new(&evidence).is_file(), "{evidence}");
        }
        if channel == Some(TEST) {
            lab.attach_small_client(name);
            assert_eq!(lab.geometry(name), "80x23 1 latest");
        }
        assert!(
            lab.real(&["kill-session", "-t", &format!("={name}")])
                .status
                .success()
        );
        let script = tc::session_temp_path(name, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
        let command = format!("bash {}", super::shell_escape(&script));
        let dir = lab.root.path().to_str().unwrap();
        crate::services::platform::tmux::create_session(name, Some(dir), &command).unwrap();
        let direct = lab.take_calls();
        assert_eq!(
            launched.first(),
            direct.first(),
            "{name}: create args differ"
        );
        let verbs = |calls: &[String]| -> Vec<String> {
            let verb = |call: &String| call.split(' ').nth(1).unwrap_or_default().to_string();
            calls.iter().map(verb).collect()
        };
        assert_eq!(verbs(&launched), verbs(&direct), "{name}: {launched:?}");
        assert!(!launched.iter().any(|call| call.contains("resize-window")));
        assert_eq!(geometry, lab.geometry(name), "{name}: default geometry");
    }
}
