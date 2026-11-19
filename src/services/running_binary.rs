use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::process_boot;

const MAX_RECORD: u64 = 4096;
static RUNNING_BINARY: OnceLock<Option<SourceRecord>> = OnceLock::new();

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    schema_version: u32,
    source_sha: String,
    target: String,
    sha256: String,
}

pub(crate) fn initialize() {
    RUNNING_BINARY.get_or_init(|| {
        let path = std::env::current_exe().ok()?;
        running_source(&path)
    });
}

/// Preserve a verified package record when `init` copies this running executable.
pub(crate) fn copy_current_to(source: &Path, destination: &Path) -> io::Result<()> {
    let record = running_source(source);
    let metadata = sidecar(destination);
    match fs::remove_file(&metadata) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::copy(source, destination)?;
    if let Some(record) = record {
        let mut digest = Sha256::new();
        io::copy(&mut File::open(destination)?, &mut digest)?;
        if format!("{:x}", digest.finalize()) == record.sha256 {
            let mut staged =
                tempfile::NamedTempFile::new_in(destination.parent().unwrap_or(Path::new(".")))?;
            serde_json::to_writer(&mut staged, &record)?;
            staged.persist(metadata)?;
        }
    }
    Ok(())
}

fn running_source(path: &Path) -> Option<SourceRecord> {
    verified(&sidecar(path), || current_image_digest(path))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn current_image_digest(path: &Path) -> Option<String> {
    use super::session_host::policy::{file, peer};

    let image = file::image(path).ok()?;
    #[cfg(target_os = "linux")]
    peer::verify_current_image(&image).ok()?;
    #[cfg(target_os = "macos")]
    peer::verify_current_release_image(&image).ok()?;
    Some(hex::encode(image.digest()))
}

#[cfg(windows)]
#[path = "running_binary/windows_image.rs"]
mod windows_image;

#[cfg(windows)]
use windows_image::current_image_digest;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn current_image_digest(_path: &Path) -> Option<String> {
    None
}

fn sidecar(binary: &Path) -> PathBuf {
    let mut path = binary.as_os_str().to_os_string();
    path.push(".source.json");
    path.into()
}

fn lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verified(record: &Path, image_digest: impl FnOnce() -> Option<String>) -> Option<SourceRecord> {
    if !fs::symlink_metadata(record).ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(record)
        .ok()?
        .take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD {
        return None;
    }
    let record: SourceRecord = serde_json::from_slice(&bytes).ok()?;
    if record.schema_version != 1
        || !lowercase_hex(&record.source_sha, 40)
        || !lowercase_hex(&record.sha256, 64)
        || !(1..=80).contains(&record.target.len())
        || !record
            .target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return None;
    }
    (image_digest()? == record.sha256).then_some(record)
}

pub(crate) fn attach(health: &mut Value, detailed: bool) {
    if detailed && let Some(record) = RUNNING_BINARY.get().and_then(Option::as_ref) {
        health["running_binary"] =
            witness(record, process_boot::initialize().started_at_ms, Utc::now());
    }
}

fn witness(record: &SourceRecord, boot_started_at_ms: i64, observed_at: DateTime<Utc>) -> Value {
    json!({
        "source_sha": record.source_sha,
        "target": record.target,
        "boot_started_at_ms": boot_started_at_ms,
        "observed_at": observed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_source_requires_metadata_for_the_open_executable() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("agentdesk");
        let metadata = sidecar(&binary);
        fs::write(&binary, "running image").unwrap();
        let image_digest = || {
            let bytes = fs::read(&binary).ok()?;
            Some(format!("{:x}", Sha256::digest(bytes)))
        };
        assert!(verified(&metadata, image_digest).is_none());

        let source = "a".repeat(40);
        let data = json!({
            "schema_version": 1,
            "source_sha": source,
            "target": "aarch64-apple-darwin",
            "sha256": format!("{:x}", Sha256::digest("running image")),
        });
        fs::write(&metadata, data.to_string()).unwrap();
        let record = verified(&metadata, image_digest).unwrap();
        let health = witness(&record, 123, Utc::now());
        assert_eq!(health["source_sha"], source);
        assert_eq!(health["target"], "aarch64-apple-darwin");
        assert_eq!(health["boot_started_at_ms"], 123);
        assert!(health["observed_at"].is_string());

        fs::write(&binary, "other image").unwrap();
        assert!(verified(&metadata, image_digest).is_none());
        fs::write(&metadata, vec![b' '; MAX_RECORD as usize + 1]).unwrap();
        assert!(verified(&metadata, image_digest).is_none());
    }
}
