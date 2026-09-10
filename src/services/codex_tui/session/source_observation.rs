//! Metadata comparison only: native IDs are not ADK turn/lease or launch proof.
use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use std::io::{BufRead, BufReader, Read};

// Install holds source authority: bounded bytes/lines, no waiting or retries.
const HEADER_BYTES: u64 = 64 * 1024;
const HEADER_LINES: usize = 16;

pub(super) fn observe(path: &Path, supplied: Option<&str>) -> &'static str {
    let root = default_codex_sessions_dir().and_then(|p| p.canonicalize().ok());
    let canonical = path.canonicalize().ok();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok();
    let identity = file.as_ref().map(SourceFileIdentity::from_open_file);
    let verdict = match (root.as_ref(), canonical.as_ref(), file) {
        (Some(root), Some(path), Some(file)) if path.starts_with(root) => compare(file, supplied),
        _ => "pending",
    };
    // This install log is the consumer, not durable source selection evidence.
    tracing::info!(
        scope = "source_metadata_only",
        launch_verified = false,
        verdict,
        ?root,
        ?canonical,
        ?identity,
        "Codex rollout metadata observation"
    );
    verdict
}

pub(super) fn compare(file: std::fs::File, supplied: Option<&str>) -> &'static str {
    let Some(supplied) = supplied.and_then(|id| uuid::Uuid::parse_str(id.trim()).ok()) else {
        return "pending";
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return "pending";
    }
    let mut reader = BufReader::new(file.take(HEADER_BYTES));
    let mut line = String::new();
    for _ in 0..HEADER_LINES {
        line.clear();
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            return "pending";
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return "pending";
        };
        if value["type"] != "session_meta" {
            continue;
        }
        let meta = &value["payload"];
        let Some(id) = meta["id"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            return "pending";
        };
        if id != supplied {
            return "rejected";
        }
        let source = &meta["source"];
        if source.get("subagent").is_some() || meta["parent_thread_id"].as_str().is_some() {
            return "child";
        }
        return match source.as_str() {
            Some("cli" | "vscode" | "exec") => "matched",
            _ => "pending",
        };
    }
    "pending"
}
