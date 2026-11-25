//! One `agy acp` child per destination: its own process group, a child-only kill
//! token, and a `child.json` record so a restarted AgentDesk can clear an orphan.

use std::io::BufReader;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

use crate::services::platform::apply_binary_resolution;
use crate::services::process::reaping::CommandReapExt;
use crate::services::process::stream_child::StreamChild;
use crate::services::process::{ProcessIdentity, configure_child_process_group};
use crate::services::provider::CancelToken;

use super::Launch;
use super::actor::Event;
use super::rpc;

pub(super) const RECORD: &str = "child.json";

pub(super) struct Owned {
    pub stdin: Option<ChildStdin>,
    kill: Arc<CancelToken>,
}

impl Owned {
    /// Forced stop: the child-only token makes `StreamChild::wait` run its platform terminate.
    pub(super) fn terminate(&mut self) {
        self.stdin = None;
        self.kill.publish_cancel("agy_acp_terminate");
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    pid: u32,
    pgid: u32,
    starttime: Option<u128>,
    macos_lstart_hash: Option<u128>,
}

/// Lines and exit are reported as `incarnation` events so a later child never reads them; the
/// actor removes the record on its own child's exit.
pub(super) fn spawn(
    launch: &Launch,
    record: Option<&Path>,
    incarnation: u64,
    events: Sender<Event>,
) -> Result<Owned, String> {
    let mut command = Command::new(&launch.program);
    if let Some(resolution) = &launch.resolution {
        apply_binary_resolution(&mut command, resolution);
    }
    configure_child_process_group(&mut command);
    for key in &launch.unset_env {
        command.env_remove(key);
    }
    command.envs(launch.env.iter().map(|(key, value)| (key, value)));
    crate::config::test_instance::environment::configure_child_command(&mut command)?;
    let mut child = command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn_reaping()
        .map_err(|error| format!("Failed to start agy acp: {error}"))?;
    let kill = Arc::new(CancelToken::new());
    let mut lifecycle = StreamChild::new(&child, Some(kill.clone()), None);
    let recorded = record.map(|path| write_record(path, child.id()));
    let (Some(stdout), None | Some(Ok(()))) = (child.stdout.take(), &recorded) else {
        lifecycle.terminate(&mut child);
        return Err(match recorded {
            Some(Err(error)) => format!("could not record the agy acp child: {error}"),
            _ => "agy acp stdout is not piped".into(),
        });
    };
    let stdin = child.stdin.take();
    let lines = events.clone();
    std::thread::spawn(move || {
        rpc::read_lines(BufReader::new(stdout), |inbound| {
            lines
                .send(Event::Line {
                    incarnation,
                    inbound,
                })
                .is_ok()
        });
        let _ = lines.send(Event::Eof { incarnation });
    });
    std::thread::spawn(move || {
        let _ = lifecycle.wait(&mut child);
        let _ = events.send(Event::Exited { incarnation });
    });
    Ok(Owned { stdin, kill })
}

fn write_record(path: &Path, pid: u32) -> Result<(), String> {
    let identity = ProcessIdentity::capture(pid);
    let body = serde_json::to_string(&Record {
        pid,
        pgid: pid,
        starttime: identity.persisted_starttime(),
        macos_lstart_hash: identity.persisted_macos_lstart_hash(),
    })
    .map_err(|error| error.to_string())?;
    crate::domain::atomic_write::prepare(path, &body)
        .and_then(|prepared| prepared.publish())
        .map(drop)
        .map_err(|error| format!("{error:?}"))
}

pub(super) fn forget(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("could not remove {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Clears a crashed predecessor before this destination's first spawn. `Err` keeps
/// the record and holds the spawn; the next input judges again.
pub(super) fn sweep_recorded(path: &Path) -> Result<(), String> {
    let body = match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        Ok(body) => body,
    };
    let Ok(record) = serde_json::from_slice::<Record>(&body) else {
        tracing::warn!(path = %path.display(), "unreadable agy acp child record removed");
        return forget(path);
    };
    tracing::info!(pid = record.pid, pgid = record.pgid, starttime = ?record.starttime, lstart = ?record.macos_lstart_hash, "agy acp child record found before spawn");
    // Windows: the dcserver job closed the tree; a recorded PID cannot be verified, so no signal.
    #[cfg(unix)]
    sweep_group(&record)?;
    forget(path)
}

#[cfg(unix)]
fn sweep_group(record: &Record) -> Result<(), String> {
    let identity = ProcessIdentity::from_persisted(record.starttime, record.macos_lstart_hash);
    crate::services::process::orphan_group::end_recorded_group(record.pid, record.pgid, identity)
}
