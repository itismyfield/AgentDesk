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
            let ready =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)
                    .map(|(provider, _)| {
                        let binding =
                            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                                tmux_name,
                            );
                        let runtime_kind = binding
                            .as_ref()
                            .map(|binding| binding.runtime_kind)
                            .or_else(|| {
                                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
                                    tmux_name,
                                )
                            });
                        if let Some(ready) = binding
                            .as_ref()
                            .and_then(|binding| {
                                crate::services::tui_turn_state::runtime_binding_ready_for_input(
                                    &provider, binding, true,
                                )
                            })
                            .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
                        {
                            return ready;
                        }
                        if crate::services::tui_turn_state::pane_ready_fallback_allowed(
                            &provider,
                            runtime_kind,
                        ) {
                            crate::services::provider::tmux_session_ready_for_input(
                                tmux_name, &provider,
                            )
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
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
