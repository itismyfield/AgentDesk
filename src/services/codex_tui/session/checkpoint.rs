//! Physical record boundaries are read checkpoints, never delivery receipts.
use crate::services::cluster::stream_relay::SourceFileIdentity;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub path: PathBuf,
    pub session_id: Option<String>,
    identity: SourceFileIdentity,
    generation: Option<(SourceFileIdentity, SystemTime)>,
    nonce: Option<String>,
    raw_end: u64,
    pub complete_end: u64,
}

impl Source {
    pub(crate) fn matches_path(&self, path: &Path) -> bool {
        crate::services::codex_tui::session::codex_tui_rollout_paths_same(&self.path, path)
    }
    pub(crate) fn open(
        tmux: &str,
        path: &Path,
        session_id: Option<String>,
    ) -> io::Result<(Self, File)> {
        let mut file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("Codex source is not a regular file"));
        }
        let len = file.metadata()?.len();
        let source = Self {
            path: path.to_path_buf(),
            session_id,
            identity: SourceFileIdentity::from_open_file(&file),
            generation: generation(tmux),
            nonce: nonce(tmux),
            raw_end: len,
            complete_end: boundary(&mut file, len)?,
        };
        source.check(tmux)?;
        Ok((source, file))
    }

    pub(crate) fn record(&self, tmux: &str, end: u64) -> io::Result<serde_json::Value> {
        let mut file = self.reader(tmux)?;
        if end == 0 || boundary(&mut file, end)? != end {
            return Err(io::Error::other("Codex record is not sealed"));
        }
        let start = boundary(&mut file, end - 1)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    pub(crate) fn check(&self, tmux: &str) -> io::Result<()> {
        self.reader(tmux).map(|_| ())
    }

    pub(crate) fn reader(&self, tmux: &str) -> io::Result<File> {
        let file = File::open(&self.path)?;
        if self.identity == SourceFileIdentity::Unavailable
            || SourceFileIdentity::from_open_file(&file) != self.identity
            || file.metadata()?.len() < self.raw_end
            || generation(tmux) != self.generation
            || nonce(tmux) != self.nonce
        {
            return Err(io::Error::other("Codex checkpoint source changed"));
        }
        Ok(file)
    }
}

fn generation(tmux: &str) -> Option<(SourceFileIdentity, SystemTime)> {
    let path = crate::services::tmux_common::resolve_session_temp_path(tmux, "generation")?;
    let file = File::open(path).ok()?;
    Some((
        SourceFileIdentity::from_open_file(&file),
        file.metadata().ok()?.modified().ok()?,
    ))
}

fn nonce(tmux: &str) -> Option<String> {
    let path = crate::services::tmux_common::resolve_session_temp_path(tmux, "spawn_nonce")?;
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

pub(crate) fn boundary(file: &mut File, offset: u64) -> io::Result<u64> {
    if offset > file.metadata()?.len() {
        return Err(io::Error::other("Codex checkpoint beyond source"));
    }
    let mut end = offset;
    let mut chunk = [0_u8; 8192];
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let len = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..len])?;
        if let Some(index) = chunk[..len].iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + index as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

pub(crate) fn at(path: &Path, offset: u64) -> io::Result<u64> {
    boundary(&mut File::open(path)?, offset)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Advance {
    Advanced {
        source: SourceFileIdentity,
        start: u64,
        end: u64,
    },
    AlreadyAdvanced,
    SourceChanged,
    StartMismatch,
    PersistFailed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanStats {
    pub assistant_records: u64,
    pub records: u64,
    pub source_bytes: u64,
    pub assistant_bytes: u64,
    pub malformed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationKind {
    Unknown,
    Delegated,
}

impl ScanStats {
    pub(crate) fn add(&mut self, stats: Self) {
        self.assistant_records += stats.assistant_records;
        self.records += stats.records;
        self.source_bytes += stats.source_bytes;
        self.assistant_bytes += stats.assistant_bytes;
        self.malformed += stats.malformed;
    }

    pub(crate) fn report(self, source: &Source, start: u64, end: u64, kind: ObservationKind) {
        tracing::info!(
            status = match kind { ObservationKind::Unknown => "unknown", ObservationKind::Delegated => "delegated" },
            source = ?source.identity, start, end,
            assistant_records = self.assistant_records,
            records = self.records,
            source_bytes = self.source_bytes,
            unknown_assistant_bytes = self.assistant_bytes,
            malformed = self.malformed,
            "Codex observed a physical span"
        );
    }
    pub(crate) fn observe(&mut self, bytes: usize, json: Option<&serde_json::Value>) {
        self.records += 1;
        self.source_bytes += bytes as u64;
        let Some(json) = json else {
            self.malformed += 1;
            return;
        };
        let payload = &json["payload"];
        if json["type"] == "response_item"
            && payload["type"] == "message"
            && payload["role"] == "assistant"
        {
            self.assistant_records += 1;
            if let Some(content) = payload["content"].as_array() {
                self.assistant_bytes += content
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .map(|text| text.len() as u64)
                    .sum::<u64>();
            }
        } else if json["type"] == "event_msg" && payload["type"] == "agent_message" {
            self.assistant_records += 1;
            self.assistant_bytes += payload["message"]
                .as_str()
                .map_or(0, |text| text.len() as u64);
        }
    }
}

pub(crate) type ParkedOwnerKey = (u64, String, String, String, u64, u64);

#[derive(Clone, Debug)]
pub(crate) struct RestorePlan<C> {
    pub source: Source,
    pub input_start: u64,
    pub held: Option<C>,
    pub replay_from: Option<u64>,
    pub observed_end: u64,
    pub unknown: ScanStats,
    pub delegated: ScanStats,
    pub owners: HashMap<String, ParkedOwnerKey>,
    pub settled_inputs: HashSet<usize>,
    pub joined_inputs: HashMap<usize, (String, u64)>,
}

impl<C> RestorePlan<C> {
    pub(crate) fn new(source: Source, input_start: u64, held: Option<C>, stats: ScanStats) -> Self {
        Self {
            observed_end: source.complete_end,
            source,
            input_start,
            replay_from: held.as_ref().map(|_| input_start),
            held,
            unknown: stats,
            delegated: ScanStats::default(),
            owners: HashMap::new(),
            settled_inputs: HashSet::new(),
            joined_inputs: HashMap::new(),
        }
    }

    pub(crate) fn record_unknown(
        &mut self,
        tmux: &str,
        start: u64,
        end: u64,
        stats: ScanStats,
        kind: ObservationKind,
    ) {
        if self.source.check(tmux).is_ok()
            && end > self.observed_end
            && stats.source_bytes == end.saturating_sub(start.max(self.observed_end))
        {
            self.observed_end = end;
            match kind {
                ObservationKind::Unknown => self.unknown.add(stats),
                ObservationKind::Delegated => self.delegated.add(stats),
            }
            stats.report(&self.source, start, end, kind);
        }
    }
}

pub(crate) fn first_user_record_created_since(
    rollout_path: &Path,
    since: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let (mut offset, mut line) = (0_u64, String::new());
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 || !line.ends_with('\n') {
            return None;
        }
        let start = offset;
        offset += read as u64;
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(
            &json,
        )
        .is_some()
            && crate::services::tui_prompt_dedupe::RecordEvidence::from_record(&json)
                .created_since(since)
        {
            return Some(start);
        }
    }
}
