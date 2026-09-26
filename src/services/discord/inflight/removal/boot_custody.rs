//! Boot custody: before the boot reaper may unlink anything, copy every row's raw
//! bytes, pending-start records and TUI-direct transcript turns into `discord_custody`.

use super::*;
use crate::services::discord::runtime_store;
use crate::services::discord::tui_direct_pending_start::TuiDirectPendingStart;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// A transcript turn longer than this keeps only its path, offset and head hash.
const SEGMENT_COPY_CAP: u64 = 64 << 20;
const HEAD_HASH_BYTES: u64 = 64 << 10;

/// Kind, source path and raw bytes of one file to copy, plus the transcript turn it names.
struct Item(&'static str, PathBuf, Vec<u8>, Option<(PathBuf, u64)>);

/// Fail-open: a custody error or panic is only logged, so the reaper still runs.
pub(super) fn preserve_before_boot_reap(inflight_root: &Path, provider: &ProviderKind) {
    let pass = std::panic::AssertUnwindSafe(|| preserve(inflight_root, provider));
    if std::panic::catch_unwind(pass).is_err() {
        tracing::warn!(provider = provider.as_str(), "boot custody copy panicked");
    }
}

fn preserve(inflight_root: &Path, provider: &ProviderKind) {
    let Some(root) = runtime_store::runtime_root() else {
        return;
    };
    let custody = root.join("discord_custody").join(provider.as_str());
    let mut episodes: BTreeMap<String, (serde_json::Value, Vec<Item>)> = BTreeMap::new();
    let mut add = |(key, item): (serde_json::Value, Item)| {
        let digest = format!("{:x}", Sha256::digest(key.to_string()));
        episodes
            .entry(digest)
            .or_insert_with(|| (key, Vec::new()))
            .1
            .push(item);
    };
    for path in json_files(&inflight_provider_dir(inflight_root, provider)) {
        let bytes = {
            let _lock = lock_inflight_state_path(&path).ok();
            fs::read(&path)
        };
        if let Ok(bytes) = bytes {
            add(row_item(provider, path, bytes));
        }
    }
    let pending = runtime_store::tui_direct_pending_start_root();
    for path in pending.map(|dir| json_files(&dir)).unwrap_or_default() {
        let item = fs::read(&path).ok();
        if let Some(item) = item.and_then(|bytes| pending_item(provider, path, bytes)) {
            add(item);
        }
    }
    let boot_generation = runtime_store::process_generation_binding().generation;
    for (digest, (key, items)) in episodes {
        let dir = custody.join(digest);
        if let Err(error) = preserve_episode(&dir, key, items, boot_generation) {
            let dir = dir.display();
            tracing::warn!(provider = provider.as_str(), %dir, %error, "boot custody copy failed");
        }
    }
}

/// Writes the copies, then the manifest; the manifest doubles as the episode marker.
fn preserve_episode(
    dir: &Path,
    key: serde_json::Value,
    items: Vec<Item>,
    boot_generation: u64,
) -> Result<(), String> {
    let manifest_path = dir.join("manifest.json");
    if manifest_path.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for Item(kind, source, bytes, segment) in items {
        let name = format!("{}-{kind}.json", entries.len());
        let written = fs::write(dir.join(&name), bytes).map(|()| name);
        entries.push(copy_entry(kind, &source, written));
        if let Some((transcript, offset)) = segment {
            let name = format!("{}-transcript.part", entries.len());
            entries.push(segment_entry(dir, name, &transcript, offset));
        }
    }
    let manifest = serde_json::json!({
        "episode": key,
        "boot_generation": boot_generation,
        "preserved_at": chrono::Utc::now().to_rfc3339(),
        "entries": entries,
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(&manifest_path, &text)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let entries = fs::read_dir(dir).into_iter().flatten().flatten();
    let paths = entries.map(|entry| entry.path());
    paths
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect()
}

/// Unparseable bytes are keyed by their own hash, so each distinct content is one episode.
fn malformed_key(provider: &ProviderKind, path: &Path, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "provider": provider.as_str(),
        "malformed": path.file_name().map(|name| name.to_string_lossy()),
        "sha256": format!("{:x}", Sha256::digest(bytes)),
    })
}

fn row_item(provider: &ProviderKind, source: PathBuf, bytes: Vec<u8>) -> (serde_json::Value, Item) {
    let text = std::str::from_utf8(&bytes).ok();
    let Some(row) = text.and_then(|text| parse_inflight_state_content(text).ok()) else {
        return (
            malformed_key(provider, &source, &bytes),
            Item("row", source, bytes, None),
        );
    };
    let owner = crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
    let tui_direct =
        row.request_owner_user_id == owner && row.turn_source == TurnSource::ExternalInput;
    let segment = tui_direct.then(|| {
        let transcript = PathBuf::from(row.output_path.clone().unwrap_or_default());
        (transcript, row.turn_start_offset.unwrap_or(0))
    });
    let key = episode_key(
        provider,
        [row.channel_id, row.user_msg_id],
        row.turn_start_offset,
        row.external_turn_id.as_deref(),
        row.output_path.as_deref(),
    );
    (key, Item("row", source, bytes, segment))
}

/// Records of another provider are left to that provider's reaper pass.
fn pending_item(
    provider: &ProviderKind,
    source: PathBuf,
    bytes: Vec<u8>,
) -> Option<(serde_json::Value, Item)> {
    let kind = "pending_start";
    let Ok(record) = serde_json::from_slice::<TuiDirectPendingStart>(&bytes) else {
        let name = source.file_name()?.to_string_lossy();
        let prefix = format!("{}_", provider.as_str());
        name.starts_with(&prefix).then_some(())?;
        let key = malformed_key(provider, &source, &bytes);
        return Some((key, Item(kind, source, bytes, None)));
    };
    (record.provider == provider.as_str()).then_some(())?;
    let captured = record.captured_source.as_ref();
    let key = episode_key(
        provider,
        [record.channel_id, record.anchor_message_id],
        captured.map(|(_, offset)| *offset),
        record.lease_turn_id.as_deref(),
        captured.map(|(path, _)| path.as_str()),
    );
    let segment = record
        .captured_source
        .map(|(path, offset)| (path.into(), offset));
    Some((key, Item(kind, source, bytes, segment)))
}

/// A claimed row and the pending-start record it was claimed from share this key.
fn episode_key(
    provider: &ProviderKind,
    [channel_id, anchor_id]: [u64; 2],
    turn_start_offset: Option<u64>,
    external_turn_id: Option<&str>,
    output_path: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "provider": provider.as_str(),
        "channel_id": channel_id,
        "anchor_id": anchor_id,
        "turn_start_offset": turn_start_offset,
        "external_turn_id": external_turn_id,
        "output_path": output_path,
    })
}

fn copy_entry(kind: &str, source: &Path, copy: std::io::Result<String>) -> serde_json::Value {
    let (copy, error) = match copy {
        Ok(name) => (Some(name), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let source = source.to_string_lossy();
    serde_json::json!({ "kind": kind, "source": source, "copy": copy, "error": error })
}

/// Copies transcript bytes `[offset, EOF)` as of the stat and records the source identity.
fn segment_entry(dir: &Path, name: String, source: &Path, offset: u64) -> serde_json::Value {
    let mut entry = serde_json::json!({ "offset": offset });
    let mut copy = || -> std::io::Result<String> {
        let mut file = fs::File::open(source)?;
        let (metadata, mut head) = (file.metadata()?, Vec::new());
        (&mut file).take(HEAD_HASH_BYTES).read_to_end(&mut head)?;
        let ((dev, ino), size) = (file_identity(&metadata), metadata.len());
        let head_sha256 = format!("{:x}", Sha256::digest(&head));
        entry = serde_json::json!({ "offset": offset, "dev": dev, "ino": ino, "size": size,
            "head_sha256": head_sha256 });
        let len = size.checked_sub(offset);
        let len = len.ok_or_else(|| std::io::Error::other("turn start is past EOF"))?;
        if len > SEGMENT_COPY_CAP {
            return Err(std::io::Error::other("turn exceeds the copy cap"));
        }
        file.seek(SeekFrom::Start(offset))?;
        std::io::copy(&mut file.take(len), &mut fs::File::create(dir.join(&name))?)?;
        Ok(name.clone())
    };
    let copied = copy_entry("transcript", source, copy());
    if let (Some(fields), serde_json::Value::Object(copied)) = (entry.as_object_mut(), copied) {
        fields.extend(copied);
    }
    entry
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}
