//! Boot custody: before the boot reaper may unlink anything, copy into `discord_custody` what
//! each row, pending-start record and TUI-direct turn holds that no earlier boot preserved.

use super::*;
use crate::services::discord::runtime_store;
use crate::services::discord::tui_direct_pending_start::TuiDirectPendingStart;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// A transcript turn longer than this keeps only its path, offset and head hash.
const SEGMENT_COPY_CAP: u64 = 64 << 20;
const HEAD_HASH_BYTES: u64 = 64 << 10;
/// Transcript bytes one provider's boot pass may copy (tiny under test so tests reach it).
const BOOT_COPY_BUDGET: u64 = if cfg!(test) { 32 } else { 512 << 20 };
thread_local!(static BUDGET: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });

/// Kind, source path, bytes or read error of one file seen this boot, the turn it names, and
/// why its content could not be parsed.
struct Item(
    &'static str,
    PathBuf,
    Result<Vec<u8>, String>,
    Option<(PathBuf, u64)>,
    Option<&'static str>,
);

/// Copied digests and each transcript's latest copy across an episode's published revisions.
type Held = (BTreeSet<String>, BTreeMap<String, Value>);

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
    BUDGET.set(BOOT_COPY_BUDGET);
    let mut episodes: BTreeMap<String, (Value, Vec<Item>)> = BTreeMap::new();
    let mut add = |(key, item): (Value, Item)| {
        let entry = episodes.entry(sha(key.to_string().as_bytes()));
        entry.or_insert_with(|| (key, Vec::new())).1.push(item);
    };
    for path in json_files(&inflight_provider_dir(inflight_root, provider), provider) {
        let lock = lock_inflight_state_path(&path);
        if let Err(error) = &lock {
            let path = path.display();
            tracing::warn!(provider = provider.as_str(), %path, %error, "custody read a row unlocked");
        }
        let bytes = read_source(&path);
        drop(lock);
        if let Some(bytes) = bytes {
            add(row_item(provider, path, bytes));
        }
    }
    let pending = runtime_store::tui_direct_pending_start_root();
    for path in pending.map_or_else(Vec::new, |dir| json_files(&dir, provider)) {
        let bytes = read_source(&path);
        if let Some(item) = bytes.and_then(|bytes| pending_item(provider, path, bytes)) {
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

/// Publishes the episode marker once, then one revision with what no earlier one holds.
fn preserve_episode(dir: &Path, key: Value, items: Vec<Item>, boot: u64) -> Result<(), String> {
    let marker = dir.join("episode.json");
    if !marker.exists() {
        let tui_direct = items
            .iter()
            .any(|Item(kind, .., seg, _)| *kind != "row" || seg.is_some());
        let marker_json = json!({ "episode": key, "tui_direct": tui_direct, "first_boot": boot });
        runtime_store::atomic_write(&marker, &marker_json.to_string())?;
    }
    let (rev, (digests, transcripts)) = held_copies(dir)?;
    let (mut entries, mut segments) = (Vec::new(), BTreeMap::new());
    for Item(kind, source, bytes, segment, unparsed) in items {
        if let Some((transcript, offset)) = segment {
            let start = segments.entry(transcript).or_insert(offset);
            *start = offset.min(*start);
        }
        let digest = bytes.as_deref().ok().map(sha);
        if digest
            .as_ref()
            .is_some_and(|digest| digests.contains(digest))
        {
            continue;
        }
        let name = format!("{}-{kind}.json", entries.len());
        let copy = bytes.and_then(|bytes| write_synced(&rev, &name, &bytes).map(|()| name));
        let error = copy.as_ref().err().map(String::as_str).or(unparsed);
        entries.push(
            json!({ "kind": kind, "source": source.to_string_lossy(), "sha256": digest,
            "copy": copy.as_ref().ok(), "error": error }),
        );
    }
    for (source, offset) in segments {
        let name = format!("{}-transcript.part", entries.len());
        let prior = transcripts.get(source.to_string_lossy().as_ref());
        entries.extend(segment_entry(&rev, name, (&source, offset), prior));
    }
    let errors: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["error"].as_str())
        .collect();
    let complete = errors.is_empty();
    if !entries.is_empty() {
        let now = chrono::Utc::now().to_rfc3339();
        let manifest = json!({ "boot_generation": boot, "preserved_at": now,
            "complete": complete, "entries": entries });
        runtime_store::atomic_write(&rev.join("manifest.json"), &manifest.to_string())?;
    }
    match complete {
        true => Ok(()),
        false => Err(format!(
            "incomplete, retried by a boot that still finds its row or record: {}",
            errors.join("; ")
        )),
    }
}

/// The next revision directory and what published ones hold; an unlistable episode is skipped.
fn held_copies(dir: &Path) -> Result<(PathBuf, Held), String> {
    let mut revisions = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let name = entry.map_err(|error| error.to_string())?.file_name();
        let name = name.to_str().and_then(|name| name.strip_prefix("rev-"));
        revisions.extend(name.and_then(|index| index.parse::<u32>().ok()));
    }
    revisions.sort_unstable();
    let mut held = Held::default();
    for index in &revisions {
        let manifest = fs::read(dir.join(format!("rev-{index:04}/manifest.json")));
        let manifest: Option<Value> = manifest.ok().and_then(|m| serde_json::from_slice(&m).ok());
        let entries = manifest.and_then(|manifest| manifest["entries"].as_array().cloned());
        for mut entry in entries.unwrap_or_default() {
            if entry["copy"].is_string() && entry["kind"] == "transcript" {
                entry["rev"] = format!("rev-{index:04}").into();
                let source = entry["source"].as_str().unwrap_or_default().to_string();
                held.1.insert(source, entry);
            } else if entry["copy"].is_string() {
                held.0.extend(entry["sha256"].as_str().map(str::to_string));
            }
        }
    }
    let next = revisions.last().map_or(0, |last| last + 1);
    Ok((dir.join(format!("rev-{next:04}")), held))
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// `None` when the file vanished after listing; any other read failure is kept for the manifest.
fn read_source(path: &Path) -> Option<Result<Vec<u8>, String>> {
    match fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(read.map_err(|error| error.to_string())),
    }
}

/// Lists `*.json` in `dir`; a listing failure other than a missing directory is logged.
fn json_files(dir: &Path, provider: &ProviderKind) -> Vec<PathBuf> {
    let warn = |error: std::io::Error| {
        let (provider, dir) = (provider.as_str(), dir.display());
        tracing::warn!(provider, %dir, %error, "boot custody could not list sources");
    };
    let entries = match fs::read_dir(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        entries => entries.map_err(warn).into_iter().flatten(),
    };
    let paths = entries.filter_map(|entry| entry.map_err(warn).ok().map(|entry| entry.path()));
    paths
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect()
}

/// Unparseable bytes are keyed by their own hash and an unreadable file by its name.
fn unparsed_key(provider: &ProviderKind, path: &Path, bytes: &Result<Vec<u8>, String>) -> Value {
    json!({
        "provider": provider.as_str(),
        "malformed": path.file_name().map(|name| name.to_string_lossy()),
        "sha256": bytes.as_deref().ok().map(sha),
    })
}

fn row_item(
    provider: &ProviderKind,
    source: PathBuf,
    bytes: Result<Vec<u8>, String>,
) -> (Value, Item) {
    let text = bytes
        .as_deref()
        .ok()
        .and_then(|bytes| std::str::from_utf8(bytes).ok());
    let Some(row) = text.and_then(|text| parse_inflight_state_content(text).ok()) else {
        return (
            unparsed_key(provider, &source, &bytes),
            Item("row", source, bytes, None, Some("the row does not parse")),
        );
    };
    let owner = crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
    let tui_direct =
        row.request_owner_user_id == owner && row.turn_source == TurnSource::ExternalInput;
    let segment = tui_direct.then(|| {
        let transcript = PathBuf::from(row.output_path.clone().unwrap_or_default());
        (transcript, row.turn_start_offset.unwrap_or(0))
    });
    let started = || json!([row.started_at, row.turn_start_offset]);
    let turn = row.turn_nonce.clone().filter(|nonce| !nonce.is_empty());
    let anchorless = (row.user_msg_id == 0).then(|| turn.map_or_else(started, Value::from));
    let key = episode_key(provider, [row.channel_id, row.user_msg_id], anchorless);
    (key, Item("row", source, bytes, segment, None))
}

/// Records of another provider are left to that provider's reaper pass.
fn pending_item(
    provider: &ProviderKind,
    source: PathBuf,
    bytes: Result<Vec<u8>, String>,
) -> Option<(Value, Item)> {
    let record = bytes.as_deref().ok();
    let record =
        record.and_then(|bytes| serde_json::from_slice::<TuiDirectPendingStart>(bytes).ok());
    let Some(record) = record else {
        let name = source.file_stem()?.to_string_lossy().into_owned();
        let ids = name.strip_prefix(&format!("{}_", provider.as_str()))?;
        // The writer names a record `<provider>_<channel>_<anchor>.json`.
        let ids = ids
            .split_once('_')
            .and_then(|(channel, anchor)| Some([channel.parse().ok()?, anchor.parse().ok()?]));
        let key = match ids {
            Some(ids) => episode_key(provider, ids, None),
            None => unparsed_key(provider, &source, &bytes),
        };
        let unparsed = Some("the record does not parse");
        return Some((key, Item("pending_start", source, bytes, None, unparsed)));
    };
    (record.provider == provider.as_str()).then_some(())?;
    let key = episode_key(
        provider,
        [record.channel_id, record.anchor_message_id],
        None,
    );
    let segment = record
        .captured_source
        .map(|(path, offset)| (path.into(), offset));
    Some((key, Item("pending_start", source, bytes, segment, None)))
}

/// The row claimed from a pending-start record keeps its anchor as `user_msg_id`, so both
/// share this key; an anchorless row is told apart by its turn nonce, else start and offset.
fn episode_key(provider: &ProviderKind, ids: [u64; 2], anchorless: Option<Value>) -> Value {
    let [channel_id, anchor_id] = ids;
    let provider = provider.as_str();
    json!({ "provider": provider, "channel_id": channel_id, "anchor_id": anchor_id,
        "anchorless": anchorless })
}

fn write_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut file = fs::File::create(dir.join(name))?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    write().map_err(|error| error.to_string())
}

/// Copies what `[offset, EOF)` holds past the latest copy of the same file; `None` if nothing
/// is new. A replaced, truncated or rewritten file is copied again and flagged as changed.
fn segment_entry(
    dir: &Path,
    name: String,
    turn: (&Path, u64),
    prior: Option<&Value>,
) -> Option<Value> {
    let (source, offset) = turn;
    let mut entry = json!({ "kind": "transcript", "source": source.to_string_lossy(),
        "offset": offset, "copy": null, "error": null });
    match copy_segment(dir, &name, turn, prior, &mut entry) {
        Ok(false) => return None,
        Ok(true) if entry["source_changed"] == true => {
            entry["copy"] = name.into();
            entry["error"] = "the transcript changed since the last copy".into();
        }
        Ok(true) => entry["copy"] = name.into(),
        Err(e) => (entry["error"], entry["supersedes"]) = (e.to_string().into(), Value::Null),
    }
    Some(entry)
}

/// Resumes at the latest copy's end while the file keeps its identity and head; `false` when
/// nothing is new.
fn copy_segment(
    dir: &Path,
    name: &str,
    (source, offset): (&Path, u64),
    prior: Option<&Value>,
    entry: &mut Value,
) -> std::io::Result<bool> {
    let mut file = fs::File::open(source)?;
    let (metadata, mut head) = (file.metadata()?, Vec::new());
    (&mut file).take(HEAD_HASH_BYTES).read_to_end(&mut head)?;
    let ((dev, ino), size) = (file_identity(&metadata), metadata.len());
    let same_head = |prior: &Value| {
        let len = prior["head_len"]
            .as_u64()
            .and_then(|len| usize::try_from(len).ok());
        let prior_head = len.and_then(|len| head.get(..len));
        prior_head.is_some_and(|bytes| prior["head_sha256"] == sha(bytes))
    };
    let same_file = |prior: &&Value| prior["dev"] == dev && prior["ino"] == ino && same_head(prior);
    let kept = prior.filter(same_file);
    let kept = kept.filter(|prior| prior["to"].as_u64().is_some_and(|to| to <= size));
    // Earlier copies cover this turn only when they start at or before its offset.
    let start = kept.and_then(|prior| prior["start"].as_u64());
    let start = start.filter(|start| *start <= offset);
    let resume = start.and(kept).and_then(|prior| prior["to"].as_u64());
    let from = resume.unwrap_or(offset);
    let supersedes = prior.filter(|_| resume.is_none());
    let fields = json!({ "dev": dev, "ino": ino, "size": size, "head_len": head.len(),
        "head_sha256": sha(&head), "start": start.unwrap_or(offset), "from": from, "to": size,
        "source_changed": prior.is_some() && kept.is_none(),
        "supersedes": supersedes.map(|prior| json!([prior["rev"], prior["copy"]])) });
    if let (Some(entry), Value::Object(fields)) = (entry.as_object_mut(), fields) {
        entry.extend(fields);
    }
    if resume == Some(size) {
        return Ok(false);
    }
    let len = size.checked_sub(from);
    let len = len.ok_or_else(|| std::io::Error::other("turn start is past EOF"))?;
    if len > SEGMENT_COPY_CAP {
        return Err(std::io::Error::other("turn exceeds the copy cap"));
    }
    let left = BUDGET.get().checked_sub(len);
    let left = left.ok_or("boot copy budget exhausted");
    BUDGET.set(left.map_err(std::io::Error::other)?);
    #[cfg(test)]
    super::boot_custody_tests::before_segment_copy(source);
    file.seek(SeekFrom::Start(from))?;
    fs::create_dir_all(dir)?;
    let mut copy = fs::File::create(dir.join(name))?;
    let copied = std::io::copy(&mut file.take(len), &mut copy)?;
    copy.sync_all()?;
    match copied == len {
        true => Ok(true),
        false => Err(std::io::Error::other(format!(
            "short copy: {copied} of {len} bytes"
        ))),
    }
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
