use std::collections::{HashMap, HashSet};
use std::process::Command;

use chrono::{DateTime, NaiveDateTime, Utc};

#[cfg(unix)]
use crate::services::tmux_diagnostics::tmux_session_has_live_pane;

const REMOTE_HEARTBEAT_GRACE_SECS: i64 = 90;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSessionState {
    pub status: &'static str,
    pub active_dispatch_id: Option<String>,
    pub is_working: bool,
}

#[derive(Default)]
pub struct SessionActivityResolver {
    local_host_aliases: Option<HashSet<String>>,
    tmux_live_cache: HashMap<String, bool>,
}

impl SessionActivityResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(
        &mut self,
        session_key: Option<&str>,
        raw_status: Option<&str>,
        active_dispatch_id: Option<&str>,
        last_heartbeat: Option<&str>,
    ) -> EffectiveSessionState {
        let local_host_aliases = self.local_host_aliases().clone();
        let now = Utc::now();
        let cache = &mut self.tmux_live_cache;
        let mut probe_tmux = |tmux_name: &str| {
            if let Some(cached) = cache.get(tmux_name) {
                return *cached;
            }
            #[cfg(unix)]
            let live = tmux_session_has_live_pane(tmux_name);
            #[cfg(not(unix))]
            let live = false; // tmux not available on Windows
            cache.insert(tmux_name.to_string(), live);
            live
        };

        resolve_effective_state_with(
            &local_host_aliases,
            session_key,
            raw_status,
            active_dispatch_id,
            last_heartbeat,
            now,
            &mut probe_tmux,
        )
    }

    fn local_host_aliases(&mut self) -> &HashSet<String> {
        if self.local_host_aliases.is_none() {
            self.local_host_aliases = Some(load_local_host_aliases());
        }
        self.local_host_aliases
            .as_ref()
            .expect("local_host_aliases initialized")
    }
}

fn load_local_host_aliases() -> HashSet<String> {
    let mut aliases = HashSet::new();
    for args in [vec!["-s"], Vec::<&str>::new()] {
        let mut cmd = Command::new("hostname");
        cmd.args(&args);
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                if let Ok(text) = String::from_utf8(output.stdout) {
                    if let Some(host) = normalize_host(&text) {
                        aliases.insert(host);
                    }
                }
            }
        }
    }
    aliases
}

fn resolve_effective_state_with<F>(
    local_host_aliases: &HashSet<String>,
    session_key: Option<&str>,
    raw_status: Option<&str>,
    active_dispatch_id: Option<&str>,
    last_heartbeat: Option<&str>,
    now: DateTime<Utc>,
    probe_tmux: &mut F,
) -> EffectiveSessionState
where
    F: FnMut(&str) -> bool,
{
    let status = raw_status.unwrap_or("idle").trim();
    if status.eq_ignore_ascii_case("disconnected") {
        return EffectiveSessionState {
            status: "disconnected",
            active_dispatch_id: None,
            is_working: false,
        };
    }

    let has_work_signal = status.eq_ignore_ascii_case("working") || active_dispatch_id.is_some();
    let is_live = if has_work_signal {
        match session_key.and_then(parse_session_key) {
            Some((host, tmux_name)) if local_host_aliases.contains(&host) => probe_tmux(&tmux_name),
            Some(_) => heartbeat_is_recent(last_heartbeat, now),
            None => heartbeat_is_recent(last_heartbeat, now),
        }
    } else {
        false
    };

    EffectiveSessionState {
        status: if is_live && has_work_signal {
            "working"
        } else {
            "idle"
        },
        active_dispatch_id: if is_live {
            active_dispatch_id.map(str::to_string)
        } else {
            None
        },
        is_working: is_live && has_work_signal,
    }
}

fn parse_session_key(session_key: &str) -> Option<(String, String)> {
    let (host, tmux_name) = session_key.split_once(':')?;
    let host = normalize_host(host)?;
    let tmux_name = tmux_name.trim();
    if tmux_name.is_empty() {
        return None;
    }
    Some((host, tmux_name.to_string()))
}

fn normalize_host(host: &str) -> Option<String> {
    let trimmed = host.trim().trim_end_matches(".local").trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn heartbeat_is_recent(last_heartbeat: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(raw) = last_heartbeat
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
        });

    parsed
        .map(|value| (now - value).num_seconds() <= REMOTE_HEARTBEAT_GRACE_SECS)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn local_aliases() -> HashSet<String> {
        ["mac-mini".to_string()].into_iter().collect()
    }

    #[test]
    fn local_dead_tmux_session_becomes_idle() {
        let now = Utc::now();
        let mut probe = |_name: &str| false;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("mac-mini:AgentDesk-claude-ad"),
            Some("working"),
            Some("dispatch-1"),
            Some(
                &(now - Duration::seconds(5))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            now,
            &mut probe,
        );

        assert_eq!(state.status, "idle");
        assert_eq!(state.active_dispatch_id, None);
        assert!(!state.is_working);
    }

    #[test]
    fn remote_fresh_heartbeat_stays_working() {
        let now = Utc::now();
        let heartbeat = (now - Duration::seconds(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut probe = |_name: &str| false;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("remote-host:AgentDesk-codex-adk-cdx"),
            Some("working"),
            Some("dispatch-2"),
            Some(&heartbeat),
            now,
            &mut probe,
        );

        assert_eq!(state.status, "working");
        assert_eq!(state.active_dispatch_id.as_deref(), Some("dispatch-2"));
        assert!(state.is_working);
    }
}
