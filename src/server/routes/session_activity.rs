use std::collections::{HashMap, HashSet};
use std::process::Command;

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::db::session_status::{
    AWAITING_BG, AWAITING_USER, DISCONNECTED, IDLE, is_active_status, is_bg_wait_status,
    is_terminal_status, is_user_wait_status, normalize_session_status,
};

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
    tmux_ready_cache: HashMap<String, bool>,
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
        let live_cache = &mut self.tmux_live_cache;
        let ready_cache = &mut self.tmux_ready_cache;
        let mut probe_tmux_live = |tmux_name: &str| {
            if let Some(cached) = live_cache.get(tmux_name) {
                return *cached;
            }
            #[cfg(unix)]
            let live = tmux_session_has_live_pane(tmux_name);
            #[cfg(not(unix))]
            let live = false; // tmux not available on Windows
            live_cache.insert(tmux_name.to_string(), live);
            live
        };
        let mut probe_tmux_ready = |tmux_name: &str| {
            if let Some(cached) = ready_cache.get(tmux_name) {
                return *cached;
            }
            #[cfg(unix)]
            let ready = crate::services::provider::tmux_session_ready_for_input(tmux_name);
            #[cfg(not(unix))]
            let ready = false; // tmux not available on Windows
            ready_cache.insert(tmux_name.to_string(), ready);
            ready
        };

        resolve_effective_state_with(
            &local_host_aliases,
            session_key,
            raw_status,
            active_dispatch_id,
            last_heartbeat,
            now,
            &mut probe_tmux_live,
            &mut probe_tmux_ready,
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

fn resolve_effective_state_with<LiveProbe, ReadyProbe>(
    local_host_aliases: &HashSet<String>,
    session_key: Option<&str>,
    raw_status: Option<&str>,
    active_dispatch_id: Option<&str>,
    last_heartbeat: Option<&str>,
    now: DateTime<Utc>,
    probe_tmux_live: &mut LiveProbe,
    probe_tmux_ready: &mut ReadyProbe,
) -> EffectiveSessionState
where
    LiveProbe: FnMut(&str) -> bool,
    ReadyProbe: FnMut(&str) -> bool,
{
    let status = normalize_session_status(raw_status, 0);
    if is_terminal_status(status) {
        return EffectiveSessionState {
            status,
            active_dispatch_id: None,
            is_working: false,
        };
    }

    let has_work_signal = is_active_status(status) || active_dispatch_id.is_some();
    let is_live = if has_work_signal {
        match session_key.and_then(parse_session_key) {
            Some((host, tmux_name)) if local_host_aliases.contains(&host) => {
                let tmux_live = probe_tmux_live(&tmux_name);
                let tmux_ready = tmux_live && probe_tmux_ready(&tmux_name);
                tmux_live && !tmux_ready
            }
            Some(_) => heartbeat_is_recent(last_heartbeat, now),
            None => heartbeat_is_recent(last_heartbeat, now),
        }
    } else {
        false
    };
    let effective_status = if is_live && has_work_signal {
        crate::db::session_status::TURN_ACTIVE
    } else if is_bg_wait_status(status) {
        AWAITING_BG
    } else if is_user_wait_status(status) {
        if status == IDLE { IDLE } else { AWAITING_USER }
    } else if status == DISCONNECTED {
        DISCONNECTED
    } else {
        IDLE
    };

    EffectiveSessionState {
        status: effective_status,
        active_dispatch_id: if is_live {
            active_dispatch_id.map(str::to_string)
        } else {
            None
        },
        is_working: effective_status == crate::db::session_status::TURN_ACTIVE
            || effective_status == AWAITING_BG,
    }
}

fn parse_session_key(session_key: &str) -> Option<(String, String)> {
    let (host_prefix, tmux_name) = session_key.split_once(':')?;
    let host = host_prefix.rsplit('/').next().and_then(normalize_host)?;
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
        let mut probe_live = |_name: &str| false;
        let mut probe_ready = |_name: &str| false;
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
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "idle");
        assert_eq!(state.active_dispatch_id, None);
        assert!(!state.is_working);
    }

    #[test]
    fn remote_fresh_heartbeat_stays_turn_active() {
        let now = Utc::now();
        let heartbeat = (now - Duration::seconds(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut probe_live = |_name: &str| false;
        let mut probe_ready = |_name: &str| false;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("remote-host:AgentDesk-codex-adk-cdx"),
            Some("working"),
            Some("dispatch-2"),
            Some(&heartbeat),
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "turn_active");
        assert_eq!(state.active_dispatch_id.as_deref(), Some("dispatch-2"));
        assert!(state.is_working);
    }

    #[test]
    fn local_ready_for_input_tmux_session_becomes_idle() {
        let now = Utc::now();
        let mut probe_live = |_name: &str| true;
        let mut probe_ready = |_name: &str| true;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("mac-mini:AgentDesk-codex-adk-dash-cdx"),
            Some("working"),
            Some("dispatch-3"),
            Some(
                &(now - Duration::seconds(5))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "idle");
        assert_eq!(state.active_dispatch_id, None);
        assert!(!state.is_working);
    }

    #[test]
    fn prefixed_local_ready_for_input_tmux_session_becomes_idle() {
        let now = Utc::now();
        let mut probe_live = |_name: &str| true;
        let mut probe_ready = |_name: &str| true;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("codex/discord_key/mac-mini:AgentDesk-codex-adk-dash-cdx"),
            Some("working"),
            Some("dispatch-3b"),
            Some(
                &(now - Duration::seconds(5))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "idle");
        assert_eq!(state.active_dispatch_id, None);
        assert!(!state.is_working);
    }

    #[test]
    fn local_live_tmux_without_ready_prompt_stays_turn_active() {
        let now = Utc::now();
        let mut probe_live = |_name: &str| true;
        let mut probe_ready = |_name: &str| false;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("mac-mini:AgentDesk-codex-adk-dash-cdx"),
            Some("working"),
            Some("dispatch-4"),
            Some(
                &(now - Duration::seconds(5))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "turn_active");
        assert_eq!(state.active_dispatch_id.as_deref(), Some("dispatch-4"));
        assert!(state.is_working);
    }

    #[test]
    fn awaiting_background_stays_visible_without_foreground_dispatch() {
        let now = Utc::now();
        let mut probe_live = |_name: &str| false;
        let mut probe_ready = |_name: &str| false;
        let state = resolve_effective_state_with(
            &local_aliases(),
            Some("remote-host:AgentDesk-codex-adk-cdx"),
            Some("awaiting_bg"),
            None,
            None,
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "awaiting_bg");
        assert_eq!(state.active_dispatch_id, None);
        assert!(state.is_working);
    }

    #[test]
    fn provider_prefixed_local_ready_for_input_tmux_session_becomes_idle() {
        let now = Utc::now();
        let mut probe_live = |_name: &str| true;
        let mut probe_ready = |_name: &str| true;
        let state = resolve_effective_state_with(
            &["itismyfieldui-macmini".to_string()].into_iter().collect(),
            Some(
                "codex/discord_7a14995f62164ef1/itismyfieldui-Macmini:AgentDesk-codex-adk-dash-cdx",
            ),
            Some("working"),
            None,
            Some(
                &(now - Duration::seconds(5))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            now,
            &mut probe_live,
            &mut probe_ready,
        );

        assert_eq!(state.status, "idle");
        assert_eq!(state.active_dispatch_id, None);
        assert!(!state.is_working);
    }

    #[test]
    fn parse_session_key_supports_provider_prefixed_host() {
        let parsed = parse_session_key(
            "codex/discord_7a14995f62164ef1/itismyfieldui-Macmini:AgentDesk-codex-adk-dash-cdx",
        );

        assert_eq!(
            parsed,
            Some((
                "itismyfieldui-macmini".to_string(),
                "AgentDesk-codex-adk-dash-cdx".to_string(),
            ))
        );
    }
}
