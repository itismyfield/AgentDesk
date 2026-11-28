use std::path::PathBuf;
use std::process::Output;

#[cfg(unix)]
use super::create::{HostCreate, Request as CreateRequest, TmuxCreate, TmuxPlace};
use super::model::{
    HostCapabilities, HostError, HostKey, HostKind, HostLiveness, HostMutation, HostPresence,
    HostSessionRef,
};
use super::submit::{InputSource, SubmitPayload};
use super::traits::InteractiveSessionHost;
use crate::services::platform::tmux;

#[cfg(unix)]
mod fifo;
pub(super) mod input;

/// Adapter over platform probes and input executors, retaining their transport behaviour.
pub struct TmuxHost;

fn map_output(result: Result<Output, String>) -> Result<HostMutation, HostError> {
    match result {
        Ok(output) if output.status.success() => Ok(HostMutation::Confirmed),
        Ok(output) => Err(HostError::Transport(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
        Err(error) => Err(HostError::Transport(error)),
    }
}

/// The physical tmux create of an admitted launch.
#[cfg(unix)]
pub(super) fn create(request: &TmuxCreate<'_>) -> Result<Output, String> {
    let (session, dir, command) = (request.session, request.working_dir, request.command);
    match request.place {
        TmuxPlace::NewSession(identity) => {
            tmux::create_session_with_identity(session, dir, command, identity)
        }
        TmuxPlace::DeadPane => tmux::respawn_dead_pane(session, dir, command),
    }
}

/// tmux `send-keys` name of a host key.
pub fn tmux_key_name(key: HostKey) -> &'static str {
    match key {
        HostKey::Enter => "Enter",
        HostKey::Escape => "Escape",
        HostKey::CtrlU => "C-u",
        HostKey::CtrlE => "C-e",
        HostKey::CtrlS => "C-s",
        HostKey::Left => "Left",
        HostKey::Right => "Right",
        HostKey::Backspace => "BSpace",
    }
}

impl TmuxHost {
    /// A new detached session running the prepared command; the launch keeps tmux's raw result.
    #[cfg(unix)]
    pub(crate) fn create_session(
        &self,
        session: &str,
        working_dir: Option<&str>,
        command: &str,
        identity: &crate::domain::session_identity::managed::ManagedIdentity,
    ) -> Result<Output, String> {
        self.create(HostCreate(CreateRequest::Tmux(TmuxCreate {
            session,
            working_dir,
            command,
            place: TmuxPlace::NewSession(identity),
        })))
        .into_output()
    }

    /// The prepared command started again in the session's dead pane; a live pane is refused.
    #[cfg(unix)]
    pub(crate) fn respawn_dead_pane(
        &self,
        session: &str,
        working_dir: Option<&str>,
        command: &str,
    ) -> Result<Output, String> {
        self.create(HostCreate(CreateRequest::Tmux(TmuxCreate {
            session,
            working_dir,
            command,
            place: TmuxPlace::DeadPane,
        })))
        .into_output()
    }

    /// Preserve the stop owner's keys and raw status/error handling.
    pub fn interrupt_keys(&self, name: &str, keys: &[&str]) -> Result<Output, String> {
        tmux::send_keys(name, keys)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn legacy_has_session_for_test(&self, name: &str) -> bool {
        self.legacy_has_session(name)
    }

    /// Keep the legacy boolean collapse inside the platform compatibility probe.
    pub(crate) fn legacy_has_session(&self, name: &str) -> bool {
        tmux::has_session(name)
    }

    /// Keep the legacy bool probe, including its unbounded list-panes call.
    pub(crate) fn legacy_has_live_pane(&self, name: &str) -> bool {
        tmux::has_live_pane(name)
    }

    pub(crate) fn pane_liveness(&self, name: &str) -> tmux::PaneLiveness {
        tmux::pane_liveness(name)
    }

    pub fn pane_current_path(&self, name: &str) -> Option<String> {
        tmux::pane_current_path(name)
    }

    pub fn list_session_names(&self) -> Result<Vec<String>, String> {
        tmux::list_session_names()
    }

    pub(crate) fn capture_raw(&self, session: &str, scroll_back: i32) -> Option<String> {
        tmux::capture_pane(session, scroll_back)
    }

    pub(crate) fn capture_ansi(&self, session: &str, scroll_back: i32) -> Option<String> {
        tmux::capture_pane_with_escapes(session, scroll_back)
    }

    pub(crate) fn capture_bounded(
        &self,
        session: &str,
        scroll_back: i32,
        timeout: std::time::Duration,
    ) -> Option<String> {
        tmux::capture_pane_timeout(session, scroll_back, timeout)
    }

    pub fn kill(&self, name: &str, reason: &str) -> bool {
        tmux::kill_session(name, reason)
    }

    pub fn kill_output(&self, name: &str, reason: &str) -> std::io::Result<Output> {
        tmux::kill_session_output(name, reason)
    }

    pub fn kill_output_timeout(
        &self,
        name: &str,
        reason: &str,
        timeout: std::time::Duration,
    ) -> Result<Output, String> {
        tmux::kill_session_output_timeout(name, reason, timeout)
    }

    /// Raw `send-keys` output, so the caller keeps tmux's exit status and stderr.
    pub(crate) fn send_host_keys(&self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        self.platform_input(session, InputSource::Inherited, SubmitPayload::Keys(keys))
            .into_output()
    }

    /// Literal text behind the owner content fence; unheld targets run main's command.
    pub(crate) fn send_literal(&self, session: &str, text: &str) -> Result<Output, String> {
        self.platform_input(
            session,
            InputSource::Inherited,
            SubmitPayload::Literal(text),
        )
        .into_output()
    }

    pub(crate) fn paste_buffer(
        &self,
        s: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        self.platform_input(
            s,
            InputSource::Inherited,
            SubmitPayload::Paste { buffer, delete },
        )
        .into_output()
    }

    pub fn liveness_within(
        &self,
        session: HostSessionRef<'_>,
        budget: std::time::Duration,
    ) -> HostLiveness {
        let probe = |name| tmux::pane_liveness_within(name, budget).into();
        session
            .legacy_name()
            .map_or(HostLiveness::ProbeError, probe)
    }
}

impl InteractiveSessionHost for TmuxHost {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            idle_cleanup: true,
            send_text: true,
            send_keys: true,
            interrupt: true,
            capture_screen: true,
            current_working_dir: true,
            execution_pid: true,
        }
    }

    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(injected) = super::test_support::injected_presence(session) {
            return injected;
        }
        let probe = |name| tmux::session_presence(name).into();
        session
            .legacy_name()
            .map_or(HostPresence::ProbeFailed, probe)
    }

    // Same probe as the sync `tmux_diagnostics::tmux_session_pane_liveness`.
    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(injected) = super::test_support::injected_liveness(session) {
            return injected;
        }
        let probe = |name| self.pane_liveness(name).into();
        session
            .legacy_name()
            .map_or(HostLiveness::ProbeError, probe)
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        map_output(self.send_literal(session.legacy_name()?, text))
    }

    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        map_output(tmux::send_keys(session.legacy_name()?, keys))
    }

    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        map_output(self.interrupt_keys(session.legacy_name()?, &["C-c"]))
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        tmux::capture_pane(session.legacy_name()?, scroll_back)
            .ok_or_else(|| HostError::Transport("tmux capture-pane failed".to_string()))
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        Ok(self
            .pane_current_path(session.legacy_name()?)
            .map(PathBuf::from))
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        Ok(tmux::pane_pid(session.legacy_name()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn busy_input_preserves_the_original_target_argument() {
        use super::super::SubmitPayload;
        use std::os::unix::fs::PermissionsExt;

        let root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let program = dir.path().join("tmux");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program =
            crate::services::platform::tmux::session_identity_tests::managed_fixture_program(
                program,
            );
        let target = "==adk-original-target:";
        TmuxHost
            .run_busy(&program, target, &["send-keys", "-t", target, "Enter"])
            .unwrap();
        let original = std::fs::read_to_string(&log).unwrap();
        TmuxHost
            .busy_input(&program, target, SubmitPayload::Keys(&[HostKey::Enter]))
            .into_bounded()
            .unwrap();
        assert_eq!(std::fs::read_to_string(log).unwrap(), original);
        drop(root);
    }

    // The prepared argv, cwd and identity reach tmux as the platform call sends them, a new session
    // and a dead pane's restart stay distinct, and the launch reads tmux's own status and stderr.
    #[cfg(unix)]
    #[test]
    fn create_runs_the_prepared_argv_as_the_platform_does_and_keeps_the_raw_result() {
        use super::super::create::HostCreation;
        use crate::services::provider_teardown::test_support::FakeTmux;
        let _root = crate::config::TestRuntimeRootGuard::new();
        let name = "adk-s04-create-port";
        let fake = FakeTmux::install(name);
        let identity = crate::services::session_identity_metadata::runtime_identity(name).unwrap();
        let (dir, command) = (Some("/tmp"), "bash '/tmp/launch one.sh'");
        let failed = |output: Result<Output, String>| {
            let output = output.unwrap();
            (output.status.code(), output.stderr)
        };
        let platform = [
            failed(tmux::create_session_with_identity(
                name, dir, command, &identity,
            )),
            failed(tmux::respawn_dead_pane(name, dir, command)),
        ];
        let original = fake.take_calls();
        let through = [
            failed(TmuxHost.create_session(name, dir, command, &identity)),
            failed(TmuxHost.respawn_dead_pane(name, dir, command)),
        ];
        assert_eq!(fake.take_calls(), original);
        assert_eq!(through, platform);
        assert!(platform.iter().all(|(code, stderr)| *code == Some(1)
            && String::from_utf8_lossy(stderr).contains("can't find session")));
        let verbs: Vec<_> = original
            .iter()
            .filter_map(|call| call.split(' ').next())
            .collect();
        assert_eq!(verbs, ["new-session", "respawn-pane"], "{original:?}");
        assert!(
            original
                .iter()
                .all(|call| call.ends_with(&format!("-c /tmp {command}")))
        );

        let session = |command| {
            CreateRequest::Tmux(TmuxCreate {
                session: name,
                working_dir: dir,
                command,
                place: TmuxPlace::NewSession(&identity),
            })
        };
        let refused = TmuxHost.create(HostCreate(session(command)));
        assert!(
            matches!(refused.creation, HostCreation::Indeterminate(why) if why.contains("can't find session"))
        );
        fake.enable_fixture_launch();
        let created = TmuxHost.create(HostCreate(session("true")));
        assert_eq!(created.creation, HostCreation::Created(name.into()));
        assert!(created.into_output().unwrap().status.success());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_pid_delegation_preserves_the_lookup_for_herdr_shaped_names() {
        use crate::services::provider_teardown::test_support::FakeTmux;
        let root = crate::config::TestRuntimeRootGuard::new();
        for name in ["adk-pid-delegation", "42"] {
            let fake = FakeTmux::install(name);
            fake.run_pane(31337);
            assert_eq!(tmux::pane_pid(name), Some(31337));
            assert_eq!(
                TmuxHost
                    .execution_pid(HostSessionRef::tmux(name))
                    .ok()
                    .flatten(),
                Some(31337)
            );
            assert!(
                TmuxHost
                    .execution_pid(HostSessionRef::herdr_pane(name))
                    .is_err()
            );
            assert_eq!(
                fake.take_calls(),
                vec![format!("display-message -p -t ={name}: #{{pane_pid}}"); 2]
            );
        }
        drop(root);
    }

    #[cfg(unix)]
    #[test]
    fn observations_keep_probe_errors_distinct_from_legacy_bool_results() {
        use crate::services::provider_teardown::test_support::FakeTmux;
        use crate::services::session_host::legacy_collapse;
        use crate::services::tmux_diagnostics;

        let root = crate::config::TestRuntimeRootGuard::new();
        let name = "adk-s02-observation-port";
        let fake = FakeTmux::install(name);
        let session = HostSessionRef::tmux(name);
        let check = |presence: HostPresence, liveness: HostLiveness, legacy_live: bool| {
            assert_eq!(TmuxHost.presence(session), presence);
            assert_eq!(HostLiveness::from(TmuxHost.pane_liveness(name)), liveness);
            assert_eq!(TmuxHost.liveness(session), liveness);
            assert_eq!(
                HostLiveness::from(tmux_diagnostics::tmux_session_pane_liveness(name)),
                liveness
            );
            assert_eq!(
                TmuxHost.legacy_has_session(name),
                presence == HostPresence::Present
            );
            assert_eq!(
                tmux_diagnostics::tmux_session_exists(name),
                presence == HostPresence::Present
            );
            assert_eq!(TmuxHost.legacy_has_live_pane(name), legacy_live);
            assert_eq!(legacy_collapse::tmux_live_pane_bool(name), legacy_live);
            assert_eq!(
                tmux_diagnostics::tmux_session_has_live_pane(name),
                legacy_live
            );
            let calls = fake.take_calls();
            assert!(!calls.is_empty());
            let expected_target = format!("={name}:");
            assert!(
                calls.iter().all(|call| {
                    let mut args = call.split_whitespace();
                    matches!(args.next(), Some("has-session" | "list-panes"))
                        && args.next() == Some("-t")
                        && args.next() == Some(expected_target.as_str())
                }),
                "{calls:?}"
            );
        };
        check(HostPresence::Present, HostLiveness::Live, true);
        fake.mark_dead_pane();
        check(HostPresence::Present, HostLiveness::DeadOrAbsent, false);
        fake.mark_missing();
        check(HostPresence::Missing, HostLiveness::DeadOrAbsent, false);
        fake.fail_probes();
        check(HostPresence::ProbeFailed, HostLiveness::ProbeError, false);
        drop(fake);
        drop(root);
    }

    #[test]
    fn kill_methods_preserve_the_exact_audit_reason() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(log.reopen().unwrap())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let reasons = [
            " bool 종료 사유 = keep spaces ",
            " output 종료 사유 = keep spaces ",
            " timeout 종료 사유 = keep spaces ",
        ];
        // Blank targets reach the platform's refusal log without spawning tmux.
        tracing::subscriber::with_default(subscriber, || {
            assert!(!TmuxHost.kill("", reasons[0]));
            assert!(
                !TmuxHost
                    .kill_output("", reasons[1])
                    .unwrap()
                    .status
                    .success()
            );
            assert!(
                TmuxHost
                    .kill_output_timeout("", reasons[2], std::time::Duration::from_millis(1))
                    .is_err()
            );
        });
        let logged = std::fs::read_to_string(log.path()).unwrap();
        let observed: Vec<&str> = logged
            .lines()
            .filter_map(|line| line.split_once("reason=").map(|parts| parts.1))
            .collect();
        assert_eq!(observed, reasons, "platform audit log: {logged}");
    }

    #[test]
    fn probes_are_the_platform_probes() {
        // A blank name short-circuits both platform probes without spawning tmux.
        let blank = HostSessionRef::tmux("");
        assert_eq!(TmuxHost.presence(blank), tmux::session_presence("").into());
        assert_eq!(TmuxHost.presence(blank), HostPresence::ProbeFailed);
        assert_eq!(TmuxHost.liveness(blank), tmux::pane_liveness("").into());
        assert_eq!(TmuxHost.liveness(blank), HostLiveness::DeadOrAbsent);
        assert_eq!(TmuxHost.kind(), HostKind::Tmux);
        assert!(TmuxHost.capabilities().interrupt);
    }

    #[test]
    fn host_keys_use_the_legacy_tmux_key_names() {
        let names = [
            (HostKey::Enter, "Enter"),
            (HostKey::Escape, "Escape"),
            (HostKey::CtrlU, "C-u"),
            (HostKey::CtrlE, "C-e"),
            (HostKey::CtrlS, "C-s"),
            (HostKey::Left, "Left"),
            (HostKey::Right, "Right"),
            (HostKey::Backspace, "BSpace"),
        ];
        for (key, name) in names {
            assert_eq!(tmux_key_name(key), name, "{key:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn mutation_outcome_follows_the_tmux_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        let output = |code: i32, stderr: &str| Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(map_output(Ok(output(0, ""))), Ok(HostMutation::Confirmed));
        assert_eq!(
            map_output(Ok(output(1, "can't find pane\n"))),
            Err(HostError::Transport("can't find pane".to_string()))
        );
        assert_eq!(
            map_output(Err("spawn failed".to_string())),
            Err(HostError::Transport("spawn failed".to_string()))
        );
    }

    #[test]
    fn herdr_ref_never_reaches_a_tmux_probe() {
        // A real probe of an absent session would read DeadOrAbsent.
        let herdr = HostSessionRef::herdr_pane("session-host-herdr-no-such-tmux");
        assert_eq!(
            TmuxHost.liveness(herdr),
            HostLiveness::ProbeError,
            "a same-named Herdr pane must never reach a tmux probe"
        );
        assert_eq!(TmuxHost.presence(herdr), HostPresence::ProbeFailed);
        let refused = Err(HostError::Unsupported(HostKind::Herdr, "legacy_name"));
        assert_eq!(TmuxHost.send_text(herdr, "x"), refused);
        assert_eq!(
            TmuxHost.execution_pid(herdr),
            Err(HostError::Unsupported(HostKind::Herdr, "legacy_name"))
        );
    }
}
