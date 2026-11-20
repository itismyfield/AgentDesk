use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::Model;

use crate::services::cswap::{ClaudeAccountsResponse, ClaudeAccountsStatus, CswapService};
use crate::services::provider_auth_profile::ProviderAuthOverlay;

const MAX_AGE_SECONDS: i64 = 15 * 60;

mod statusline;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Quota {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    fable_seven_day: Option<Window>,
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
        let windows = [
            ("5h", self.five_hour, "%H:%M"),
            ("7d", self.seven_day, "%m/%d %H:%M"),
            ("Fable 7d", self.fable_seven_day, "%m/%d %H:%M"),
        ];
        for (index, (label, window, format)) in windows
            .into_iter()
            .filter_map(|(label, window, format)| Some((label, window?, format)))
            .enumerate()
        {
            let reset = window.reset.with_timezone(&chrono_tz::Asia::Seoul);
            line.push_str(if index == 0 { "\n-# " } else { " · " });
            line.push_str(&format!(
                "{label} {}% ({})",
                window.percent,
                reset.format(format)
            ));
        }
    }

    fn current(mut self, now: DateTime<Utc>) -> Option<Self> {
        self.five_hour = self.five_hour.filter(|window| window.reset > now);
        self.seven_day = self.seven_day.filter(|window| window.reset > now);
        self.fable_seven_day = self.fable_seven_day.filter(|window| window.reset > now);
        (self.five_hour.is_some() || self.seven_day.is_some() || self.fable_seven_day.is_some())
            .then_some(self)
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
        fable_seven_day: usage
            .extra
            .get("scoped")
            .and_then(|scoped| fable_scoped(scoped, now)),
    }
    .current(now)
}

fn fable_scoped(scoped: &Value, now: DateTime<Utc>) -> Option<Window> {
    let mut fable = scoped
        .as_array()?
        .iter()
        .filter(|window| window["name"] == "Fable");
    let window = fable.next()?;
    if fable.next().is_some() {
        return None;
    }
    Window::new(
        window["pct"].as_f64()?,
        window["resetsAt"].as_str()?.parse().ok()?,
        now,
    )
}

fn buckets(data: &str, now: DateTime<Utc>, claude: bool) -> Option<Quota> {
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
        fable_seven_day: if claude { window("7d Fable") } else { None },
    }
    .current(now)
}

#[derive(Default)]
struct Cache {
    quotas: BTreeMap<(String, String), (DateTime<Utc>, Quota)>,
    profiles: BTreeMap<(String, u64), Selection>,
}

struct Selection {
    at: DateTime<Utc>,
    profile: String,
    default_claude_account: Option<u64>,
}

static CACHE: LazyLock<RwLock<Cache>> = LazyLock::new(RwLock::default);

pub(crate) fn record_launch(
    channel: Option<u64>,
    overlay: ProviderAuthOverlay,
) -> Result<ProviderAuthOverlay, String> {
    let now = Utc::now();
    let default_claude_account = (overlay.provider.as_str() == "claude"
        && overlay.profile_id == "default")
        .then(|| CswapService::global().cached_accounts_for_summary())
        .flatten()
        .filter(|response| claude(response, now).is_some())
        .and_then(|response| response.active_account_number);
    if let Some(channel) = channel
        && let Ok(mut cache) = CACHE.write()
    {
        cache.profiles.insert(
            (overlay.provider.as_str().to_string(), channel),
            Selection {
                at: now,
                profile: overlay.profile_id.clone(),
                default_claude_account,
            },
        );
    }
    Ok(overlay)
}

pub(crate) fn publish(provider: &str, profile: &str, data: &str, fetched_at: i64) {
    if provider != "codex" && (provider != "claude" || profile == "default") {
        return;
    }
    let Some(fetched) = DateTime::from_timestamp(fetched_at, 0) else {
        return;
    };
    let quota = buckets(data, fetched, provider == "claude");
    if let Ok(mut cache) = CACHE.write() {
        let key = (provider.to_string(), profile.to_string());
        if let Some(quota) = quota {
            cache.quotas.insert(key, (fetched, quota));
        } else {
            cache.quotas.remove(&key);
        }
    }
}

impl Selection {
    fn allows(&self, selection: Selected, now: DateTime<Utc>) -> bool {
        match selection {
            Selected::Before(started) => self.at <= started,
            Selected::Within(started) => (started..=now).contains(&self.at),
        }
    }
}

fn profile_quota(
    cache: &Cache,
    provider: &str,
    profile: &str,
    now: DateTime<Utc>,
) -> Option<Quota> {
    let key = (provider.to_string(), profile.to_string());
    let (fetched, quota) = cache.quotas.get(&key)?;
    fresh(*fetched, now).then_some(*quota)?.current(now)
}

fn cached_codex(
    cache: &RwLock<Cache>,
    channel: u64,
    selection: Selected,
    now: DateTime<Utc>,
) -> Option<Quota> {
    let cache = cache.try_read().ok()?;
    let selected = cache.profiles.get(&("codex".to_string(), channel))?;
    if !selected.allows(selection, now) {
        return None;
    }
    profile_quota(&cache, "codex", &selected.profile, now)
}

fn cached_claude(
    channel: u64,
    selection: Option<Selected>,
    session_id: &str,
    logical_session: &str,
    now: DateTime<Utc>,
) -> Option<Quota> {
    // The binding names the wrapper, and its auth marker must name this native session.
    let profile =
        crate::services::tmux_common::claude_session_auth(logical_session, session_id).ok()??;
    let cache = CACHE.try_read().ok()?;
    if profile != "default" {
        return profile_quota(&cache, "claude", &profile, now);
    }
    let selected = cache.profiles.get(&("claude".to_string(), channel))?;
    if selected.profile != profile || !selected.allows(selection?, now) {
        return None;
    }
    let account = selected.default_claude_account?;
    drop(cache);
    let response = CswapService::global().cached_accounts_for_summary()?;
    (response.active_account_number == Some(account)).then(|| claude(&response, now))?
}

fn merge_claude(session: Option<Quota>, account: Option<Quota>) -> Option<Quota> {
    if let Some(mut session) = session {
        session.fable_seven_day = account.and_then(|quota| quota.fable_seven_day);
        Some(session)
    } else {
        account
    }
}

pub(crate) struct Snapshot {
    pub quota: Option<Quota>,
    statusline: Option<statusline::Record>,
}

impl Snapshot {
    pub(crate) fn model(&self) -> Model<'_> {
        self.statusline
            .as_ref()
            .map_or_else(Model::default, |record| Model {
                name: record.model(),
                effort: record.effort(),
                fast: record.fast(),
            })
    }

    pub(crate) fn model_between(&self, turn: Option<(DateTime<Utc>, DateTime<Utc>)>) -> Model<'_> {
        if self
            .statusline
            .as_ref()
            .zip(turn)
            .is_some_and(|(record, (start, end))| (start..=end).contains(&record.observed_at()))
        {
            self.model()
        } else {
            Model::default()
        }
    }
}

pub(crate) fn snapshot(
    provider: &str,
    channel: u64,
    selection: Option<Selected>,
    session_id: Option<&str>,
    logical_session: Option<&str>,
    show_quota: bool,
) -> Snapshot {
    let now = Utc::now();
    let statusline = if provider == "claude" {
        session_id.and_then(|session| {
            statusline::read(
                &crate::services::claude_tui::hook_bundle::statusline::record_dir(),
                session,
                now,
            )
        })
    } else {
        None
    };
    let quota = if show_quota {
        match provider {
            "claude" => merge_claude(
                statusline.as_ref().and_then(|record| record.quota(now)),
                session_id
                    .zip(logical_session)
                    .and_then(|(session, logical)| {
                        cached_claude(channel, selection, session, logical, now)
                    }),
            ),
            "codex" => {
                selection.and_then(|selection| cached_codex(&CACHE, channel, selection, now))
            }
            _ => None,
        }
    } else {
        None
    };
    Snapshot { quota, statusline }
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
