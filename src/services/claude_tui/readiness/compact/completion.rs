//! Only a confirmed local compact reservation can consume its own command output.
use super::super::{Reservation, evidence::Stamp};
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

pub(in super::super) struct Done {
    pub floor: Stamp,
    pub recovery: Option<(u64, chrono::DateTime<chrono::Utc>)>,
}

/// A closed local output ends the command; success also requires a manual boundary.
pub(in super::super) fn read(
    path: &Path,
    floor: &Stamp,
    native: &str,
    idle_only: bool,
) -> Option<bool> {
    let stamp = Stamp::read(path)?;
    if stamp.file != floor.file || stamp.len <= floor.len {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(floor.len)).ok()?;
    let mut reader = BufReader::new(file);
    let mut row = Vec::new();
    let (mut boundary, mut command, mut output) = (false, false, None);
    while reader.read_until(b'\n', &mut row).ok()? > 0 {
        if !row.ends_with(b"\n") {
            return None;
        }
        let value: Value = serde_json::from_slice(&row).ok()?;
        if value["sessionId"]
            .as_str()
            .or_else(|| value["session_id"].as_str())
            .is_some_and(|seen| seen != native)
        {
            return None;
        }
        match value["type"].as_str()? {
            "system"
                if value["subtype"] == "compact_boundary"
                    && value["compactMetadata"]["trigger"] == "manual"
                    && !command
                    && !boundary =>
            {
                boundary = true;
            }
            "user" => {
                let text = value["message"]["content"].as_str()?.trim();
                if output.is_some() {
                    return None;
                }
                if !command
                    && ((value["isCompactSummary"] == true && boundary)
                        || text.starts_with("<local-command-caveat>"))
                {
                    // Summary and caveat content are opaque, never readiness authority.
                } else if text.starts_with("<command-name>/compact</command-name>") && !command {
                    command = true;
                } else if command {
                    let (body, stderr) = if let Some(body) = text
                        .strip_prefix("<local-command-stdout>")
                        .and_then(|text| text.strip_suffix("</local-command-stdout>"))
                    {
                        (body, false)
                    } else if let Some(body) = text
                        .strip_prefix("<local-command-stderr>")
                        .and_then(|text| text.strip_suffix("</local-command-stderr>"))
                    {
                        (body, true)
                    } else {
                        return None;
                    };
                    let plain =
                        crate::services::codex_tui::input::strip_ansi_escape_sequences(body);
                    output = Some(boundary && !stderr && plain.trim().starts_with("Compacted ("));
                } else {
                    return None;
                }
            }
            "attachment"
            | "file-history-snapshot"
            | "last-prompt"
            | "ai-title"
            | "mode"
            | "permission-mode"
            | "session-owner"
            | "atis-latch"
            | "cost-state"
            | "queue-operation" => {}
            _ => return None,
        }
        // Retire the completed control even if a new native turn beat this timer read.
        if output.is_some() && !idle_only {
            break;
        }
        row.clear();
    }
    if Stamp::read(path).as_ref() != Some(&stamp) {
        return None;
    }
    output
}

impl Reservation {
    pub(super) fn compact_submitted(self) -> Self {
        let mut state = super::super::ledger();
        if let Some(entry) = state.entries.get_mut(&self.source)
            && entry
                .pending
                .is_some_and(|(_, token)| token == self.revision)
        {
            entry.submitted = true;
            entry.compact_ticket = Some(self.revision);
        }
        self
    }

    /// Generation is rechecked by the worker caller; only its exact pending token is retired.
    pub(super) fn finish_compact(
        &self,
        floor: &Stamp,
        start: Option<chrono::DateTime<chrono::Utc>>,
    ) -> bool {
        // Source/file checks precede the caller's generation lock; publication is ledger-only.
        let mut state = super::super::ledger();
        let current_start = self.source.nonce.as_ref().and_then(|nonce| {
            state
                .starts
                .get(&(self.source.session.clone(), nonce.clone()))
                .map(|(at, _)| *at)
        });
        if current_start != start {
            return false;
        }
        let Some(entry) = state.entries.get_mut(&self.source) else {
            return false;
        };
        if entry
            .recovery
            .is_some_and(|recovery| Some(recovery) != self.recovery)
            || entry.compact_ticket != Some(self.revision)
            || entry
                .pending
                .is_none_or(|(_, token)| token != self.revision)
        {
            return false;
        }
        entry.pending = None;
        entry.compact_ticket = None;
        entry.compact_done = Some(Done {
            floor: floor.clone(),
            recovery: entry.recovery,
        });
        entry.revision += 1;
        super::super::changed(&mut state);
        true
    }
}
