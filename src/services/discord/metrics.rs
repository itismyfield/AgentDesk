use std::fs;
use std::io::Write;
use std::path::PathBuf;

use super::runtime_store;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(super) struct TurnMetric {
    pub channel_id: u64,
    pub provider: String,
    pub timestamp: String, // ISO 8601
    pub duration_secs: f64,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub memory_input_tokens: Option<u64>,
    pub memory_output_tokens: Option<u64>,
}

fn metrics_dir() -> Option<PathBuf> {
    runtime_store::agentdesk_root().map(|root| root.join("metrics"))
}

fn today_file() -> Option<PathBuf> {
    let dir = metrics_dir()?;
    let _ = fs::create_dir_all(&dir);
    let date = chrono::Local::now().format("%Y-%m-%d");
    Some(dir.join(format!("{date}.jsonl")))
}

/// Append a turn metric entry to today's JSONL file.
pub(super) fn record_turn(metric: &TurnMetric) {
    let Some(path) = today_file() else { return };
    let Ok(json) = serde_json::to_string(metric) else {
        return;
    };
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(file, "{json}");
}
