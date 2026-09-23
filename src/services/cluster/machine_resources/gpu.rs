use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::GpuResources;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

// Driver utilities must never hold the collector indefinitely. The commands
// return a small device list, not per-process or identifying information.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_OUTPUT_BYTES: u64 = 128 * 1024;
#[cfg(any(test, not(target_os = "macos")))]
const MIB_BYTES: f64 = 1024.0 * 1024.0;

#[derive(Default)]
pub(super) struct GpuSampler {
    previous: Vec<GpuResources>,
}

impl GpuSampler {
    pub(super) async fn collect(&mut self) -> Vec<GpuResources> {
        #[cfg(target_os = "macos")]
        let result = macos::collect().await;
        #[cfg(not(target_os = "macos"))]
        let result = other::collect().await;
        if let Some(devices) = result {
            self.previous = devices;
        } else {
            // Retain static device names across driver errors, never old load.
            for gpu in &mut self.previous {
                gpu.usage_percent = None;
                gpu.memory_used_bytes = None;
            }
        }
        self.previous.clone()
    }
}

async fn command(program: &str, args: &[&str]) -> Option<Vec<u8>> {
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

#[cfg(any(test, not(target_os = "macos")))]
pub(super) fn nvidia_rows(raw: &str) -> Vec<GpuResources> {
    raw.lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 4 || fields[0].is_empty() {
                return None;
            }
            let mib = |raw: &str| {
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v <= u64::MAX as f64 / MIB_BYTES)
                    .map(|v| (v * MIB_BYTES) as u64)
            };
            Some(GpuResources {
                name: fields[0].into(),
                memory_total_bytes: mib(fields[1]),
                memory_used_bytes: mib(fields[2]),
                usage_percent: fields[3].parse().ok().and_then(super::valid_percent),
                shared_memory: false,
            })
        })
        .collect()
}
