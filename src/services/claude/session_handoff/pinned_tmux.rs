//! A pane-close capability lasts only on the control connection that observed it.
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const WAIT: Duration = Duration::from_secs(5);
const FORMAT: &str = "#{pid} #{session_id} #{window_id} #{pane_id} #{pane_pid} #{pane_dead}";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pin {
    pub(crate) nonce: String,
    server: u32,
    session: String,
    window: String,
    pane: String,
    pid: u32,
    birth: String,
}

struct Control {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
}
impl Control {
    async fn attach_with(
        mut command: std::process::Command,
        logical: &str,
    ) -> Result<Self, String> {
        crate::services::platform::tmux::validate_target(logical).map_err(|e| e.to_string())?;
        crate::config::test_instance::configure_tmux_command(&mut command)
            .map_err(|e| e.to_string())?;
        let mut command = tokio::process::Command::from(command);
        let mut child = command
            .args([
                "-u",
                "-N",
                "-C",
                "attach-session",
                "-E",
                "-f",
                "ignore-size,no-output",
                "-t",
                &format!("={logical}"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().ok_or("control stdout missing")?).lines();
        let mut control = Self {
            child,
            input,
            output,
        };
        control.reply().await?;
        Ok(control)
    }
    async fn reply(&mut self) -> Result<Vec<String>, String> {
        tokio::time::timeout(WAIT, async {
            let mut begin = None;
            let mut rows = Vec::new();
            for turn in 0..128 {
                let line = self
                    .output
                    .next_line()
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("control connection ended")?;
                if line.len() > 16384 {
                    return Err("control line oversized".into());
                }
                if let Some(id) = line.strip_prefix("%begin ") {
                    if begin.is_some() {
                        return Err("nested control reply".into());
                    }
                    begin = Some(id.to_string());
                } else if let Some(id) = line.strip_prefix("%end ") {
                    if begin.as_deref() != Some(id) {
                        return Err("foreign control reply".into());
                    }
                    return Ok(rows);
                } else if line.starts_with("%error ") || line.starts_with("%exit") {
                    return Err("control command refused or disconnected".into());
                } else if begin.is_some() {
                    rows.push(line);
                }
                if turn == 127 {
                    return Err("control reply overflow".into());
                }
            }
            Err("control reply missing".into())
        })
        .await
        .map_err(|_| "control reply timeout".to_string())?
    }
    async fn command(&mut self, command: &str) -> Result<Vec<String>, String> {
        let input = self.input.as_mut().ok_or("closed control capability")?;
        tokio::time::timeout(WAIT, input.write_all(format!("{command}\n").as_bytes()))
            .await
            .map_err(|_| "control write timeout".to_string())?
            .map_err(|e| e.to_string())?;
        self.reply().await
    }
    async fn finish(mut self) {
        self.input.take();
        let waited = tokio::time::timeout(WAIT, self.child.wait()).await;
        if waited.is_err() {
            tracing::debug!("tmux control client did not detach before deadline");
        }
    }
}

#[cfg(test)]
pub(crate) async fn scoped_control_attach_for_test(
    program: &std::path::Path,
    logical: &str,
) -> Result<(), String> {
    let control = Control::attach_with(std::process::Command::new(program), logical).await?;
    control.finish().await;
    Ok(())
}

fn id(value: &str, prefix: char) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}
fn fields(raw: &[String]) -> Result<(u32, String, String, String, u32), String> {
    let [line] = raw else {
        return Err("pane metadata missing".into());
    };
    let words: Vec<_> = line.split_ascii_whitespace().collect();
    let [server, session, window, pane, pid, "0"] = words.as_slice() else {
        return Err("pane metadata invalid".into());
    };
    let server = server
        .parse::<u32>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or("server pid invalid")?;
    let pid = pid
        .parse::<u32>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or("pane pid invalid")?;
    if !id(session, '$') || !id(window, '@') || !id(pane, '%') {
        return Err("pane identity invalid".into());
    }
    Ok((
        server,
        session.to_string(),
        window.to_string(),
        pane.to_string(),
        pid,
    ))
}
fn birth(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let mut fields = stat.rsplit_once(')')?.1.split_ascii_whitespace();
        let value = fields.nth(19)?;
        value.parse::<u64>().ok().map(|v| format!("linux:{v}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .ok()?;
        let value = String::from_utf8(out.stdout).ok()?;
        (out.status.success() && out.stderr.is_empty() && value.split_whitespace().count() == 5)
            .then(|| format!("darwin:{}", value.trim()))
    }
}
fn exited(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with("Z "))
            })
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && out.stderr.is_empty()
                    && String::from_utf8(out.stdout)
                        .is_ok_and(|state| state.trim_start().starts_with('Z'))
            })
    }
}
impl Pin {
    /// A failed read is never an absence witness; PID reuse is observed, never signalled.
    pub(crate) fn gone(&self) -> bool {
        use crate::services::process::{ProcessIdentity, ProcessIdentityProbe};
        match birth(self.pid) {
            Some(now) => now != self.birth || exited(self.pid),
            None => {
                ProcessIdentity::capture(self.pid).probe(self.pid)
                    == ProcessIdentityProbe::GoneOrReused
            }
        }
    }
}

/// Persist and recheck this exact pane before its only destructive command. Reconnects do not
/// inherit a previous connection's authority, even if the numeric pane id is the same.
pub(crate) async fn end<F, Fut>(
    logical: &str,
    channel: u64,
    expected: Option<&Pin>,
    authorize: F,
) -> Result<(), String>
where
    F: FnMut(Pin) -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    let command = crate::services::platform::binary_resolver::runtime_command("tmux")
        .map_err(|e| e.to_string())?;
    end_with(command, logical, channel, expected, authorize).await
}
async fn end_with<F, Fut>(
    command: std::process::Command,
    logical: &str,
    channel: u64,
    expected: Option<&Pin>,
    mut authorize: F,
) -> Result<(), String>
where
    F: FnMut(Pin) -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    if logical.is_empty()
        || !logical
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
    {
        return Err("logical tmux identity unsupported".into());
    }
    if let Some(pin) = expected.filter(|pin| pin.gone()) {
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        match observe_spawn_nonce_marker(logical) {
            SpawnNonceMarker::Known(nonce) if nonce != pin.nonce => {
                return Err("legacy execution replaced".into());
            }
            SpawnNonceMarker::Unreadable => return Err("legacy marker unreadable".into()),
            _ => (),
        }
        return if authorize(pin.clone()).await? {
            Ok(())
        } else {
            Err("handoff generation revoked".into())
        };
    }
    let nonce =
        match crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(
            logical,
        ) {
            crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker::Known(nonce) => {
                nonce
            }
            _ => return Err("legacy execution nonce unobserved".into()),
        };
    let context =
        crate::services::tui_prompt_dedupe::binding_context::execution_context("claude", &nonce)?;
    if context.tmux_session != logical || context.channel_id != Some(channel) {
        return Err("legacy execution context changed".into());
    }
    let mut control = Control::attach_with(command, logical).await?;
    let query = format!("display-message -p -t ={logical}: '{FORMAT}'");
    let (server, session, window, pane, pid) = fields(&control.command(&query).await?)?;
    crate::services::platform::tmux::validate_target(&pane).map_err(|e| e.to_string())?;
    let pin = Pin {
        nonce,
        server,
        session,
        window,
        pane,
        pid,
        birth: birth(pid).ok_or("provider start unobserved")?,
    };
    if expected.is_some_and(|old| old != &pin) {
        return Err("legacy pane replaced; no close authority".into());
    }
    if !authorize(pin.clone()).await? {
        return Err("handoff generation revoked".into());
    }
    let same = fields(
        &control
            .command(&format!("display-message -p -t {} '{FORMAT}'", pin.pane))
            .await?,
    )?;
    let now_nonce =
        crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(logical);
    if same
        != (
            pin.server,
            pin.session.clone(),
            pin.window.clone(),
            pin.pane.clone(),
            pin.pid,
        )
        || birth(pin.pid).as_deref() != Some(pin.birth.as_str())
        || !matches!(now_nonce, crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker::Known(ref n) if n == &pin.nonce)
    {
        return Err("legacy identity changed before close".into());
    }
    if !authorize(pin.clone()).await? {
        return Err("handoff generation revoked before close".into());
    }
    control
        .command(&format!("kill-pane -t {}", pin.pane))
        .await?;
    control.finish().await;
    if pin.gone() {
        Ok(())
    } else {
        Err("closed pane; provider end still unobserved".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CAPTURE_46: &str = "      \u{001b}[38;5;114m✔ Update installed · Restart to update\u{001b}[39m\n\u{001b}[38;5;244m──────────────────────────────────────────────\n\u{001b}[38;5;246m❯ \u{001b}[39m\n\u{001b}[38;5;244m──────────────────────────────────────────────\n  \u{001b}[38;5;117m◆ Opus(X)\u{001b}[38;5;246m \u{001b}[2m│\u{001b}[0m\u{001b}[38;5;246m \u{001b}[38;5;222m⚠️\u{001b}[38;5;246m \u{001b}[2m│\u{001b}[0m\u{001b}[38;5;246m 7d: \u{001b}[38;5;151m16%\u{001b}[38;5;246m (6d1h) \u{001b}[2m│\u{001b}[0m\u{001b}[38;5;246m 7d-F: \u{001b}[38;5;151m1…\u{001b}[39m\n  \u{001b}[38;5;211m⏵⏵ bypass permissions on\u{001b}[38;5;246m · \u{001b}[38;5;44m2 shells\u{001b}[38;5;246m · \u{001b}[38;5;174m⧉\u{001b}[39m \u{001b}[38;5;246mse\u{001b}[39m\n";
    struct Server {
        dir: tempfile::TempDir,
    }
    impl Server {
        fn command(&self) -> std::process::Command {
            let mut command = std::process::Command::new("tmux");
            command.args(["-S", self.dir.path().join("s").to_str().unwrap()]);
            command
        }
        fn run(&self, args: &[&str]) -> String {
            let out = self.command().args(args).output().unwrap();
            assert!(out.status.success(), "private tmux command failed");
            String::from_utf8(out.stdout).unwrap()
        }
        fn start(&self) {
            let capture = self.dir.path().join("busy-screen");
            std::fs::write(&capture, CAPTURE_46).unwrap();
            let command = format!("cat '{}'; exec sleep 300", capture.display());
            self.run(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "46",
                "-y",
                "39",
                "-s",
                "i8077-close-test",
                &command,
            ]);
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let ended = self.command().args(["-N", "kill-server"]).status();
            assert!(ended.is_ok());
        }
    }
    #[test]
    fn empty_success_and_malformed_metadata_never_authorize_close() {
        for raw in [
            vec![],
            vec![String::new()],
            vec!["1 $0 @0 %0 2 1".into()],
            vec!["1 $0 @0 %0 0 0".into()],
            vec!["1 $0 @0 %wrong 2 0".into()],
        ] {
            assert!(fields(&raw).is_err());
        }
        assert!(fields(&["1 $0 @0 %0 2 0".into()]).is_ok());
    }
    #[tokio::test]
    async fn same_control_connection_closes_only_exact_pane_and_cannot_replay_after_restart() {
        let server = Server {
            dir: tempfile::tempdir().unwrap(),
        };
        server.start();
        assert_eq!(
            server
                .run(&[
                    "-N",
                    "display-message",
                    "-p",
                    "-t",
                    "=i8077-close-test:",
                    "#{pane_width} #{pane_height}"
                ])
                .trim(),
            "46 39"
        );
        let mut control = Control::attach_with(server.command(), "i8077-close-test")
            .await
            .unwrap();
        let original = fields(
            &control
                .command(&format!(
                    "display-message -p -t =i8077-close-test: '{FORMAT}'"
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        let other = server.run(&[
            "-N",
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &original.3,
            "exec sleep 300",
        ]);
        control
            .command(&format!("kill-pane -t {}", original.3))
            .await
            .unwrap();
        let panes = server.run(&["-N", "list-panes", "-a", "-F", "#{pane_id}"]);
        assert!(!panes.split_whitespace().any(|pane| pane == original.3));
        assert!(panes.split_whitespace().any(|pane| pane == other.trim()));
        let empty = control
            .command(&format!("display-message -p -t {} '{FORMAT}'", original.3))
            .await
            .unwrap();
        assert!(
            fields(&empty).is_err(),
            "exit zero with empty output is not identity evidence"
        );
        assert!(
            control
                .command(&format!("kill-pane -t {}", original.3))
                .await
                .is_err()
        );
        server.run(&["-N", "kill-server"]);
        tokio::time::timeout(WAIT, control.child.wait())
            .await
            .unwrap()
            .unwrap();
        server.start();
        let new_pane = server.run(&[
            "-N",
            "display-message",
            "-p",
            "-t",
            "=i8077-close-test:",
            "#{pane_id}",
        ]);
        assert_eq!(
            new_pane.trim(),
            original.3,
            "server restart demonstrates numeric pane reuse"
        );
        assert!(
            control
                .command(&format!("kill-pane -t {}", original.3))
                .await
                .is_err()
        );
        assert_eq!(
            server
                .run(&[
                    "-N",
                    "display-message",
                    "-p",
                    "-t",
                    "=i8077-close-test:",
                    "#{pane_id}"
                ])
                .trim(),
            new_pane.trim()
        );
    }
    #[tokio::test]
    async fn revoked_after_metadata_recheck_sends_no_close_and_lost_marker_can_settle_a_gone_provider()
     {
        use crate::services::tui_prompt_dedupe::binding_context::{
            BindingContext, PreparedIncarnation,
        };
        let root_guard = crate::config::TestRuntimeRootGuard::new();
        let server = Server {
            dir: tempfile::tempdir().unwrap(),
        };
        server.start();
        let logical = "i8077-close-test";
        let nonce = "00112233445566778899aabbccddee77";
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.into(),
            tmux_session: logical.into(),
            channel_id: Some(8077),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some("argv-a".into()),
            launch_mode: "resume".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        };
        PreparedIncarnation::create(context).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(logical, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, nonce).unwrap();
        let pins = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = pins.clone();
        let result = end_with(server.command(), logical, 8077, None, move |pin| {
            let mut seen = observed.lock().unwrap();
            seen.push(pin);
            let allowed = seen.len() == 1;
            async move { Ok(allowed) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(pins.lock().unwrap().len(), 2);
        let pin = pins.lock().unwrap()[0].clone();
        assert!(!pin.gone());
        assert_eq!(
            server
                .run(&[
                    "-N",
                    "display-message",
                    "-p",
                    "-t",
                    "=i8077-close-test:",
                    "#{pane_id}"
                ])
                .trim(),
            pin.pane
        );
        let mut waiter = std::process::Command::new("python3").args(["-c", r#"
import os, select, sys
pid = int(sys.argv[1])
try:
    if hasattr(os, 'pidfd_open'):
        fd = os.pidfd_open(pid)
        watch = select.poll()
        watch.register(fd, select.POLLIN)
        sys.exit(0 if watch.poll(5000) else 1)
    watch = select.kqueue()
    event = select.kevent(pid, filter=select.KQ_FILTER_PROC, flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT)
    sys.exit(0 if watch.control([event], 1, 5) else 1)
except ProcessLookupError:
    sys.exit(0)
"#, &pin.pid.to_string()]).spawn().unwrap();
        server.run(&["-N", "kill-server"]);
        assert!(
            waiter.wait().unwrap().success(),
            "provider exit event was not observed"
        );
        std::fs::remove_file(&marker).unwrap();
        assert!(pin.gone());
        assert!(
            end_with(server.command(), logical, 8077, Some(&pin), |_| async {
                Ok(true)
            })
            .await
            .is_ok()
        );
        drop(root_guard);
    }
}
