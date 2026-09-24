//! Small, bounded host probes shared by GPU and network discovery.
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_OUTPUT_BYTES: u64 = 128 * 1024;

pub(super) async fn run(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut output = Vec::new();
        stdout
            .take(MAX_OUTPUT_BYTES + 1)
            .read_to_end(&mut output)
            .await
            .ok()?;
        if output.len() as u64 > MAX_OUTPUT_BYTES {
            return None;
        }
        child.wait().await.ok()?.success().then_some(output)
    })
    .await
    .ok()
    .flatten()
}
