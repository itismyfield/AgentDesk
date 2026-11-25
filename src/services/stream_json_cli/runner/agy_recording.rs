use std::path::Path;
use std::process::Command;

use crate::services::agent_protocol::StreamMessage;
use crate::services::tui_o::store::agy_spool::{AgySpool, Failure};
use uuid::Uuid;

use super::StreamJsonCodec;

pub(super) struct Recording<'a> {
    pub(super) spool: &'a mut AgySpool,
    pub(super) failure: Failure,
    conversation: Option<Uuid>,
    storage_failed: bool,
}

impl<'a> Recording<'a> {
    pub(super) fn new(spool: &'a mut AgySpool) -> Self {
        Self {
            conversation: spool.resume(),
            spool,
            failure: Failure::Spawn,
            storage_failed: false,
        }
    }

    /// Commits `launch` before spawn; a missing executable stays a spawn failure with no launch.
    pub(super) fn launch(&mut self, executable: &Path) -> Result<(), String> {
        if !executable.is_file() {
            return Err(format!(
                "Failed to start StreamJson CLI: {} is not a file",
                executable.display()
            ));
        }
        self.spool
            .launch()
            .map_err(|error| format!("agy launch was not recorded: {error}"))
    }

    /// Commits the child's identity, then lets the gate exec; a failed commit ends the group.
    #[cfg(unix)]
    pub(super) fn start(&mut self, child: &mut std::process::Child) -> Result<(), String> {
        use std::io::Write;
        let pid = child.id();
        let identity = crate::services::process::ProcessIdentity::capture(pid);
        let go = self
            .spool
            .started(pid, &identity)
            .map_err(|error| format!("agy start was not recorded: {error}"))
            .and_then(|()| {
                let mut stdin = child.stdin.take().ok_or("agy gate stdin is missing")?;
                stdin
                    .write_all(b"go\n")
                    .map_err(|error| format!("agy gate did not take go: {error}"))
            });
        if go.is_err() {
            self.failure = Failure::Spawn;
            crate::services::process::kill_pid_tree(pid);
        }
        go
    }

    pub(super) fn message(&mut self, message: &StreamMessage) {
        if self.storage_failed {
            return;
        }
        let result = match message {
            StreamMessage::Init { session_id, .. } => {
                let Ok(id) = Uuid::parse_str(session_id) else {
                    return;
                };
                if self.conversation.is_some_and(|expected| expected != id) {
                    return;
                }
                self.conversation = Some(id);
                self.spool.init(id)
            }
            StreamMessage::Text { content } => self.spool.delta(content),
            _ => return,
        };
        self.record(result);
    }

    pub(super) fn done(&mut self, codec: &mut dyn StreamJsonCodec) {
        if self.storage_failed {
            return;
        }
        let Some((conversation, body)) = codec.captured_body() else {
            return;
        };
        let result = self.spool.done(conversation, &body);
        self.record(result);
    }

    fn record(&mut self, result: std::io::Result<()>) {
        if let Err(error) = result {
            self.storage_failed = true;
            tracing::warn!(%error, "agy spool remains unsealed");
        }
    }
}

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        let result = self.spool.fail(self.failure);
        self.record(result);
    }
}

/// The recorded child waits for one stdin line before exec; EOF instead exits 75 and runs nothing.
pub(super) fn gated(executable: &Path, args: &[String]) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "IFS= read -r _ || exit 75; exec \"$0\" \"$@\""])
        .arg(executable)
        .args(args);
    command
}
