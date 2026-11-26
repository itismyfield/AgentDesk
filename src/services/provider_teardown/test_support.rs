use std::io::Write;
use std::os::unix::fs::PermissionsExt;

/// PATH-first fake tmux logging each call: sessions exist with live panes, only a new
/// session fails, and a kill notes an exit reason already written. No CLI resolves.
pub(crate) struct FakeTmux {
    dir: tempfile::TempDir,
    _env: Vec<crate::config::TestEnvVarGuard>,
}

impl FakeTmux {
    #[cfg(test)]
    pub(super) fn install_with_runtime_root(
        name: &str,
    ) -> (crate::config::TestRuntimeRootGuard, Self) {
        let root = crate::config::TestRuntimeRootGuard::new();
        let tmux = Self::install(name);
        (root, tmux)
    }

    /// Needs the shared test-env lock held, e.g. by a `TestRuntimeRootGuard`.
    pub(crate) fn install(name: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tmux dir");
        let reason = crate::services::tmux_common::session_temp_path(name, "exit_reason");
        std::fs::write(dir.path().join("reason_path"), reason).unwrap();
        std::fs::write(dir.path().join("session_name"), format!("{name}\n")).unwrap();
        let identity = crate::services::session_identity_metadata::runtime_identity(name).unwrap();
        std::fs::write(
            dir.path().join("identity"),
            identity.environment_assignment().unwrap(),
        )
        .unwrap();
        let binary = dir.path().join("tmux");
        let mut file = std::fs::File::create(&binary).expect("fake tmux");
        writeln!(
            file,
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
             [ \"$1\" = -N ] && shift\n\
             if [ \"$1\" = show-environment ]; then [ ! -f \"$d/missing\" ] && [ ! -f \"$d/probe_fails\" ] && [ \"$3\" = \"=$(cat \"$d/session_name\")\" ] || exit 1; cat \"$d/identity\"; exit 0; fi\n\
             echo \"$*\" >> \"$d/calls\"\ncase \"$1\" in\n\
             has-session) [ -f \"$d/probe_fails\" ] && {{ echo \"error connecting to $d/socket (Permission denied)\" >&2; exit 1; }}\n\
             [ -f \"$d/missing\" ] && {{ echo \"can't find session: $3\" >&2; exit 1; }}; exit 0 ;;\nlist-panes) if [ -f \"$d/pane_dead\" ]; then echo 1; else echo 0; fi; exit 0 ;;\n\
             display-message) for adk_fake_arg do last=\"$adk_fake_arg\"; done; \n\
             if [ \"$last\" = '#{{pane_dead}}' ]; then [ -f \"$d/probe_fails\" ] && exit 1; \n\
             if [ -f \"$d/pane_dead\" ] || [ -f \"$d/missing\" ]; then echo 1; else echo 0; fi; exit 0; fi; \n\
             [ -f \"$d/pane_pid\" ] && {{ cat \"$d/pane_pid\"; exit 0; }} ;;\n\
             -V) [ -f \"$d/launch_enabled\" ] && {{ echo tmux 3.5a; exit 0; }} ;;\n\
             capture-pane) if [ -f \"$d/launch_enabled\" ]; then cat \"$d/ready\"; else echo pane; fi; exit 0 ;;\n\
             kill-session) [ -f \"$(cat \"$d/reason_path\")\" ] && echo reason-before-kill >> \"$d/calls\"\n\
             [ -f \"$d/hold\" ] && {{ : > \"$d/killing\"; n=0; while [ ! -f \"$d/release\" ] && [ $n -lt 200 ]; do sleep 0.05; n=$((n+1)); done; }}\n\
             [ -f \"$d/pane_pid\" ] && [ ! -f \"$d/pane_survives\" ] && kill -9 \"$(cat \"$d/pane_pid\")\" 2>/dev/null\n\
             [ -f \"$d/lost_kill_ack\" ] && {{ : > \"$d/missing\"; exit 1; }}\n\
             [ -f \"$d/kill_fails\" ] && exit 1\n\
             [ -f \"$d/missing_after_kill\" ] && : > \"$d/missing\"; exit 0 ;;\n\
             list-sessions) [ -f \"$d/missing_after_kill\" ] && {{ [ -f \"$d/missing\" ] || cat \"$d/session_name\"; [ ! -f \"$d/external_sessions\" ] || cat \"$d/external_sessions\"; exit 0; }} ;;\n\
             new-session) [ -f \"$d/launch_enabled\" ] && {{ rm -f \"$d/missing\"; for adk_fake_arg do adk_fake_command=\"$adk_fake_arg\"; done; /bin/bash -c \"$adk_fake_command\"; exit $?; }} ;;\n\
             set-option|set-hook) [ -f \"$d/launch_enabled\" ] && exit 0 ;;\n\
             esac\necho \"can't find session: $3\" >&2; exit 1"
        )
        .expect("fake tmux body");
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).expect("join PATH");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let absent = dir.path().join("absent-cli");
        let env = vec![
            set("PATH", std::path::Path::new(&path)),
            set("AGENTDESK_CLAUDE_PATH", &absent),
            set("AGENTDESK_CODEX_PATH", &absent),
        ];
        Self { dir, _env: env }
    }

    pub(crate) fn add_unwatched_inventory(&self, name: &str) {
        std::fs::write(
            self.dir.path().join("external_sessions"),
            format!("{name}\n"),
        )
        .unwrap();
    }

    pub(crate) fn mark_missing(&self) {
        std::fs::write(self.dir.path().join("missing"), "").unwrap();
    }

    /// The flag `mark_missing` writes, for a session another fake ends.
    pub(crate) fn missing_flag(&self) -> std::path::PathBuf {
        self.dir.path().join("missing")
    }

    pub(crate) fn mark_present(&self) {
        let _ = std::fs::remove_file(self.dir.path().join("missing"));
    }

    /// A remain-on-exit pane keeps its session present after its process has ended.
    pub(crate) fn mark_dead_pane(&self) {
        self.mark_present();
        std::fs::write(self.dir.path().join("pane_dead"), "").unwrap();
    }

    /// Successful kills remove this session from later presence and inventory reads.
    pub(crate) fn become_missing_after_kill(&self) {
        std::fs::write(self.dir.path().join("missing_after_kill"), "").unwrap();
    }

    /// New sessions run only their fixture command and expose a ready, empty composer.
    pub(crate) fn enable_fixture_launch(&self) {
        std::fs::write(self.dir.path().join("launch_enabled"), "").unwrap();
        std::fs::write(
            self.dir.path().join("ready"),
            "› \n\n  gpt-6.1-sol · Fast off · Context 100% left\n",
        )
        .unwrap();
    }

    /// Every later presence probe fails with a socket error, neither present nor missing.
    pub(crate) fn fail_probes(&self) {
        std::fs::write(self.dir.path().join("probe_fails"), "").unwrap();
    }

    pub(crate) fn restore_probes(&self) {
        let _ = std::fs::remove_file(self.dir.path().join("probe_fails"));
    }

    /// The session disappears but kill returns an unconfirmed failure.
    pub(crate) fn lose_kill_ack_after_exit(&self) {
        let _ = std::fs::remove_file(self.dir.path().join("kill_fails"));
        std::fs::write(self.dir.path().join("lost_kill_ack"), "").unwrap();
    }

    /// Every later kill fails and leaves the session in place.
    pub(crate) fn fail_kills(&self) {
        std::fs::write(self.dir.path().join("kill_fails"), "").unwrap();
    }

    /// Later kills wait (up to 10s) until `release_kills`; `killing` says one is waiting.
    pub(crate) fn hold_kills(&self) {
        std::fs::write(self.dir.path().join("hold"), "").unwrap();
    }

    pub(crate) fn killing(&self) -> bool {
        self.dir.path().join("killing").exists()
    }

    pub(crate) fn release_kills(&self) {
        std::fs::write(self.dir.path().join("release"), "").unwrap();
    }

    /// The session's pane runs `pid`, which a later kill ends unless the pane survives it.
    pub(crate) fn run_pane(&self, pid: u32) {
        std::fs::write(self.dir.path().join("pane_pid"), pid.to_string()).unwrap();
    }

    pub(crate) fn keep_pane_alive(&self) {
        std::fs::write(self.dir.path().join("pane_survives"), "").unwrap();
    }

    /// The logged calls, oldest first, and clears the log.
    pub(crate) fn take_calls(&self) -> Vec<String> {
        let log = self.dir.path().join("calls");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(log);
        calls.lines().map(str::to_string).collect()
    }
}
