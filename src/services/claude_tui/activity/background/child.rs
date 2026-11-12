//! Each exact child rollout is replayed once, then read from its saved cursor.
use super::super::source::Source;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Cursor {
    identity: (u64, u64),
    failed: bool,
    modified: SystemTime,
    len: u64,
    pub through: u64,
    partial: Vec<u8>,
    terminal: Option<bool>,
}
pub(super) fn path(source: &Source, id: &str) -> Option<PathBuf> {
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)))
    .then(|| {
        source
            .path
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{id}.jsonl"))
    })
}
pub(super) fn read(
    source: &Source,
    id: &str,
    at: DateTime<Utc>,
    previous: &mut Option<Cursor>,
) -> Result<Option<bool>, ()> {
    let path = path(source, id).ok_or(())?;
    if fs::canonicalize(&path).map_err(|_| ())? != path {
        return Err(());
    }
    let mut file = File::open(&path).map_err(|_| ())?;
    let meta = file.metadata().map_err(|_| ())?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    let modified = meta.modified().map_err(|_| ())?;
    let mut cursor = previous.clone().unwrap_or(Cursor {
        identity,
        failed: false,
        modified,
        len: meta.len(),
        through: 0,
        partial: Vec::new(),
        terminal: None,
    });
    if cursor.failed
        || identity != cursor.identity
        || meta.len() < cursor.len
        || (meta.len() == cursor.len && modified != cursor.modified)
    {
        if let Some(old) = previous {
            old.failed = true;
        }
        return Err(());
    }
    cursor.len = meta.len();
    cursor.modified = modified;
    file.seek(SeekFrom::Start(cursor.through)).map_err(|_| ())?;
    let mut reader = BufReader::new(file.take(256 * 1024));
    let mut line = std::mem::take(&mut cursor.partial);
    loop {
        let first = cursor.through == line.len() as u64;
        let n = reader.read_until(b'\n', &mut line).map_err(|_| ())?;
        cursor.through += n as u64;
        if line.len() > crate::services::tui_o::shadow::capture::MAX_PARTIAL_BYTES {
            return Err(());
        }
        if n == 0 || !line.ends_with(b"\n") {
            cursor.partial = line;
            break;
        }
        let row: Value = serde_json::from_slice(&line).map_err(|_| ())?;
        line.clear();
        let native = row["sessionId"]
            .as_str()
            .or_else(|| row["session_id"].as_str());
        if (first && native != Some(&source.native))
            || native.is_some_and(|s| s != source.native)
            || row["agentId"]
                .as_str()
                .or_else(|| row["agent_id"].as_str())
                .is_some_and(|s| s != id)
        {
            return Err(());
        }
        let time = row["timestamp"]
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok());
        if time.is_none_or(|time| time < at) {
            continue;
        }
        let terminal = row["type"] == "result"
            || (row["type"] == "system" && row["subtype"] == "turn_duration")
            || (row["type"] == "assistant" && row["message"]["stop_reason"] == "end_turn");
        if terminal {
            cursor.terminal = Some(row["is_error"] != true);
        } else if matches!(row["type"].as_str(), Some("user" | "assistant")) {
            cursor.terminal = None;
        }
    }
    let final_meta = fs::metadata(&path).map_err(|_| ())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (final_meta.dev(), final_meta.ino()) != cursor.identity {
            return Err(());
        }
    }
    if final_meta.len() != cursor.len || final_meta.modified().ok() != Some(cursor.modified) {
        return Err(());
    }
    let result = if cursor.through == cursor.len && cursor.partial.is_empty() {
        cursor.terminal
    } else {
        None
    };
    *previous = Some(cursor);
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn large_child_replay_advances_saved_offset_and_unchanged_reads_do_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let source = Source {
            session: "fixture".into(),
            channel: 0,
            owner: "fixture".into(),
            host: None,
            nonce: "fixture".into(),
            native: uuid::Uuid::new_v4().to_string(),
            path: dir.path().join("native.jsonl"),
            file: (0, 0),
            visit: Utc::now(),
        };
        let path = path(&source, "child-a").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        let now = Utc::now();
        for _ in 0..1500 {
            writeln!(file,"{}",serde_json::json!({"type":"mode","sessionId":source.native,"agentId":"child-a","timestamp":now,"padding":"x".repeat(1024)})).unwrap();
        }
        writeln!(file,"{}",serde_json::json!({"type":"result","sessionId":source.native,"agentId":"child-a","timestamp":now})).unwrap();
        drop(file);
        let mut cursor = None;
        assert_eq!(
            read(&source, "child-a", source.visit, &mut cursor),
            Ok(None)
        );
        let first = cursor.as_ref().unwrap().through;
        assert!(first > 0 && first <= 256 * 1024 && first < fs::metadata(&path).unwrap().len());
        let mut result = None;
        for _ in 0..16 {
            let before = cursor.as_ref().unwrap().through;
            result = read(&source, "child-a", source.visit, &mut cursor).unwrap();
            assert!(cursor.as_ref().unwrap().through >= before);
            if result.is_some() {
                break;
            }
        }
        assert_eq!(result, Some(true));
        let through = cursor.as_ref().unwrap().through;
        assert_eq!(
            read(&source, "child-a", source.visit, &mut cursor),
            Ok(Some(true))
        );
        assert_eq!(cursor.as_ref().unwrap().through, through);
        assert!(super::path(&source, "../other").is_none());
        let wrong = serde_json::json!({"type":"result","sessionId":source.native,"agentId":"child-b","timestamp":now});
        fs::write(path, format!("{wrong}\n")).unwrap();
        assert!(read(&source, "child-a", source.visit, &mut None).is_err());
    }
}
