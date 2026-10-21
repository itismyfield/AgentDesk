//! A bounded, isolated invocation of the supported binary's local config checker.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::Unverified;
use super::peer::PeerSnapshot;

const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: usize = 64 * 1024;
const HEALTHY_OUTPUT: &[u8] = b"config: ok\n";

pub(crate) async fn run(peer: &PeerSnapshot) -> Result<(), Unverified> {
    run_with_limits(peer, CHECK_TIMEOUT, OUTPUT_LIMIT).await
}

async fn read_capped(stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, Unverified> {
    let mut bytes = Vec::new();
    stream
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Unverified::Checker)?;
    if bytes.len() > limit {
        return Err(Unverified::Diagnostics);
    }
    Ok(bytes)
}

async fn run_with_limits(
    peer: &PeerSnapshot,
    timeout: Duration,
    limit: usize,
) -> Result<(), Unverified> {
    if !peer.checker.is_absolute() || !peer.home.is_absolute() || !peer.config.is_absolute() {
        return Err(Unverified::Ambiguous);
    }
    if peer.image.logical_path() != peer.checker {
        return Err(Unverified::Identity);
    }
    let image = peer.image.clone();
    tokio::task::spawn_blocking(move || image.fast_unchanged().map_err(|_| Unverified::Identity))
        .await
        .map_err(|_| Unverified::Cache)??;
    let mut child = Command::new(&peer.checker)
        .args(["config", "check"])
        .current_dir(&peer.home)
        .env_clear()
        .env("HOME", &peer.home)
        .env("HERDR_CONFIG_PATH", &peer.config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Unverified::Checker)?;
    let stdout = child.stdout.take().ok_or(Unverified::Checker)?;
    let stderr = child.stderr.take().ok_or(Unverified::Checker)?;
    let checked = tokio::time::timeout(timeout, async {
        let (stdout, stderr, status) = tokio::try_join!(
            read_capped(stdout, limit),
            read_capped(stderr, limit),
            async { child.wait().await.map_err(|_| Unverified::Checker) },
        )?;
        if !status.success() {
            return Err(Unverified::Checker);
        }
        if stdout != HEALTHY_OUTPUT || !stderr.is_empty() {
            return Err(Unverified::Diagnostics);
        }
        Ok(())
    })
    .await
    .unwrap_or(Err(Unverified::Timeout));
    if let Err(reason) = checked {
        // A capped stream may finish while the child is still writing. Reap it before
        // returning so failures cannot accumulate checker processes.
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(reason);
    }
    let image = peer.image.clone();
    tokio::task::spawn_blocking(move || image.fast_unchanged())
        .await
        .map_err(|_| Unverified::Cache)??;
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "checker_tests.rs"]
mod tests;
