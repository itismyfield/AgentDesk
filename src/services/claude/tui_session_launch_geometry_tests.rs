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
const HELD: &str = "160x40 1 manual";

/// Real tmux on private `-L` sockets behind a shim that records AgentDesk's calls and fails a
/// verb on request; drop kills only these servers.
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
                 if [ -e \"$AGENTDESK_ROOT_DIR/fail-$2\" ]; then echo \"injected $2 failure\" >&2; exit 1; fi\n\
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

    fn alive(&self, name: &str) -> bool {
        let target = format!("={name}:");
        self.real(&["has-session", "-t", &target]).status.success()
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
    fn attach_small_client(&self, name: &str) {
        let attach = format!(
            "env -u TMUX '{}' -L {} -f /dev/null attach -t ={name}",
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

// Only an allowlisted channel opens sized and held; another channel and `None` make the same
// create call as before, and a Herdr channel reaches no tmux at all.
#[test]
fn claude_launch_sizes_only_the_allowlisted_channel_and_keeps_the_rest_unchanged() {
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

    for (name, channel) in [
        ("AgentDesk-cga-other", Some(OTHER)),
        ("AgentDesk-cga-none", None),
    ] {
        lab.launch(name, channel).unwrap();
        let launched = lab.take_calls();
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
        assert_ne!(lab.geometry(name), HELD, "{name} keeps the default size");
    }

    let name = "AgentDesk-cga-test";
    lab.launch(name, Some(TEST)).unwrap();
    let calls = lab.take_calls();
    assert!(
        calls[0].starts_with("-u new-session -d -P -F ")
            && calls[0].contains(&format!(" -s {name} -x 160 -y 40 -c ")),
        "{calls:?}"
    );
    assert!(
        calls[1].starts_with("-u resize-window -t @") && calls[1].ends_with(" -x 160 -y 40"),
        "{calls:?}"
    );
    assert_eq!(lab.geometry(name), HELD);
    lab.attach_small_client(name);
    assert_eq!(
        lab.geometry(name),
        HELD,
        "an 80x24 client must not shrink the pane"
    );
}

// A resize or geometry-query failure after create removes exactly that session and its launch
// files, leaves another session alone, and the next launch of the channel succeeds.
#[test]
fn guarded_launch_failure_after_create_leaves_nothing_and_the_next_launch_succeeds() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let lab = Lab::new();
    let _hosts = crate::config::session_hosts::force_with_claude_guard_for_test(None, &[], &[TEST]);
    for (verb, bystander) in [("resize-window", false), ("display-message", true)] {
        let name = format!("AgentDesk-cga-fail-{verb}");
        let other = "AgentDesk-cga-bystander";
        if bystander {
            assert!(
                lab.real(&["new-session", "-d", "-s", other, "sleep 600"])
                    .status
                    .success()
            );
        }
        let fault = lab.root.path().join(format!("fail-{verb}"));
        std::fs::write(&fault, "").unwrap();
        let error = lab.launch(&name, Some(TEST)).unwrap_err();
        assert!(
            error.contains(&format!("injected {verb} failure")),
            "{error}"
        );
        assert!(
            error.contains("removed") && !error.contains("unconfirmed"),
            "{error}"
        );
        assert!(!lab.alive(&name), "{verb}: the created session is gone");
        assert_eq!(
            lab.alive(other),
            bystander,
            "{verb}: other sessions are untouched"
        );
        for leftover in [
            tc::tmux_owner_path(&name),
            tc::session_temp_path(&name, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT),
            tc::session_dead_marker_path(&name),
        ] {
            assert!(
                !std::path::Path::new(&leftover).exists(),
                "{verb}: {leftover}"
            );
        }

        std::fs::remove_file(&fault).unwrap();
        lab.take_calls();
        lab.launch(&name, Some(TEST))
            .expect("the next launch succeeds");
        assert_eq!(lab.geometry(&name), HELD);
    }
}
