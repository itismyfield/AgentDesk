//! Read-only fold of the boot-custody ledger into one preservation obligation per transcript
//! source, printed by `adk custody status`. Transcripts are assumed to be append-only.

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(crate) const CUSTODY_DIR: &str = "discord_custody";
/// The boot copier's per-copy cap; a copy plan longer than this is not attempted.
const COPY_CAP: u64 = 64 << 20;
/// Bytes compared at the preserved end, once on the source and once in the copy.
const JOIN_WINDOW: u64 = 4 << 10;
const CHANGED: &str = "unresolved_source_changed";

/// A source's identity as one attempt saw it; `g_prefix_sha` hashes the first bytes of the
/// obligation's generation when the writer knew that generation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Observation {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) head_len: u64,
    pub(crate) head_sha256: String,
    #[serde(default)]
    pub(crate) g_prefix_sha: Option<String>,
}

/// `intent.json`, written before an attempt copies anything.
#[derive(Deserialize)]
struct Intent {
    sources: Vec<IntentSource>,
}

#[derive(Deserialize)]
struct IntentSource {
    source: String,
    required_from: u64,
    pre: Option<Observation>,
}

/// `outcome.json`, written after an attempt, also when no manifest is published.
#[derive(Deserialize)]
struct Outcome {
    sources: Vec<OutcomeSource>,
}

#[derive(Deserialize)]
struct OutcomeSource {
    source: String,
    result: String,
    post: Option<Observation>,
    copy: Option<String>,
    from: u64,
    to: u64,
}

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
}

impl EpisodeStatus {
    /// Everything required up to the current EOF is held and nothing is unresolved.
    pub(crate) fn complete(&self) -> bool {
        let held = |s: &SourceStatus| s.flags.is_empty() && s.current == "complete_to_eof";
        self.flags.is_empty() && self.sources.iter().all(held)
    }
}

/// Folds every episode of one provider's custody directory; an unlistable directory is an error.
pub(crate) fn provider_status(dir: &Path) -> Result<Vec<EpisodeStatus>, String> {
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    Ok(dirs
        .iter()
        .filter(|dir| dir.is_dir())
        .map(|dir| episode_status(dir))
        .collect())
}

fn episode_status(dir: &Path) -> EpisodeStatus {
    let id = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut episode = EpisodeStatus {
        id: id.unwrap_or_default(),
        ..Default::default()
    };
    let marker = fs::read(dir.join("episode.json")).ok();
    match marker.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
        Some(marker) => episode.channel_id = marker["episode"]["channel_id"].as_u64(),
        None => _ = episode.flags.insert("inventory_unresolved"),
    }
    let revisions = match revisions(dir) {
        Ok(revisions) => revisions,
        Err(_) => return flagged(episode, "unreadable_episode"),
    };
    if revisions.is_empty() {
        episode.flags.insert("inventory_unresolved");
    }
    let mut failed_bytes = BTreeSet::new();
    for rev in revisions {
        fold_revision(&mut episode, &mut failed_bytes, &dir.join(&rev), &rev);
    }
    if !failed_bytes.is_empty() {
        episode.flags.insert("bytes_copy_failed");
    }
    for status in &mut episode.sources {
        judge_now(status);
    }
    episode
}

fn flagged(mut episode: EpisodeStatus, flag: &'static str) -> EpisodeStatus {
    episode.flags.insert(flag);
    episode
}

fn revisions(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut revisions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let index = name
            .strip_prefix("rev-")
            .and_then(|index| index.parse::<u32>().ok());
        revisions.extend(index.map(|index| (index, name)));
    }
    revisions.sort();
    Ok(revisions.into_iter().map(|(_, name)| name).collect())
}

/// Reads one optional ledger file: `None` when absent, `Some(Err)` when present but unusable.
fn ledger_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<Result<T, ()>> {
    match fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        bytes => Some(
            bytes
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .ok_or(()),
        ),
    }
}

/// Applies one revision in ledger order: intent, then the published manifest, then the outcome.
fn fold_revision(ep: &mut EpisodeStatus, failed: &mut BTreeSet<String>, dir: &Path, rev: &str) {
    let intent = ledger_file::<Intent>(&dir.join("intent.json"));
    let manifest = ledger_file::<Value>(&dir.join("manifest.json"));
    let outcome = ledger_file::<Outcome>(&dir.join("outcome.json"));
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
        ep.flags.insert("history_unresolved");
    }
    for entry in entries.flatten().unwrap_or_default() {
        let (Some(kind), Some(source)) = (entry["kind"].as_str(), entry["source"].as_str()) else {
            ep.flags.insert("history_unresolved");
            continue;
        };
        if kind != "transcript" {
            match entry["copy"].is_string() {
                true => failed.remove(source),
                false => failed.insert(source.to_string()),
            };
            continue;
        }
        let status = source_mut(ep, source);
        let observed = serde_json::from_value::<Observation>(entry.clone()).ok();
        status.see(
            entry["offset"].as_u64(),
            observed.as_ref(),
            entry["source_changed"] == true,
        );
        let copy = entry["copy"]
            .as_str()
            .map(|copy| (&entry["from"], &entry["to"], copy));
        let copy = copy.and_then(|(from, to, copy)| Some((from.as_u64()?, to.as_u64()?, copy)));
        status.done(
            rev,
            entry["error"].as_str().unwrap_or("ok"),
            None,
            copy,
            dir,
        );
    }
    match outcome {
        Some(Ok(outcome)) => outcome.sources.iter().for_each(|source| {
            let copy = source.copy.as_deref().filter(|_| source.result == "ok");
            let copy = copy.map(|copy| (source.from, source.to, copy));
            source_mut(ep, &source.source).done(
                rev,
                &source.result,
                source.post.as_ref(),
                copy,
                dir,
            )
        }),
        Some(Err(())) => _ = ep.flags.insert("history_unresolved"),
        None => {}
    }
}

fn source_mut<'a>(episode: &'a mut EpisodeStatus, source: &str) -> &'a mut SourceStatus {
    let index = episode
        .sources
        .iter()
        .position(|status| status.source == source);
    let index = index.unwrap_or_else(|| {
        let status = SourceStatus {
            source: source.to_string(),
            ..Default::default()
        };
        episode.sources.push(status);
        episode.sources.len() - 1
    });
    &mut episode.sources[index]
}

impl SourceStatus {
    /// Adds a requirement and an observation; any evidence against append-only growth of the
    /// first observed generation is sticky.
    fn see(
        &mut self,
        required_from: Option<u64>,
        seen: Option<&Observation>,
        legacy_changed: bool,
    ) {
        if let Some(from) = required_from {
            self.required_from = Some(self.required_from.map_or(from, |min| min.min(from)));
        }
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
        if legacy_changed || moved || seen.size < self.max_eof || !prefix_kept {
            self.flags.insert(CHANGED);
        }
        self.max_eof = self.max_eof.max(seen.size);
    }

    /// Records an attempt's result; its copy counts only when it has the claimed length and no
    /// change was seen up to and including this attempt.
    fn done(
        &mut self,
        rev: &str,
        result: &str,
        post: Option<&Observation>,
        copy: Option<(u64, u64, &str)>,
        dir: &Path,
    ) {
        if post.is_some() {
            self.see(None, post, false);
        }
        self.last_attempt = Some(format!("{rev}: {result}"));
        let Some((from, to, name)) = copy.filter(|_| !self.flags.contains(CHANGED)) else {
            return;
        };
        let path = dir.join(name);
        let held = fs::metadata(&path).is_ok_and(|meta| Some(meta.len()) == to.checked_sub(from));
        if held {
            self.max_eof = self.max_eof.max(to);
            self.copies.push((from, to, path));
        }
    }
}

/// Opens the source once and judges it against the first observed generation: head prefix,
/// size and the last preserved `JOIN_WINDOW`; interior bytes are not re-read.
fn judge_now(status: &mut SourceStatus) {
    status.preserved = union(
        status
            .copies
            .iter()
            .map(|(from, to, _)| (*from, *to))
            .collect(),
    );
    let generation = status.generation.clone();
    let seen = match generation
        .as_ref()
        .filter(|_| !status.flags.contains(CHANGED))
    {
        Some(generation) => observe(status, generation),
        None => Ok(None),
    };
    let eof = seen.as_ref().ok().copied().flatten();
    let end = status.max_eof.max(eof.unwrap_or(0));
    let required_from = status.required_from.unwrap_or(end);
    if generation.is_some() && required_from > end {
        status.flags.insert("required_past_eof");
    }
    status.missing = subtract((required_from, end), &status.preserved);
    status.current = match (&seen, status.missing.first()) {
        _ if status.flags.contains(CHANGED) => CHANGED.into(),
        _ if generation.is_none() => "source never observed".into(),
        (Err(error), _) => format!("unreadable: {error}"),
        (Ok(None), _) => "source_changed_now".into(),
        (Ok(Some(_)), None) => "complete_to_eof".into(),
        (Ok(Some(_)), Some((from, _))) if end - from > COPY_CAP => format!("over_cap from {from}"),
        (Ok(Some(_)), Some((from, _))) => format!("missing readable from {from}"),
    };
}

/// `Ok(Some(size))` when the source still extends the generation append-only.
fn observe(status: &mut SourceStatus, generation: &Observation) -> Result<Option<u64>, String> {
    let mut file = fs::File::open(&status.source).map_err(|error| error.kind().to_string())?;
    let meta = file.metadata().map_err(|error| error.kind().to_string())?;
    let mut head = Vec::new();
    let head_read = (&mut file).take(generation.head_len).read_to_end(&mut head);
    head_read.map_err(|error| error.kind().to_string())?;
    status.verify_read += head.len() as u64;
    let kept = identity(&meta) == (generation.dev, generation.ino)
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

fn window(file: &mut fs::File, at: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(at))?;
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn identity(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn identity(_meta: &fs::Metadata) -> (u64, u64) {
    (0, 0)
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
pub(crate) fn status_report(
    root: &Path,
    provider: Option<&str>,
    episode: Option<&str>,
) -> Result<String, String> {
    let mut providers: Vec<PathBuf> = match provider {
        Some(provider) => vec![root.join(provider)],
        None => fs::read_dir(root)
            .map_err(|e| format!("{}: {e}", root.display()))?
            .flatten()
            .map(|entry| entry.path())
            .collect(),
    };
    providers.sort();
    let mut out = String::new();
    for dir in providers {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for status in provider_status(&dir)?
            .iter()
            .filter(|s| episode.is_none_or(|e| s.id == e))
        {
            render(&mut out, &name, status);
        }
    }
    Ok(out)
}

fn render(out: &mut String, provider: &str, ep: &EpisodeStatus) {
    use std::fmt::Write;
    let verdict = if ep.complete() {
        "complete_to_eof"
    } else {
        "incomplete"
    };
    let channel = ep.channel_id.map_or("?".into(), |id| id.to_string());
    let _ = writeln!(
        out,
        "{provider}/{} channel={channel} {verdict} flags={:?}",
        ep.id, ep.flags
    );
    for s in &ep.sources {
        let _ = writeln!(
            out,
            "  {} required_from={:?} preserved={:?} missing={:?} flags={:?}",
            s.source, s.required_from, s.preserved, s.missing, s.flags
        );
        let last = s.last_attempt.as_deref().unwrap_or("none");
        let _ = writeln!(out, "    current: {}\n    last_attempt: {last}", s.current);
        let _ = writeln!(out, "    internal: unverified (append-only assumed)");
    }
}

/// `adk custody status`: reads the release runtime's custody directory and prints the report.
pub(crate) fn cmd_status(provider: Option<&str>, episode: Option<&str>) -> Result<(), String> {
    let root = crate::config::runtime_root().ok_or("no AgentDesk root directory")?;
    let custody = root.join("runtime").join(CUSTODY_DIR);
    print!("{}", status_report(&custody, provider, episode)?);
    Ok(())
}

#[cfg(test)]
mod tests;
