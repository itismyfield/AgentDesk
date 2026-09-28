//! Read-only fold of the boot-custody ledger into one preservation obligation per transcript
//! source, printed by `adk custody status`. Transcripts are assumed to be append-only.

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The boot copier's per-copy cap; a copy plan longer than this is not attempted.
const COPY_CAP: u64 = 64 << 20;
/// Bytes compared at the preserved end, once on the source and once in the copy.
const JOIN_WINDOW: u64 = 4 << 10;
const CHANGED: &str = "unresolved_source_changed";
const HISTORY: &str = "history_unresolved";
const START: &str = "required_from_unknown";
/// Keys a transcript entry has only once its source opened; its observation must then parse.
const SEEN: &str = "dev ino size head_len head_sha256 source_changed copy";

/// A source's identity as one attempt saw it; `g_prefix_sha` hashes the first bytes of the
/// obligation's generation when the writer knew that generation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Observation {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) head_len: u64,
    pub(crate) head_sha256: String,
    pub(crate) g_prefix_sha: Option<String>,
}

/// `intent.json` (written before a copy) and `outcome.json` (after it, published or not).
#[derive(Deserialize)]
struct Records<T> {
    sources: Vec<T>,
}

#[derive(Deserialize)]
struct Intent {
    source: String,
    required_from: u64,
    pre: Option<Observation>,
}

#[derive(Deserialize)]
struct Outcome {
    source: String,
    result: String,
    post: Option<Observation>,
    copy: Option<String>,
    from: u64,
    to: u64,
}

/// A copy of source bytes `[from, to)` and where it is.
type Copy = Option<(u64, u64, PathBuf)>;

/// One transcript source's obligation and what the ledger and the source show now.
#[derive(Debug, Default)]
pub(crate) struct SourceStatus {
    pub(crate) source: String,
    pub(crate) required_from: Option<u64>,
    pub(crate) max_eof: u64,
    pub(crate) generation: Option<Observation>,
    pub(crate) flags: BTreeSet<&'static str>,
    pub(crate) preserved: Vec<(u64, u64)>,
    pub(crate) missing: Vec<(u64, u64)>,
    pub(crate) current: String,
    pub(crate) last_attempt: Option<String>,
    /// Bytes read from the source and the copy to judge `current`.
    pub(crate) verify_read: u64,
    copies: Vec<(u64, u64, PathBuf)>,
}

#[derive(Debug, Default)]
pub(crate) struct EpisodeStatus {
    pub(crate) id: String,
    pub(crate) channel_id: Option<u64>,
    pub(crate) flags: BTreeSet<&'static str>,
    pub(crate) sources: Vec<SourceStatus>,
    raw_held: BTreeMap<String, bool>,
}

impl EpisodeStatus {
    /// Everything required up to the current EOF is held and nothing is unresolved.
    pub(crate) fn complete(&self) -> bool {
        let held = |s: &SourceStatus| s.flags.is_empty() && s.current == "complete_to_eof";
        self.flags.is_empty() && self.sources.iter().all(held)
    }
}

fn name(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default();
    name.to_string_lossy().into_owned()
}

fn episode_status(dir: &Path) -> EpisodeStatus {
    let mut episode = EpisodeStatus::default();
    episode.id = name(dir);
    match ledger_file::<Value>(&dir.join("episode.json")) {
        Some(Ok(marker)) => episode.channel_id = marker["episode"]["channel_id"].as_u64(),
        _ => _ = episode.flags.insert("inventory_unresolved"),
    }
    let Ok(revisions) = revisions(dir) else {
        episode.flags.insert("unreadable_episode");
        return episode;
    };
    if revisions.is_empty() {
        episode.flags.insert("inventory_unresolved");
    }
    for rev in revisions {
        fold_revision(&mut episode, &dir.join(&rev), &rev);
    }
    if episode.raw_held.values().any(|held| !held) {
        episode.flags.insert("bytes_copy_failed");
    }
    episode.sources.iter_mut().for_each(judge_now);
    episode
}

fn revisions(dir: &Path) -> io::Result<Vec<String>> {
    let mut revisions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let index = name.strip_prefix("rev-").map(str::parse::<u32>);
        revisions.extend(index.and_then(Result::ok).map(|index| (index, name)));
    }
    revisions.sort();
    Ok(revisions.into_iter().map(|(_, name)| name).collect())
}

/// Reads one optional ledger file: `None` when no entry is there, else `Some(Err)` if unusable.
fn ledger_file<T: DeserializeOwned>(path: &Path) -> Option<Result<T, ()>> {
    let bytes = fs::read(path).map_err(|_| fs::symlink_metadata(path));
    if matches!(&bytes, Err(Err(error)) if error.kind() == io::ErrorKind::NotFound) {
        return None;
    }
    let parsed = bytes.ok().and_then(|b| serde_json::from_slice(&b).ok());
    Some(parsed.ok_or(()))
}

/// Applies one revision. With an intent or outcome it is an attempt whose copy only an `ok`
/// outcome holds, and its manifest must agree; else the manifest entry is the final record.
fn fold_revision(ep: &mut EpisodeStatus, dir: &Path, rev: &str) {
    let intent = ledger_file::<Records<Intent>>(&dir.join("intent.json"));
    let manifest = ledger_file::<Value>(&dir.join("manifest.json"));
    let outcome = ledger_file::<Records<Outcome>>(&dir.join("outcome.json"));
    if intent.is_none() && manifest.is_none() {
        ep.flags.insert("inventory_unresolved");
    }
    for source in intent.iter().flatten().flat_map(|intent| &intent.sources) {
        let status = source_mut(ep, &source.source);
        status.see(Some(source.required_from), source.pre.as_ref(), false);
        status.last_attempt = Some(format!("{rev}: incomplete (no outcome)"));
    }
    let entries = manifest.map(|m| m.ok().and_then(|m| m["entries"].as_array().cloned()));
    if matches!(entries, Some(None)) || matches!(intent, Some(Err(()))) {
        ep.flags.insert(HISTORY);
    }
    let (attempt, mut claims) = (intent.is_some() || outcome.is_some(), BTreeMap::new());
    for entry in entries.flatten().unwrap_or_default() {
        let (Some(kind), Some(source)) = (entry["kind"].as_str(), entry["source"].as_str()) else {
            ep.flags.insert(HISTORY);
            continue;
        };
        if kind != "transcript" {
            let (mut digest, copy) = (Sha256::new(), entry["copy"].as_str().unwrap_or_default());
            let read = open_copy(&dir.join(copy)).and_then(|mut f| io::copy(&mut f, &mut digest));
            // A row or pending-start copy is held only as a readable file with its digest.
            let held = read.is_ok() && entry["sha256"] == format!("{:x}", digest.finalize());
            ep.raw_held.insert(source.to_string(), held);
            continue;
        }
        let seen = serde_json::from_value::<Observation>(entry.clone()).ok();
        let torn = seen.is_none() && SEEN.split(' ').any(|key| !entry[key].is_null());
        let record = legacy_record(&entry, dir).filter(|_| !torn);
        ep.flags.extend(record.is_none().then_some(HISTORY));
        let (status, changed) = (source_mut(ep, source), entry["source_changed"] == true);
        // An unreadable turn start is taken as the first byte, never as EOF.
        let offset = entry["offset"].as_u64();
        status.flags.extend(offset.is_none().then_some(START));
        status.see(Some(offset.unwrap_or(0)), seen.as_ref(), changed);
        let (result, copy) = record.unwrap_or(("malformed", None));
        match attempt {
            true => _ = claims.insert(source.to_string(), copy.filter(|_| result == "ok")),
            false => status.done(rev, result, None, copy.filter(|_| result == "ok")),
        }
    }
    match outcome {
        Some(Ok(outcome)) => {
            let listed: Vec<_> = outcome.sources.iter().map(|s| s.source.clone()).collect();
            for source in outcome.sources {
                let copy = source.copy.filter(|_| source.result == "ok");
                let copy = copy.map(|copy| (source.from, source.to, dir.join(copy)));
                // Held only for a source listed once, whose manifest entry, if any, claims it.
                let once = listed.iter().filter(|s| **s == source.source).count() == 1;
                let claim = claims.remove(&source.source);
                let agrees = once && claim.is_none_or(|claim| claim == copy);
                ep.flags.extend((!agrees).then_some(HISTORY));
                let (status, copy) = (source_mut(ep, &source.source), copy.filter(|_| agrees));
                // A source first named here has an unknown start: byte 0 stands in, flagged.
                let unknown = status.required_from.is_none();
                status.flags.extend(unknown.then_some(START));
                status.required_from.get_or_insert(0);
                status.done(rev, &source.result, source.post.as_ref(), copy);
            }
            ep.flags.extend((!claims.is_empty()).then_some(HISTORY));
        }
        // Without outcomes the last attempts, and so the fairness order, are unknown.
        Some(Err(())) => ep.flags.extend([HISTORY, "fairness_lost"]),
        None => {}
    }
}

/// A legacy transcript entry's result (its `error`, or ok) and copy; `None` when misshapen.
fn legacy_record<'a>(entry: &'a Value, dir: &Path) -> Option<(&'a str, Copy)> {
    let result = match &entry["error"] {
        Value::Null => "ok",
        error => error.as_str()?,
    };
    let copy = match (&entry["copy"], entry["from"].as_u64(), entry["to"].as_u64()) {
        (Value::Null, ..) => None,
        (Value::String(copy), Some(from), Some(to)) => Some((from, to, dir.join(copy))),
        _ => return None,
    };
    let changed = matches!(entry["source_changed"], Value::Null | Value::Bool(_));
    changed.then_some((result, copy))
}

/// Opens a copy only when it is a regular file.
fn open_copy(path: &Path) -> io::Result<fs::File> {
    match fs::metadata(path)?.is_file() {
        true => fs::File::open(path),
        false => Err(io::ErrorKind::InvalidInput.into()),
    }
}

fn source_mut<'a>(episode: &'a mut EpisodeStatus, source: &str) -> &'a mut SourceStatus {
    let found = episode.sources.iter().position(|s| s.source == source);
    let index = found.unwrap_or(episode.sources.len());
    if index == episode.sources.len() {
        let mut status = SourceStatus::default();
        status.source = source.to_string();
        episode.sources.push(status);
    }
    &mut episode.sources[index]
}

impl SourceStatus {
    /// Adds a requirement and an observation; any evidence against append-only growth of the
    /// first observed generation is sticky.
    fn see(&mut self, required_from: Option<u64>, seen: Option<&Observation>, changed: bool) {
        if let Some(from) = required_from {
            self.required_from = Some(self.required_from.map_or(from, |min| min.min(from)));
        }
        self.flags.extend(changed.then_some(CHANGED));
        let Some(seen) = seen else {
            if self.generation.is_none() {
                self.flags.insert("first_seen_after_failure");
            }
            return;
        };
        let generation = self.generation.get_or_insert_with(|| seen.clone());
        let prefix_kept = match &seen.g_prefix_sha {
            Some(prefix) => *prefix == generation.head_sha256,
            None => {
                seen.head_len != generation.head_len || seen.head_sha256 == generation.head_sha256
            }
        };
        let moved = (seen.dev, seen.ino) != (generation.dev, generation.ino);
        if moved || seen.size < self.max_eof || !prefix_kept {
            self.flags.insert(CHANGED);
        }
        self.max_eof = self.max_eof.max(seen.size);
    }

    /// Records an attempt's result; its `ok` copy counts only as a readable file of the claimed
    /// length with no change seen up to this attempt. A failed post-copy check is a change.
    fn done(&mut self, rev: &str, result: &str, post: Option<&Observation>, copy: Copy) {
        if result == "verify_failed" {
            self.flags.insert(CHANGED);
        }
        if post.is_some() {
            self.see(None, post, false);
        }
        self.last_attempt = Some(format!("{rev}: {result}"));
        let Some((from, to, path)) = copy.filter(|_| !self.flags.contains(CHANGED)) else {
            return;
        };
        let meta = open_copy(&path).and_then(|copy| copy.metadata());
        if meta.is_ok_and(|meta| Some(meta.len()) == to.checked_sub(from)) {
            self.max_eof = self.max_eof.max(to);
            self.copies.push((from, to, path));
        }
    }
}

/// Opens the source once and judges it against the first observed generation: head prefix,
/// size and the last preserved `JOIN_WINDOW`; interior bytes are not re-read.
fn judge_now(status: &mut SourceStatus) {
    let ranges = status.copies.iter().map(|copy| (copy.0, copy.1));
    status.preserved = union(ranges.collect());
    let (generation, changed) = (status.generation.clone(), status.flags.contains(CHANGED));
    let live = generation.as_ref().filter(|_| !changed);
    let seen = live.map_or(Ok(None), |generation| observe(status, generation));
    let now = seen.as_ref().ok().copied().flatten();
    let end = status.max_eof.max(now.unwrap_or(0));
    let required_from = status.required_from.unwrap_or(0);
    if generation.is_some() && required_from > end {
        status.flags.insert("required_past_eof");
    }
    status.missing = subtract((required_from, end), &status.preserved);
    status.current = match (&seen, status.missing.first()) {
        _ if status.flags.contains(CHANGED) => CHANGED.into(),
        _ if generation.is_none() => "source never observed".into(),
        (Err(error), _) => error.clone(),
        (Ok(None), _) => "source_changed_now".into(),
        (Ok(Some(_)), None) => "complete_to_eof".into(),
        (Ok(Some(_)), Some((from, _))) if end - from > COPY_CAP => format!("over_cap from {from}"),
        (Ok(Some(_)), Some((from, _))) => format!("missing readable from {from}"),
    };
}

/// `Ok(Some(size))` when the source still extends the generation append-only.
fn observe(status: &mut SourceStatus, generation: &Observation) -> Result<Option<u64>, String> {
    let kind = |error: io::Error| format!("unreadable: {}", error.kind());
    let mut file = fs::File::open(&status.source).map_err(kind)?;
    let meta = file.metadata().map_err(kind)?;
    let id = identity(&meta).ok_or("identity_unverified")?;
    let mut head = Vec::new();
    let read = (&mut file).take(generation.head_len).read_to_end(&mut head);
    read.map_err(kind)?;
    status.verify_read += head.len() as u64;
    let kept = id == (generation.dev, generation.ino)
        && meta.len() >= status.max_eof
        && format!("{:x}", Sha256::digest(&head)) == generation.head_sha256;
    let last = status.copies.iter().max_by_key(|(_, to, _)| *to);
    let joined = last.is_none_or(|(from, to, copy)| {
        let len = JOIN_WINDOW.min(to - from);
        let source = window(&mut file, to - len, len);
        let held =
            fs::File::open(copy).and_then(|mut copy| window(&mut copy, to - from - len, len));
        status.verify_read += 2 * len;
        matches!((source, held), (Ok(a), Ok(b)) if a == b)
    });
    Ok((kept && joined).then_some(meta.len()))
}

fn window(file: &mut fs::File, at: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(at))?;
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

/// No stable file identity here, so a replaced source cannot be told apart.
#[cfg(not(unix))]
fn identity(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn union(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (from, to) in ranges {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
}

fn subtract((mut from, to): (u64, u64), held: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut missing = Vec::new();
    for &(start, end) in held.iter().filter(|(start, _)| *start < to) {
        if start > from {
            missing.push((from, start));
        }
        from = from.max(end);
    }
    if from < to {
        missing.push((from, to));
    }
    missing
}

/// The report `adk custody status` prints for the custody root, optionally narrowed.
fn status_report(root: &Path, provider: Option<&str>, id: Option<&str>) -> Result<String, String> {
    let mut out = String::new();
    let providers = match provider {
        Some(provider) => vec![root.join(provider)],
        None => subdirs(root, &mut out)?,
    };
    for dir in providers {
        for path in subdirs(&dir, &mut out)? {
            if id.is_none_or(|id| name(&path) == id) {
                render(&mut out, &name(&dir), &episode_status(&path));
            }
        }
    }
    Ok(out)
}

/// The directories under `dir`, sorted; any other entry is skipped with a warning line.
fn subdirs(dir: &Path, out: &mut String) -> Result<Vec<PathBuf>, String> {
    let error = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    let mut dirs = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| error(dir, e))? {
        let path = entry.map_err(|e| error(dir, e))?.path();
        match fs::metadata(&path).map_err(|e| error(&path, e))?.is_dir() {
            true => dirs.push(path),
            false => *out += &format!("skipped non-directory {}\n", path.display()),
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn render(out: &mut String, provider: &str, ep: &EpisodeStatus) {
    use std::fmt::Write;
    let verdict = ["incomplete", "complete_to_eof"][usize::from(ep.complete())];
    let (id, flags) = (&ep.id, &ep.flags);
    let channel = ep.channel_id.map_or("?".into(), |id| id.to_string());
    let _ = writeln!(out, "{provider}/{id} channel={channel} {verdict} {flags:?}");
    for s in &ep.sources {
        let (from, preserved, missing) = (s.required_from, &s.preserved, &s.missing);
        let _ = writeln!(out, "  {} required_from={from:?}", s.source);
        let _ = writeln!(out, "    preserved={preserved:?} missing={missing:?}");
        // The file at the path now, which an unresolved obligation no longer follows.
        let now = match fs::metadata(&s.source).map(|meta| (identity(&meta), meta.len())) {
            Ok((Some((dev, ino)), size)) => format!("(dev, ino)=({dev}, {ino}) size={size}"),
            Ok((None, size)) => format!("identity_unverified size={size}"),
            Err(error) => error.kind().to_string(),
        };
        let last = s.last_attempt.as_deref().unwrap_or("none");
        let _ = writeln!(out, "    flags={:?} current: {}", s.flags, s.current);
        let _ = writeln!(out, "    now: {now}\n    last_attempt: {last}");
        let _ = writeln!(out, "    internal: unverified (append-only assumed)");
    }
}

/// `adk custody status`: reads the release runtime's custody directory and prints the report.
pub(crate) fn cmd_status(provider: Option<&str>, episode: Option<&str>) -> Result<(), String> {
    let root = crate::config::runtime_root().ok_or("no AgentDesk root directory")?;
    let report = status_report(&root.join("runtime/discord_custody"), provider, episode)?;
    print!("{report}");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
