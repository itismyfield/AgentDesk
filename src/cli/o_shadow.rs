//! `agentdesk o-shadow`: open a TUI output shadow window, judge it, and register synthetic prompts.

use std::collections::BTreeSet;
use std::io::Write as _;

use chrono::{DateTime, Duration, Utc};
use clap::Subcommand;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::resolve_provider_session_selection_with_channel;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::report::{self, ClassifyInput, ReportInput, profile_of};
use crate::services::tui_o::shadow::root::{ShadowRoot, ShadowStore};
use crate::services::tui_o::shadow::{
    DISK_CAP_BYTES, PopulationChannel, PopulationProvider, PopulationSnapshot, PopulationSource,
    ShadowRecord, ShadowSink, SyntheticEntry, WindowStartSource,
};

const MANIFEST_FILE: &str = "synthetic_manifest.jsonl";

#[derive(Subcommand)]
pub(crate) enum OShadowCommand {
    /// Record t0 as the current size of every attached transcript; run once when the window opens
    WindowStart,
    /// Judge one window against the E1 bar; exits nonzero unless every recorded criterion passes
    Report {
        /// Earliest time of the window_start record that defines t0 (RFC 3339)
        #[arg(long)]
        from: DateTime<Utc>,
        /// Window end t1 (RFC 3339)
        #[arg(long)]
        to: DateTime<Utc>,
        /// JSON array of {diff_key, cause, note} operator classifications applied before judging
        #[arg(long)]
        classify: Option<std::path::PathBuf>,
    },
    /// Synthetic prompt manifest
    #[command(subcommand)]
    Manifest(ManifestAction),
}

#[derive(Subcommand)]
pub(crate) enum ManifestAction {
    /// Register a synthetic prompt before posting it; prints the token the prompt must contain
    Add {
        #[arg(long)]
        channel_id: u64,
        #[arg(long, value_parser = ["claude_tui", "codex_tui"])]
        expected_runtime_kind: String,
        #[arg(long)]
        prompt_id: String,
        #[arg(long, default_value_t = 0)]
        intended_tools: u32,
        #[arg(long)]
        intended_split: bool,
        #[arg(long)]
        operator: String,
    },
}

pub(crate) fn run(command: OShadowCommand) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root().ok_or("runtime root is unresolved")?;
    let root = ShadowRoot::under(&runtime_root).map_err(|e| format!("o_shadow root: {e}"))?;
    match command {
        OShadowCommand::WindowStart => window_start(&root),
        OShadowCommand::Report { from, to, classify } => {
            let classify = read_classify(classify.as_deref());
            super::direct::run_async(report(root, from, to, classify))
        }
        OShadowCommand::Manifest(ManifestAction::Add {
            channel_id,
            expected_runtime_kind,
            prompt_id,
            intended_tools,
            intended_split,
            operator,
        }) => {
            let created_at = Utc::now();
            let entry_id = format!("{prompt_id}-{}", created_at.timestamp_millis());
            let token = format!("[o-shadow-synth:{entry_id}]");
            let entry = SyntheticEntry {
                entry_id,
                channel_id,
                expected_runtime_kind,
                prompt_id,
                token: token.clone(),
                intended_tools,
                intended_split,
                operator,
                created_at,
            };
            let mut line = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
            line.push(b'\n');
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.path().join(MANIFEST_FILE))
                .and_then(|mut file| file.write_all(&line))
                .map_err(|e| format!("append synthetic manifest: {e}"))?;
            println!("{token}");
            Ok(())
        }
    }
}

fn open_store(root: &ShadowRoot) -> Result<ShadowStore, String> {
    ShadowStore::open(root.clone(), DISK_CAP_BYTES).map_err(|e| format!("o_shadow store: {e}"))
}

fn read_records(root: &ShadowRoot) -> Result<Vec<ShadowRecord>, String> {
    ShadowStore::read_records(root).map_err(|e| format!("read o_shadow records: {e}"))
}

/// fstat only; a path that now names another file keeps its next attach extent as the boundary.
fn window_start(root: &ShadowRoot) -> Result<(), String> {
    let (mut sources, mut skipped) = (Vec::new(), Vec::new());
    // t0 is fixed before the listing and fstat, so a closer appended in between stays below extent.
    let t0 = Utc::now();
    for (source, _) in report::attached_sources(&read_records(root)?) {
        let current = source_id_for(&source.session_id, &source.path);
        let extent = std::fs::metadata(&source.path).map(|meta| meta.len());
        match (current, extent) {
            (Ok(now), Ok(window_start_extent))
                if (now.dev, now.ino) == (source.dev, source.ino) =>
            {
                sources.push(WindowStartSource {
                    source,
                    window_start_extent,
                })
            }
            _ => skipped.push(source.path.display().to_string()),
        }
    }
    let attached = sources.len();
    let record = ShadowRecord::WindowStart { t0, sources };
    open_store(root)?
        .append(&record)
        .map_err(|e| format!("append window_start: {e}"))?;
    let summary = serde_json::json!({ "t0": t0, "sources": attached, "skipped": skipped });
    println!("{summary}");
    Ok(())
}

async fn report(
    root: ShadowRoot,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    classify: ClassifyInput,
) -> Result<(), String> {
    let path = crate::config::resolved_config_path();
    let bytes = std::fs::read(&path).map_err(|e| format!("read config {}: {e}", path.display()))?;
    let config = crate::config::load_from_path(&path).map_err(|e| format!("load config: {e}"))?;
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::from);
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let file = ConfigFile {
        path: path.display().to_string(),
        sha256,
        mtime,
    };
    let records = read_records(&root)?;
    let manifest = read_manifest(&root)?;
    let now = Utc::now();
    let sessions = recent_sessions(&config, from - Duration::days(7)).await;
    if let Err(error) = &sessions {
        tracing::warn!(%error, "o-shadow report: sessions history unavailable");
    }
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let resolve = |provider: &str, channel_id: Option<u64>| {
        ProviderKind::from_str(provider).is_some_and(|kind| {
            resolve_provider_session_selection_with_channel(&kind, true, channel_id)
                .requested_tui_hosting
        })
    };
    let observed: BTreeSet<String> = (sessions.iter().flatten())
        .filter(|(provider, channel_id)| resolve(provider, *channel_id))
        .map(|(provider, _)| profile_of(provider))
        .collect();
    let s3 = PopulationSource {
        name: "s3_sessions".into(),
        read_at: now,
        ok: sessions.is_ok(),
        observed_kinds: observed.into_iter().collect(),
    };
    let aux = vec![report::bound_kinds(&records, from, to, now), s3];
    let allowlist = config
        .tui_o
        .as_ref()
        .map(|t| t.shadow.channel_allowlist.clone())
        .unwrap_or_default();
    let threads: Vec<(u64, String)> = report::bound_channels(&records, to)
        .into_iter()
        .filter(|(channel_id, _)| allowlist.contains(channel_id))
        .collect();
    let snapshot = population(&config, file, aux, now, &resolve, &threads);
    let outcome = report::evaluate(&ReportInput {
        records: &records,
        manifest: &manifest,
        population: &snapshot,
        allowlist: &allowlist,
        from,
        to,
        reported_at: now,
        classify: &classify,
    });
    let snapshot_record = ShadowRecord::Population {
        snapshot: snapshot.clone(),
    };
    open_store(&root)?
        .append(&snapshot_record)
        .map_err(|e| format!("append population: {e}"))?;
    let output = serde_json::json!({ "population": snapshot, "report": outcome });
    println!(
        "{}",
        serde_json::to_string_pretty(&output).map_err(|e| e.to_string())?
    );
    if outcome.pass {
        Ok(())
    } else {
        Err(format!(
            "o-shadow report: FAIL ({} failures)",
            outcome.failures.len()
        ))
    }
}

/// A missing or malformed file becomes a report failure rather than an unclassified run.
fn read_classify(path: Option<&std::path::Path>) -> ClassifyInput {
    let Some(path) = path else {
        return ClassifyInput::Absent;
    };
    let parsed = std::fs::read(path)
        .map_err(|e| format!("read {}: {e}", path.display()))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
    parsed.map_or_else(ClassifyInput::Unreadable, ClassifyInput::Entries)
}

fn read_manifest(root: &ShadowRoot) -> Result<Vec<SyntheticEntry>, String> {
    let text = match std::fs::read_to_string(root.path().join(MANIFEST_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read synthetic manifest: {error}")),
    };
    let parse = |line: &str| {
        serde_json::from_str(line).map_err(|e| format!("synthetic manifest line: {e}"))
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse)
        .collect()
}

/// S3: (provider, channel) of sessions seen since `since`; read only, used only as a cross-check.
async fn recent_sessions(
    config: &Config,
    since: DateTime<Utc>,
) -> Result<Vec<(String, Option<u64>)>, String> {
    let pool = crate::db::postgres::connect(config)
        .await?
        .ok_or("PostgreSQL is disabled")?;
    let mut tx = pool.begin().await.map_err(|e| format!("begin: {e}"))?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("read only: {e}"))?;
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT provider, channel_id FROM sessions WHERE channel_id IS NOT NULL AND last_heartbeat >= $1",
    )
    .bind(since)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| format!("sessions: {e}"))?;
    tx.rollback().await.map_err(|e| format!("rollback: {e}"))?;
    pool.close().await;
    let row = |(provider, channel_id): (Option<String>, Option<String>)| {
        let provider = provider.unwrap_or_default().trim().to_ascii_lowercase();
        (provider, channel_id.and_then(|id| id.trim().parse().ok()))
    };
    Ok(rows.into_iter().map(row).collect())
}

struct ConfigFile {
    path: String,
    sha256: String,
    mtime: Option<DateTime<Utc>>,
}

/// Whether the dispatch resolver picks TUI for `(provider, channel)`; `None` is the provider default.
type TuiResolver<'a> = &'a dyn Fn(&str, Option<u64>) -> bool;

/// P = providers whose default resolves to TUI, plus providers of every numeric configured channel
/// that resolves to TUI. Name-only overrides are never installed, so they only warn.
fn population(
    config: &Config,
    file: ConfigFile,
    aux: Vec<PopulationSource>,
    taken_at: DateTime<Utc>,
    resolve: TuiResolver,
    threads: &[(u64, String)],
) -> PopulationSnapshot {
    let raw = |id: &str| {
        let found = config
            .providers
            .iter()
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(id));
        found
            .map(|(_, p)| (p.tui_hosting, p.runtime.clone()))
            .unwrap_or_default()
    };
    let providers: Vec<PopulationProvider> = crate::services::provider::supported_provider_ids()
        .into_iter()
        .map(|id| {
            let (tui_hosting, runtime) = raw(id);
            let (provider, effective_tui) = (id.to_string(), resolve(id, None));
            let basis = "resolver".to_string();
            PopulationProvider {
                provider,
                tui_hosting,
                runtime,
                effective_tui,
                basis,
            }
        })
        .collect();
    let (mut channels, mut warnings) = (Vec::new(), Vec::new());
    for (provider, channel) in configured_channels(config) {
        match channel.channel_id().and_then(|id| id.parse::<u64>().ok()) {
            Some(channel_id) if resolve(&provider, Some(channel_id)) => {
                let basis = "resolver".to_string();
                channels.push(PopulationChannel {
                    channel_id,
                    provider,
                    effective_tui: true,
                    basis,
                });
            }
            Some(_) => {}
            None if channel.runtime_mode_raw().is_some() || channel.tui_hosting().is_some() => {
                let name = channel.channel_name().unwrap_or_default();
                warnings.push(format!("ignored_name_only: {provider}/{name}"));
            }
            None => {}
        }
    }
    // An allowlisted channel with no config entry (a thread) takes the provider it was bound to.
    for (channel_id, provider) in threads {
        let configured = channels.iter().any(|c| c.channel_id == *channel_id);
        if !configured && resolve(provider, Some(*channel_id)) {
            let (channel_id, provider) = (*channel_id, provider.clone());
            let (effective_tui, basis) = (true, "resolver".to_string());
            channels.push(PopulationChannel {
                channel_id,
                provider,
                effective_tui,
                basis,
            });
        }
    }
    let defaults = providers
        .iter()
        .filter(|p| p.effective_tui)
        .map(|p| &p.provider);
    let profiles: BTreeSet<String> = defaults
        .chain(channels.iter().map(|c| &c.provider))
        .map(|p| profile_of(p))
        .collect();
    let (config_path, config_sha256, config_mtime) = (file.path, file.sha256, file.mtime);
    let profiles = profiles.into_iter().collect();
    PopulationSnapshot {
        taken_at,
        config_path,
        config_sha256,
        config_mtime,
        providers,
        channels,
        profiles,
        aux,
        warnings,
    }
}

fn configured_channels(
    config: &Config,
) -> impl Iterator<Item = (String, &crate::config::AgentChannel)> {
    config
        .agents
        .iter()
        .flat_map(|agent| agent.channels.iter())
        .map(|(kind, channel)| {
            let provider = channel.provider().unwrap_or_else(|| kind.to_string());
            (provider.trim().to_ascii_lowercase(), channel)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn population_joins_resolver_defaults_with_numeric_channels_and_warns_on_name_only() {
        let yaml = "server: {}\nagents:\n  - id: a\n    name: A\n    channels:\n      codex: {id: \"11\"}\n      claude: {name: only-name, runtime: tui}\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let resolve = |provider: &str, channel: Option<u64>| {
            matches!((provider, channel), ("claude", None) | ("codex", Some(11)))
        };
        let file = ConfigFile {
            path: String::new(),
            sha256: String::new(),
            mtime: None,
        };
        let snapshot = population(&config, file, Vec::new(), Utc::now(), &resolve, &[]);
        assert_eq!(snapshot.profiles, ["claude_tui", "codex_tui"]);
        assert_eq!(
            snapshot
                .channels
                .iter()
                .map(|c| c.channel_id)
                .collect::<Vec<_>>(),
            [11]
        );
        assert_eq!(snapshot.warnings, ["ignored_name_only: claude/only-name"]);
    }

    #[test]
    fn an_allowlisted_thread_bound_to_a_default_tui_provider_covers_its_profile() {
        let yaml = "server: {}\nagents:\n  - id: a\n    name: A\n    channels:\n      claude: {id: \"7\"}\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        // Claude defaults to TUI; channel 9 is a thread whose parent overrides nothing.
        let resolve =
            |provider: &str, channel: Option<u64>| provider == "claude" && channel != Some(7);
        let file = || ConfigFile {
            path: String::new(),
            sha256: String::new(),
            mtime: None,
        };
        let threads = [(9, "claude".to_string()), (7, "claude".to_string())];
        let snapshot = population(&config, file(), Vec::new(), Utc::now(), &resolve, &threads);
        let covered: Vec<(u64, &str)> = (snapshot.channels.iter())
            .map(|c| (c.channel_id, c.provider.as_str()))
            .collect();
        assert_eq!(covered, [(9, "claude")]);
        let codex = [(9, "codex".to_string())];
        let snapshot = population(&config, file(), Vec::new(), Utc::now(), &resolve, &codex);
        assert!(snapshot.channels.is_empty());
    }
}
