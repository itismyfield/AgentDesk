use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::services::cswap::{ClaudeAccountsResponse, ClaudeAccountsStatus, CswapService};
use crate::services::provider_auth_profile::ProviderAuthOverlay;

const MAX_AGE_SECONDS: i64 = 15 * 60;

mod statusline;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Quota {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

// Native starts follow launch selection; Legacy intake timestamps precede it.
#[derive(Clone, Copy)]
pub(crate) enum Selected {
    Before(DateTime<Utc>),
    Within(DateTime<Utc>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window {
    percent: u8,
    reset: DateTime<Utc>,
}

impl Window {
    fn new(percent: f64, reset: DateTime<Utc>, now: DateTime<Utc>) -> Option<Self> {
        (percent.is_finite() && (0.0..=100.0).contains(&percent) && reset > now).then_some(Self {
            percent: percent.round() as u8,
            reset,
        })
    }
}

impl Quota {
    pub(super) fn append(self, line: &mut String) {
        for (label, window, format) in [
            ("5h", self.five_hour, "%H:%M"),
            ("7d", self.seven_day, "%m/%d %H:%M"),
        ] {
            if let Some(window) = window {
                let reset = window.reset.with_timezone(&chrono_tz::Asia::Seoul);
                line.push_str(&format!(
                    " · {label} {}% ({})",
                    window.percent,
                    reset.format(format)
                ));
            }
        }
    }

    fn current(mut self, now: DateTime<Utc>) -> Option<Self> {
        self.five_hour = self.five_hour.filter(|window| window.reset > now);
        self.seven_day = self.seven_day.filter(|window| window.reset > now);
        (self.five_hour.is_some() || self.seven_day.is_some()).then_some(self)
    }
}

fn fresh(fetched: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(fetched);
    (chrono::Duration::zero()..=chrono::Duration::seconds(MAX_AGE_SECONDS)).contains(&age)
}

fn claude(response: &ClaudeAccountsResponse, now: DateTime<Utc>) -> Option<Quota> {
    if !fresh(response.fetched_at, now)
        || response.status != ClaudeAccountsStatus::Ok
        || response.usage_data_stale
    {
        return None;
    }
    let number = response.active_account_number?;
    let mut active = response.accounts.iter().filter(|account| account.active);
    let account = active.next()?;
    if active.next().is_some()
        || account.number != Some(number)
        || account.usage_status.as_deref() != Some("ok")
    {
        return None;
    }
    if let Some(fetched) = &account.usage_fetched_at {
        if !fresh(fetched.parse().ok()?, now) {
            return None;
        }
    } else {
        let base_age = account.usage_age_seconds?;
        if !base_age.is_finite() || base_age < 0.0 {
            return None;
        }
        let age = base_age
            + now
                .signed_duration_since(response.fetched_at)
                .num_milliseconds() as f64
                / 1000.0;
        if !age.is_finite() || !(0.0..=MAX_AGE_SECONDS as f64).contains(&age) {
            return None;
        }
    }
    let usage = account.usage.as_ref()?;
    let window = |value: &crate::services::cswap::ClaudeAccountUsageWindow| {
        Window::new(value.pct?, value.resets_at.as_ref()?.parse().ok()?, now)
    };
    Quota {
        five_hour: usage.five_hour.as_ref().and_then(window),
        seven_day: usage.seven_day.as_ref().and_then(window),
    }
    .current(now)
}

fn codex(data: &str, now: DateTime<Utc>) -> Option<Quota> {
    let value: Value = serde_json::from_str(data).ok()?;
    let buckets = value.get("buckets")?.as_array()?;
    let window = |name: &str| {
        let mut matching = buckets.iter().filter(|bucket| bucket["name"] == name);
        let bucket = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let limit = bucket.get("limit")?.as_f64()?;
        let used = bucket.get("used")?.as_f64()?;
        if !limit.is_finite() || limit <= 0.0 {
            return None;
        }
        let reset = DateTime::from_timestamp(bucket.get("reset")?.as_i64()?, 0)?;
        Window::new(used / limit * 100.0, reset, now)
    };
    Quota {
        five_hour: window("5h"),
        seven_day: window("7d"),
    }
    .current(now)
}

#[derive(Default)]
struct Cache {
    quotas: BTreeMap<String, (DateTime<Utc>, Quota)>,
    profiles: BTreeMap<(String, u64), Selection>,
}

struct Selection {
    at: DateTime<Utc>,
    profile: String,
}

static CACHE: LazyLock<RwLock<Cache>> = LazyLock::new(RwLock::default);

pub(crate) fn record_launch(
    channel: Option<u64>,
    overlay: ProviderAuthOverlay,
) -> Result<ProviderAuthOverlay, String> {
    if let Some(channel) = channel
        && let Ok(mut cache) = CACHE.write()
    {
        cache.profiles.insert(
            (overlay.provider.as_str().to_string(), channel),
            Selection {
                at: Utc::now(),
                profile: overlay.profile_id.clone(),
            },
        );
    }
    Ok(overlay)
}

pub(crate) fn publish(provider: &str, profile: &str, data: &str, fetched_at: i64) {
    if provider != "codex" {
        return;
    }
    let Some(fetched) = DateTime::from_timestamp(fetched_at, 0) else {
        return;
    };
    let quota = codex(data, fetched);
    if let Ok(mut cache) = CACHE.write() {
        cache.quotas.remove(profile);
        if let Some(quota) = quota {
            cache.quotas.insert(profile.to_string(), (fetched, quota));
        }
    }
}

fn cached_codex(
    cache: &RwLock<Cache>,
    channel: u64,
    selection: Selected,
    now: DateTime<Utc>,
) -> Option<Quota> {
    let cache = cache.try_read().ok()?;
    let selected = cache.profiles.get(&("codex".to_string(), channel))?;
    let allowed = match selection {
        Selected::Before(started) => selected.at <= started,
        Selected::Within(started) => (started..=now).contains(&selected.at),
    };
    if !allowed {
        return None;
    }
    let (fetched, quota) = cache.quotas.get(&selected.profile)?;
    fresh(*fetched, now).then_some(*quota)?.current(now)
}

pub(crate) fn snapshot(
    provider: &str,
    channel: u64,
    selection: Option<Selected>,
    session_id: Option<&str>,
) -> Option<Quota> {
    let now = Utc::now();
    match provider {
        "claude" => statusline::with_fallback(
            &crate::services::claude_tui::hook_bundle::statusline::record_dir(),
            session_id,
            now,
            || {
                let cache = CACHE.try_read().ok()?;
                if cache
                    .profiles
                    .get(&(provider.to_string(), channel))
                    .is_some_and(|selected| selected.profile != "default")
                {
                    return None;
                }
                claude(&CswapService::global().cached_accounts_for_summary()?, now)
            },
        ),
        "codex" => cached_codex(&CACHE, channel, selection?, now),
        _ => None,
    }
}

pub(crate) fn legacy_started(raw: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .ok()?
        .and_local_timezone(chrono::Local)
        .single()
        .map(|started| started.with_timezone(&Utc))
}

#[cfg(test)]
#[path = "quota_tests.rs"]
pub(crate) mod tests;
