//! Shared terminal metadata for Discord completion footers.

use sqlx::Row;

use super::{ProviderKind, SharedData};

const ELAPSED_PREFIX: &str = "⏱ ";
const RATE_LIMIT_PREFIX: &str = "⏳ ";
const HOST_PREFIX: &str = "🖥️ ";
const METADATA_SUFFIX_SEPARATOR: &str = "\u{2063}";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterMetadata {
    elapsed: Option<String>,
    rate_limit: Option<String>,
    host: Option<String>,
}

pub(in crate::services::discord) async fn load_completion_footer_metadata(
    shared: &SharedData,
    provider: &ProviderKind,
    owner_started_at_unix: i64,
    inflight_started_at: Option<&str>,
) -> CompletionFooterMetadata {
    let rate_limit = terminal_rate_limit_summary(shared, provider).await;
    let host = crate::config_live_reload::current()
        .is_some_and(|config| config.cluster.enabled)
        .then(crate::services::cluster::node_registry::resolve_self_instance_id_without_config);
    completion_footer_metadata_at(
        chrono::Utc::now().timestamp(),
        owner_started_at_unix,
        inflight_started_at,
        rate_limit,
        host,
    )
}

pub(in crate::services::discord) fn append_completion_footer_metadata(
    block: String,
    metadata: &CompletionFooterMetadata,
) -> String {
    let (content, existing) = split_metadata_suffix(&block);
    let merged = CompletionFooterMetadata {
        elapsed: existing.elapsed.or_else(|| metadata.elapsed.clone()),
        rate_limit: existing.rate_limit.or_else(|| metadata.rate_limit.clone()),
        host: existing.host.or_else(|| metadata.host.clone()),
    };
    let lines = merged.lines();
    if lines.is_empty() {
        return block;
    }
    let content = content.trim_end();
    if content.is_empty() {
        format!("{METADATA_SUFFIX_SEPARATOR}\n{}", lines.join("\n"))
    } else {
        format!(
            "{content}\n\n{METADATA_SUFFIX_SEPARATOR}\n{}",
            lines.join("\n")
        )
    }
}

pub(in crate::services::discord) fn completion_footer_metadata_from_block(
    block: Option<&str>,
) -> CompletionFooterMetadata {
    block
        .map(split_metadata_suffix)
        .map(|(_, metadata)| metadata)
        .unwrap_or_default()
}

impl CompletionFooterMetadata {
    pub(in crate::services::discord) fn subtext_lines(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .map(|line| format!("-# {line}"))
            .collect()
    }

    fn lines(&self) -> Vec<String> {
        [
            self.elapsed
                .as_deref()
                .map(|value| format!("{ELAPSED_PREFIX}{value}")),
            self.rate_limit
                .as_deref()
                .map(|value| format!("{RATE_LIMIT_PREFIX}{value}")),
            self.host
                .as_deref()
                .map(|value| format!("{HOST_PREFIX}{value}")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn split_metadata_suffix(block: &str) -> (&str, CompletionFooterMetadata) {
    let separator = format!("\n{METADATA_SUFFIX_SEPARATOR}\n");
    let Some((content, suffix)) = block.split_once(&separator) else {
        return (block, CompletionFooterMetadata::default());
    };
    let mut metadata = CompletionFooterMetadata::default();
    for segment in suffix.split(&separator) {
        for line in segment.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix(ELAPSED_PREFIX) {
                metadata
                    .elapsed
                    .get_or_insert_with(|| value.trim().to_string());
            } else if let Some(value) = line.strip_prefix(RATE_LIMIT_PREFIX) {
                metadata
                    .rate_limit
                    .get_or_insert_with(|| value.trim().to_string());
            } else if let Some(value) = line.strip_prefix(HOST_PREFIX) {
                metadata
                    .host
                    .get_or_insert_with(|| value.trim().to_string());
            } else {
                return (block, CompletionFooterMetadata::default());
            }
        }
    }
    if metadata.lines().is_empty() {
        (block, CompletionFooterMetadata::default())
    } else {
        (content, metadata)
    }
}

fn completion_footer_metadata_at(
    now_unix: i64,
    owner_started_at_unix: i64,
    inflight_started_at: Option<&str>,
    rate_limit: Option<String>,
    host: Option<String>,
) -> CompletionFooterMetadata {
    let started_at_unix = (owner_started_at_unix > 0)
        .then_some(owner_started_at_unix)
        .or_else(|| inflight_started_at.and_then(super::inflight::parse_started_at_unix));
    let elapsed = started_at_unix
        .map(|started_at| now_unix.saturating_sub(started_at))
        .filter(|elapsed_secs| *elapsed_secs > 0)
        .map(format_turn_duration);
    CompletionFooterMetadata {
        elapsed,
        rate_limit: rate_limit.and_then(|value| nonempty(&value)),
        host: host.and_then(|value| nonempty(&value)),
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn format_turn_duration(total_secs: i64) -> String {
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

async fn terminal_rate_limit_summary(
    shared: &SharedData,
    provider: &ProviderKind,
) -> Option<String> {
    let pool = shared.pg_pool.as_ref()?;
    let provider = provider.as_str();
    let row =
        sqlx::query("SELECT data FROM rate_limit_cache WHERE lower(provider) = lower($1) LIMIT 1")
            .bind(provider)
            .fetch_optional(pool)
            .await
            .ok()??;
    let data = row.try_get::<String, _>("data").ok()?;
    let buckets = serde_json::from_str::<serde_json::Value>(&data)
        .ok()?
        .get("buckets")?
        .as_array()?
        .iter()
        .filter_map(|bucket| {
            let name = bucket.get("name")?.as_str()?;
            let remaining = bucket.get("remaining")?.as_i64()?.clamp(0, 100);
            matches!(name, "5h" | "7d").then(|| format!("{name} {remaining}%"))
        })
        .collect::<Vec<_>>();
    (!buckets.is_empty()).then(|| buckets.join(" · "))
}

#[cfg(test)]
pub(in crate::services::discord) mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ownerless_metadata_uses_inflight_started_at_fallback_4806() {
        let fallback_started_at = chrono::Local
            .timestamp_opt(1_800_000_000, 0)
            .single()
            .expect("valid local timestamp")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let metadata =
            completion_footer_metadata_at(1_800_000_154, 0, Some(&fallback_started_at), None, None);
        let block = append_completion_footer_metadata("Context".to_string(), &metadata);

        assert_eq!(
            block,
            format!("Context\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ 2m 34s")
        );
    }

    #[test]
    fn metadata_appender_is_idempotent_and_cache_miss_omits_quota_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            None,
            Some("node-a".to_string()),
        );
        let once = append_completion_footer_metadata("Context".to_string(), &metadata);
        let twice = append_completion_footer_metadata(once.clone(), &metadata);

        assert_eq!(twice, once);
        assert_eq!(twice.matches("⏱ ").count(), 1);
        assert_eq!(twice.matches("🖥️ ").count(), 1);
        assert!(!twice.contains("⏳ "));
    }

    pub(in crate::services::discord) fn metadata_fixture_for_task_card_4806()
    -> CompletionFooterMetadata {
        completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        )
    }

    #[test]
    fn bridge_and_watcher_finalize_inputs_render_identical_metadata_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        );
        let bridge_block = append_completion_footer_metadata("Context".to_string(), &metadata);
        let watcher_block = append_completion_footer_metadata("Context".to_string(), &metadata);

        assert_eq!(bridge_block, watcher_block);
        assert!(bridge_block.contains("⏱ 2m 34s"));
        assert!(bridge_block.contains("⏳ 5h 80% · 7d 60%"));
        assert!(bridge_block.contains("🖥️ node-a"));
    }

    #[test]
    fn metadata_roundtrip_preserves_all_available_lines_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        );
        let initial = append_completion_footer_metadata("Context".to_string(), &metadata);
        let recovered = completion_footer_metadata_from_block(Some(&initial));
        let refreshed =
            append_completion_footer_metadata("Subagents\n└ worker ⠸".to_string(), &recovered);

        assert!(refreshed.contains("⏱ 2m 34s"));
        assert!(refreshed.contains("⏳ 5h 80% · 7d 60%"));
        assert!(refreshed.contains("🖥️ node-a"));
    }

    #[test]
    fn quoted_prefix_lines_never_suppress_or_recover_metadata_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80%".to_string()),
            Some("node-a".to_string()),
        );
        let quoted = "Tasks\n⏱ quoted duration\n⏳ quoted quota\n🖥️ quoted host";
        let appended = append_completion_footer_metadata(quoted.to_string(), &metadata);

        assert_eq!(appended.matches("⏱ ").count(), 2);
        assert_eq!(appended.matches("⏳ ").count(), 2);
        assert_eq!(appended.matches("🖥️ ").count(), 2);
        assert_eq!(
            completion_footer_metadata_from_block(Some(quoted)),
            CompletionFooterMetadata::default()
        );
    }

    #[test]
    fn duplicate_separators_never_leave_old_metadata_in_content_4806() {
        let duplicated = format!(
            "Context\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ 2m 34s\n\n{METADATA_SUFFIX_SEPARATOR}\n⏳ 5h 80%"
        );
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80%".to_string()),
            None,
        );
        let repaired = append_completion_footer_metadata(duplicated, &metadata);

        assert_eq!(repaired.matches(METADATA_SUFFIX_SEPARATOR).count(), 1);
        assert_eq!(repaired.matches("⏱ ").count(), 1);
        assert_eq!(repaired.matches("⏳ ").count(), 1);
        assert!(repaired.starts_with("Context\n\n"));
    }

    #[test]
    fn malformed_suffix_does_not_become_refresh_metadata_4806() {
        let block =
            format!("Tasks\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ quoted duration\nnot metadata");
        assert_eq!(
            completion_footer_metadata_from_block(Some(&block)),
            CompletionFooterMetadata::default()
        );
    }
}
