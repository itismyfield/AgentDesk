//! The user's herdr a start would run: from PATH, then the usual install dirs, never `~/.adk`; a
//! private native arm64 image on APFS reporting a supported stable version.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::model::{HERDR_PROTOCOL, supported_herdr_server};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub path: PathBuf,
    pub why: String,
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION_OUTPUT_LIMIT: usize = 4096;

pub(crate) fn search_dirs(home: &Path, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let from_path = path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute());
    let known = ["/opt/homebrew/bin", "/usr/local/bin"]
        .into_iter()
        .map(PathBuf::from)
        .chain([home.join(".local/bin"), home.join(".cargo/bin")]);
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in from_path.chain(known) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The first acceptable `herdr` in `dirs`, with why each earlier one was refused.
pub(crate) fn find(
    dirs: &[PathBuf],
    excluded: &[PathBuf],
    euid: u32,
) -> (Option<Candidate>, Vec<Rejection>) {
    let mut rejected = Vec::new();
    for dir in dirs {
        let path = dir.join("herdr");
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        match check(&path, excluded, euid) {
            Ok(candidate) => return (Some(candidate), rejected),
            Err(why) => rejected.push(Rejection { path, why }),
        }
    }
    (None, rejected)
}

fn check(path: &Path, excluded: &[PathBuf], euid: u32) -> Result<Candidate, String> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(path).map_err(|e| format!("unresolvable: {e}"))?;
    if excluded.iter().any(|dir| {
        real.starts_with(dir) || std::fs::canonicalize(dir).is_ok_and(|dir| real.starts_with(dir))
    }) {
        return Err("an AgentDesk-managed binary".into());
    }
    let meta = std::fs::metadata(&real).map_err(|e| format!("unreadable: {e}"))?;
    if !meta.is_file() || meta.mode() & 0o111 == 0 {
        return Err("not an executable regular file".into());
    }
    if meta.uid() != euid && meta.uid() != 0 {
        return Err("owned by another user".into());
    }
    if meta.mode() & 0o022 != 0 {
        return Err("group- or world-writable".into());
    }
    let mut header = [0u8; 8];
    std::fs::File::open(&real)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| format!("unreadable: {e}"))?;
    if !native_arm64(&header) {
        return Err("not a native arm64 single-architecture image".into());
    }
    if filesystem(&real).as_deref() != Some("apfs") {
        return Err("not on APFS".into());
    }
    let version = supported_version(&version_output(&real)?)
        .ok_or("not a supported stable herdr (0.9.3 or newer)")?;
    Ok(Candidate {
        path: path.into(),
        version,
    })
}

/// A 64-bit little-endian Mach-O for arm64; a universal (fat) image is refused.
pub(crate) fn native_arm64(header: &[u8]) -> bool {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;
    let word = |at: usize| {
        header
            .get(at..at + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    };
    word(0) == Some(MH_MAGIC_64) && word(4) == Some(CPU_TYPE_ARM64)
}

/// The version `herdr --version` printed, when it is a supported stable release.
pub(crate) fn supported_version(output: &str) -> Option<String> {
    let version = output.trim().strip_prefix("herdr ")?;
    supported_herdr_server(version, HERDR_PROTOCOL).then(|| version.into())
}

/// `herdr --version` run with an empty environment, bounded in time and output.
fn version_output(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("--version did not run: {e}"))?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("--version timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(VERSION_OUTPUT_LIMIT as u64)
            .read_to_end(&mut output)
            .map_err(|e| e.to_string())?;
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// The type name of the filesystem holding `path`, as `statfs` reports it.
#[cfg(target_os = "macos")]
pub(crate) fn filesystem(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and statfs writes one struct into `stat`.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs returned 0, so it filled the zeroed struct of integers and byte arrays.
    let stat = unsafe { stat.assume_init() };
    let name: Vec<u8> = stat
        .f_fstypename
        .iter()
        .map(|c| *c as u8)
        .take_while(|byte| *byte != 0)
        .collect();
    String::from_utf8(name).ok()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn filesystem(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
#[path = "candidate_tests.rs"]
mod tests;
