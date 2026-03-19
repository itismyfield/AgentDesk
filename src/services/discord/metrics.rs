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
}

fn metrics_dir() -> Option<PathBuf> {
    runtime_store::remotecc_root().map(|root| root.join("metrics"))
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
    let Ok(json) = serde_json::to_string(metric) else { return };
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let _ = writeln!(file, "{json}");
}

/// Load today's metrics.
pub(super) fn load_today() -> Vec<TurnMetric> {
    load_file(&today_file().unwrap_or_default())
}

/// Load metrics for a specific date (YYYY-MM-DD).
/// Validates date format to prevent path traversal.
pub(super) fn load_date(date: &str) -> Vec<TurnMetric> {
    // Validate YYYY-MM-DD format to prevent path traversal (e.g. "../")
    if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
        return vec![];
    }
    let Some(dir) = metrics_dir() else { return vec![] };
    load_file(&dir.join(format!("{date}.jsonl")))
}

fn load_file(path: &std::path::Path) -> Vec<TurnMetric> {
    let Ok(content) = fs::read_to_string(path) else { return vec![] };
    content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Build a summary report for Discord.
pub(super) fn build_metrics_report(metrics: &[TurnMetric], label: &str) -> String {
    if metrics.is_empty() {
        return format!("**📊 Metrics ({label})**\n  (no data)");
    }

    let total_turns = metrics.len();
    let total_duration: f64 = metrics.iter().map(|m| m.duration_secs).sum();
    let avg_duration = total_duration / total_turns as f64;
    let total_input: u64 = metrics.iter().filter_map(|m| m.input_tokens).sum();
    let total_output: u64 = metrics.iter().filter_map(|m| m.output_tokens).sum();

    // Per-channel breakdown
    let mut by_channel: std::collections::HashMap<u64, Vec<&TurnMetric>> = std::collections::HashMap::new();
    for m in metrics {
        by_channel.entry(m.channel_id).or_default().push(m);
    }

    let mut lines = vec![
        format!("**📊 Metrics ({label})**"),
        format!("  Turns: {} | Avg: {:.0}s | Total: {:.0}s", total_turns, avg_duration, total_duration),
        format!("  Tokens: {}↓ {}↑", total_input, total_output),
    ];

    let mut channels: Vec<_> = by_channel.iter().collect();
    channels.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (ch_id, turns) in channels.iter().take(10) {
        let ch_turns = turns.len();
        let ch_avg: f64 = turns.iter().map(|m| m.duration_secs).sum::<f64>() / ch_turns as f64;
        let ch_input: u64 = turns.iter().filter_map(|m| m.input_tokens).sum();
        let ch_output: u64 = turns.iter().filter_map(|m| m.output_tokens).sum();
        lines.push(format!(
            "  **#{}** — {} turns, avg {:.0}s, {}↓ {}↑",
            ch_id, ch_turns, ch_avg, ch_input, ch_output
        ));
    }

    lines.join("\n")
}
