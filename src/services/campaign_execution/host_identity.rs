use std::io;
use std::process::{Command, Stdio};

use uuid::Uuid;

use crate::domain::campaign::model::HostGate;

const BOOT_PREFIX: &str = "boot:";

pub(super) fn observe(expected_host: &str, gate: &str) -> io::Result<HostGate> {
    let local = output(Command::new("/bin/hostname"))?;
    from_observation(expected_host, gate, &local, &boot_id()?)
}

pub(super) fn from_observation(
    expected_host: &str,
    gate: &str,
    local_host: &str,
    boot: &str,
) -> io::Result<HostGate> {
    if expected_host.is_empty() || local_host.trim() != expected_host || gate.trim().is_empty() {
        return Err(io::Error::other(
            "fixed campaign host or gate does not match",
        ));
    }
    let boot = Uuid::parse_str(boot.trim()).map_err(io::Error::other)?;
    if boot.is_nil() {
        return Err(io::Error::other("empty boot identity"));
    }
    Ok(HostGate {
        host: expected_host.to_owned(),
        gate: gate.to_owned(),
        incarnation: format!("{BOOT_PREFIX}{boot}"),
    })
}

pub(super) fn previous_boot(held: &HostGate, actual: &HostGate) -> bool {
    let boot = |gate: &HostGate| Uuid::parse_str(gate.incarnation.strip_prefix(BOOT_PREFIX)?).ok();
    held.host == actual.host
        && held.gate == actual.gate
        && matches!((boot(held), boot(actual)), (Some(a), Some(b)) if !a.is_nil() && !b.is_nil() && a != b)
}

fn output(mut command: Command) -> io::Result<String> {
    let result = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !result.status.success() {
        return Err(io::Error::other("host identity probe failed"));
    }
    String::from_utf8(result.stdout).map_err(io::Error::other)
}

#[cfg(target_os = "linux")]
fn boot_id() -> io::Result<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
}

#[cfg(target_os = "macos")]
fn boot_id() -> io::Result<String> {
    let mut command = Command::new("/usr/sbin/sysctl");
    command.args(["-n", "kern.bootsessionuuid"]);
    output(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot_id() -> io::Result<String> {
    Err(io::ErrorKind::Unsupported.into())
}
