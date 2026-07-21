use std::collections::{HashMap, HashSet};
use std::process::Command;

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::db::session_status::{
    AWAITING_BG, AWAITING_USER, DISCONNECTED, IDLE, is_active_status, is_bg_wait_status,
    is_terminal_status, is_user_wait_status, normalize_session_status,
};
use crate::services::discord::session_identity::SessionIdentity;

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
                        crate::services::provider::tmux_session_fallback_ready_for_input(
                            tmux_name,
                            &provider,
                            runtime_kind,
                        )
                        .is_some_and(
                            crate::services::pane_readiness::FallbackPaneReadiness::is_ready,
                        )
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

    /// DB/heartbeat-only liveness resolution that NEVER spawns a tmux probe.
    ///
    /// Used by the read-only active-session audit on `/api/health/detail`, which
    /// is contractually an off-hot-path DB/evidence pass: it must not block the
    /// request on synchronous tmux (`has_live_pane` / pane-readiness) probes for
    /// local session keys. Every host (local or remote) is resolved via
    /// heartbeat recency only, so an unknown/stale heartbeat reads as not-live.
    pub fn resolve_db_only(
        &mut self,
        session_key: Option<&str>,
        raw_status: Option<&str>,
        active_dispatch_id: Option<&str>,
        last_heartbeat: Option<&str>,
    ) -> EffectiveSessionState {
        let now = Utc::now();
        // Empty alias set ⇒ no key is treated as local ⇒ the local-host branch
        // (the only tmux-probing branch) is never taken; heartbeat recency is the
        // sole liveness signal. The closures are unreachable but required by the
        // shared signature.
        let no_local_aliases: HashSet<String> = HashSet::new();
        let mut never_live = |_tmux_name: &str| false;
        let mut never_ready = |_tmux_name: &str| false;
        resolve_effective_state_with(
            &no_local_aliases,
            session_key,
            raw_status,
            active_dispatch_id,
            last_heartbeat,
            now,
            &mut never_live,
            &mut never_ready,
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

#[allow(clippy::too_many_arguments)]
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
    let identity = SessionIdentity::parse(session_key)?;
    let host = normalize_host(&identity.host)?;
    Some((host, identity.tmux_name))
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

    fn ts(secs_ago: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(secs_ago))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// `resolve_db_only` must NOT take the local-host (tmux-probing) branch even
    /// for a key whose host matches a local alias: it resolves via heartbeat
    /// recency only, so a recent heartbeat reads as working without any tmux probe.
    #[test]
    fn resolve_db_only_uses_heartbeat_not_tmux_for_local_keys() {
        let mut resolver = SessionActivityResolver::new();
        // Force a local alias so `resolve` (the non-db path) would tmux-probe.
        resolver.local_host_aliases = Some(HashSet::from(["localbox".to_string()]));

        let recent = resolver.resolve_db_only(
            Some("localbox:codex-chan-1"),
            Some("turn_active"),
            Some("dispatch-1"),
            Some(&ts(5)),
        );
        assert!(
            recent.is_working,
            "recent heartbeat ⇒ working via heartbeat path, no tmux probe"
        );

        let stale = resolver.resolve_db_only(
            Some("localbox:codex-chan-1"),
            Some("turn_active"),
            Some("dispatch-1"),
            Some(&ts(900)),
        );
        assert!(
            !stale.is_working,
            "stale heartbeat ⇒ not working (db-only verdict)"
        );

        // Crucially, no tmux probe cache entry was ever populated.
        assert!(resolver.tmux_live_cache.is_empty());
        assert!(resolver.tmux_ready_cache.is_empty());
    }

    /// An unknown heartbeat in db-only mode reads as not-live (cannot prove fresh).
    #[test]
    fn resolve_db_only_unknown_heartbeat_is_not_working() {
        let mut resolver = SessionActivityResolver::new();
        let state = resolver.resolve_db_only(
            Some("localbox:codex-chan-2"),
            Some("turn_active"),
            Some("dispatch-2"),
            None,
        );
        assert!(!state.is_working);
        assert!(resolver.tmux_live_cache.is_empty());
    }

    #[test]
    fn parse_session_key_accepts_namespaced_keys() {
        assert_eq!(
            parse_session_key("codex/hash123/LocalBox.local:AgentDesk-codex-adk-cdx"),
            Some((
                "localbox".to_string(),
                "AgentDesk-codex-adk-cdx".to_string()
            ))
        );
    }

    #[test]
    fn parse_session_key_preserves_legacy_host_alias_keys() {
        assert_eq!(
            parse_session_key("remote-host/LocalBox.local:codex-1234"),
            Some(("localbox".to_string(), "codex-1234".to_string()))
        );
    }
}
