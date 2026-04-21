use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::InflightRestartMode;
use super::runtime_store::{atomic_write, discord_inflight_root};
use crate::services::provider::ProviderKind;

const INFLIGHT_STATE_VERSION: u32 = 4;
const INFLIGHT_MAX_AGE_SECS: u64 = 300; // 5 minutes
const DRAIN_RESTART_MAX_AGE_SECS: u64 = 1800; // 30 minutes
const HOT_SWAP_HANDOFF_MAX_AGE_SECS: u64 = 900; // 15 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InflightTurnState {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    #[serde(default)]
    pub logical_channel_id: Option<u64>,
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub thread_title: Option<String>,
    pub request_owner_user_id: u64,
    pub user_msg_id: u64,
    pub current_msg_id: u64,
    pub current_msg_len: usize,
    pub user_text: String,
    pub session_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    pub last_offset: u64,
    /// Stable start offset for the current turn's output JSONL slice.
    #[serde(default)]
    pub turn_start_offset: Option<u64>,
    pub full_response: String,
    pub response_sent_offset: usize,
    #[serde(default)]
    pub current_tool_line: Option<String>,
    #[serde(default)]
    pub prev_tool_status: Option<String>,
    pub started_at: String,
    pub updated_at: String,
    /// Restart generation at which this turn was born.
    #[serde(default)]
    pub born_generation: u64,
    /// Whether any tool_use was seen during this turn (persisted for restart recovery).
    #[serde(default)]
    pub any_tool_used: bool,
    /// Whether text was streamed after the last tool_use (persisted for restart recovery).
    #[serde(default)]
    pub has_post_tool_text: bool,
    /// ADK session key (hostname:session-name) for long-turn diagnostics.
    #[serde(default)]
    pub session_key: Option<String>,
    /// Active dispatch ID for long-turn diagnostics.
    #[serde(default)]
    pub dispatch_id: Option<String>,
    /// Last tmux output offset from which a watcher relayed a response.
    /// Persisted so that replacement watcher instances can skip already-delivered output.
    #[serde(default)]
    pub last_watcher_relayed_offset: Option<u64>,
    /// Lifecycle-aware restart/handoff mode for recovery semantics.
    #[serde(default)]
    pub restart_mode: Option<InflightRestartMode>,
    /// Generation that owns the planned restart/handoff lifecycle.
    #[serde(default)]
    pub restart_generation: Option<u64>,
    /// #897 counter-model re-review — `true` when this inflight was
    /// synthesised by `POST /api/inflight/rebind` to adopt a live tmux
    /// session that had no real user-authored turn driving it (zero-valued
    /// `user_msg_id` / `current_msg_id` / `request_owner_user_id`).
    ///
    /// Callers that route based on "is there a live foreground turn" must
    /// treat a rebind-origin inflight as **absent** — otherwise the
    /// background-trigger notify-bot predicate in
    /// `should_route_terminal_response_via_notify_bot` sees a
    /// non-rebind_origin inflight, routes the recovered auto-trigger
    /// response back through the command bot, and reintroduces the
    /// loop-hazard that #826 was fixing. Reactions / transcript writes
    /// that key off `user_msg_id` should also skip work when this flag is
    /// set, because the placeholder IDs do not identify a real Discord
    /// message.
    #[serde(default)]
    pub rebind_origin: bool,
}

impl InflightTurnState {
    pub fn new(
        provider: ProviderKind,
        channel_id: u64,
        channel_name: Option<String>,
        request_owner_user_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        user_text: String,
        session_id: Option<String>,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        input_fifo_path: Option<String>,
        last_offset: u64,
    ) -> Self {
        let now = now_string();
        Self {
            version: INFLIGHT_STATE_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            channel_name,
            logical_channel_id: Some(channel_id),
            thread_id: None,
            thread_title: None,
            request_owner_user_id,
            user_msg_id,
            current_msg_id,
            current_msg_len: 0,
            user_text,
            session_id,
            tmux_session_name,
            output_path,
            input_fifo_path,
            last_offset,
            turn_start_offset: Some(last_offset),
            full_response: String::new(),
            response_sent_offset: 0,
            current_tool_line: None,
            prev_tool_status: None,
            started_at: now.clone(),
            updated_at: now,
            born_generation: super::runtime_store::load_generation(),
            any_tool_used: false,
            has_post_tool_text: false,
            session_key: None,
            dispatch_id: None,
            last_watcher_relayed_offset: None,
            restart_mode: None,
            restart_generation: None,
            rebind_origin: false,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }

    pub fn set_restart_mode(&mut self, restart_mode: InflightRestartMode) {
        self.restart_mode = Some(restart_mode);
        self.restart_generation = Some(super::runtime_store::load_generation());
    }

    pub fn clear_restart_mode(&mut self) {
        self.restart_mode = None;
        self.restart_generation = None;
    }
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn inflight_state_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(super) fn save_inflight_state(state: &InflightTurnState) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

/// #897 counter-model review P2 #1 — atomic "create, don't overwrite"
/// variant of `save_inflight_state`. Used by `POST /api/inflight/rebind` so a
/// concurrent legitimate turn that wins the mailbox race between the rebind
/// handler's existence check and its write cannot have its canonical
/// inflight file silently overwritten by the synthetic rebind state
/// (`user_msg_id=0`, placeholder ids zeroed). Returns `InflightAlreadyExists`
/// when the target path is already occupied — the handler translates that
/// into HTTP 409 and the operator retries (or leaves it to the live turn).
#[derive(Debug)]
pub(super) enum CreateNewInflightError {
    /// A state file already exists at the target path — another path wrote
    /// it between the caller's preflight check and this call.
    AlreadyExists,
    /// Filesystem or serialization failure.
    Internal(String),
}

impl std::fmt::Display for CreateNewInflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "inflight state already exists"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

pub(super) fn save_inflight_state_create_new(
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(root) = inflight_runtime_root() else {
        return Err(CreateNewInflightError::Internal(
            "Home directory not found".to_string(),
        ));
    };
    save_inflight_state_create_new_in_root(&root, state)
}

/// Test-visible inner form of `save_inflight_state_create_new`. Takes an
/// explicit root so unit tests can exercise the O_CREAT|O_EXCL semantics
/// without tripping over `AGENTDESK_ROOT_DIR` env-var races.
fn save_inflight_state_create_new_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(provider) = state.provider_kind() else {
        return Err(CreateNewInflightError::Internal(format!(
            "Unknown provider '{}'",
            state.provider
        )));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
    }
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated)
        .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;

    // `OpenOptions::create_new(true)` is the canonical atomic check-and-
    // create primitive on POSIX (O_CREAT | O_EXCL). No reliance on a
    // preceding `load_inflight_state` — the kernel itself serializes this.
    use std::io::Write;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(json.as_bytes())
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            file.sync_all()
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateNewInflightError::AlreadyExists)
        }
        Err(e) => Err(CreateNewInflightError::Internal(e.to_string())),
    }
}

fn save_inflight_state_in_root(root: &Path, state: &InflightTurnState) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(super) fn clear_inflight_state(provider: &ProviderKind, channel_id: u64) {
    let Some(root) = inflight_runtime_root() else {
        return;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(super) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };

    let provider_dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(&provider_dir) else {
        return false;
    };

    let mut cleared = false;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.tmux_session_name.as_deref() != Some(tmux_name) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            cleared = true;
        }
    }

    cleared
}

pub(super) fn mark_all_inflight_states_restart_mode(
    provider: &ProviderKind,
    restart_mode: InflightRestartMode,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    let states = load_inflight_states_from_root(&root, provider);
    let mut updated = 0usize;
    for mut state in states {
        state.set_restart_mode(restart_mode);
        if save_inflight_state_in_root(&root, &state).is_ok() {
            updated += 1;
        }
    }
    updated
}

/// Load a single inflight state by provider + channel_id (returns None if missing).
pub(super) fn load_inflight_state(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<InflightTurnState> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub(super) fn load_inflight_states(provider: &ProviderKind) -> Vec<InflightTurnState> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    load_inflight_states_from_root(&root, provider)
}

pub(crate) fn latest_request_owner_user_id_for_channel(channel_id: u64) -> Option<u64> {
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ];

    providers
        .iter()
        .flat_map(load_inflight_states)
        .filter(|state| state.channel_id == channel_id)
        .max_by(|left, right| left.updated_at.cmp(&right.updated_at))
        .map(|state| state.request_owner_user_id)
}

fn planned_restart_retention_secs(restart_mode: InflightRestartMode) -> u64 {
    match restart_mode {
        InflightRestartMode::DrainRestart => DRAIN_RESTART_MAX_AGE_SECS,
        InflightRestartMode::HotSwapHandoff => HOT_SWAP_HANDOFF_MAX_AGE_SECS,
    }
}

fn stale_removal_reason(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> Option<String> {
    match state.restart_mode {
        Some(restart_mode) => {
            if state.restart_generation != Some(current_generation) {
                return Some(format!(
                    "removing {} inflight state from old generation {:?} (current generation {})",
                    restart_mode.label(),
                    state.restart_generation,
                    current_generation
                ));
            }
            let max_age = planned_restart_retention_secs(restart_mode);
            if age_secs > max_age {
                return Some(format!(
                    "removing stale {} inflight state file ({age_secs}s old > {max_age}s)",
                    restart_mode.label()
                ));
            }
            None
        }
        None => {
            if age_secs > INFLIGHT_MAX_AGE_SECS {
                Some(format!(
                    "removing stale inflight state file ({age_secs}s old > {INFLIGHT_MAX_AGE_SECS}s)"
                ))
            } else {
                None
            }
        }
    }
}

fn load_inflight_states_from_root(root: &Path, provider: &ProviderKind) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut states = Vec::new();
    let current_generation = super::runtime_store::load_generation();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing malformed inflight state file: {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing inflight state with provider mismatch: {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        }
        if let Ok(meta) = fs::metadata(&path)
            && let Ok(modified) = meta.modified()
            && let Ok(age) = modified.elapsed()
            && let Some(reason) = stale_removal_reason(&state, age.as_secs(), current_generation)
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ⚠ {}: {}", reason, path.display());
            let _ = fs::remove_file(&path);
            continue;
        }
        states.push(state);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::{
        CreateNewInflightError, InflightTurnState, latest_request_owner_user_id_for_channel,
        load_inflight_states, load_inflight_states_from_root,
        mark_all_inflight_states_restart_mode, save_inflight_state_create_new_in_root,
        save_inflight_state_in_root, stale_removal_reason,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_inflight_state() {
        let temp = TempDir::new().unwrap();

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 123);
        assert_eq!(loaded[0].current_msg_id, 999);
        assert_eq!(loaded[0].last_offset, 42);
        assert_eq!(loaded[0].turn_start_offset, Some(42));
    }

    #[test]
    fn planned_restart_state_uses_generation_aware_retention() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        assert!(
            stale_removal_reason(&state, 600, 7).is_none(),
            "current-generation planned restart should survive the normal 300s cleanup window"
        );
        assert!(
            stale_removal_reason(&state, 10, 8)
                .expect("old generation planned restart should be removed")
                .contains("old generation")
        );
    }

    #[test]
    fn latest_request_owner_user_id_prefers_most_recent_state_across_providers() {
        let temp = TempDir::new().unwrap();
        let inflight_root = temp.path().join("runtime").join("discord_inflight");

        let mut claude_state = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            111,
            789,
            999,
            "hello".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        claude_state.updated_at = "2026-04-11 00:00:00".to_string();
        save_inflight_state_in_root(&inflight_root, &claude_state).unwrap();

        let mut codex_state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            222,
            790,
            1000,
            "world".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        codex_state.updated_at = "2026-04-11 00:00:05".to_string();
        save_inflight_state_in_root(&inflight_root, &codex_state).unwrap();

        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let owner = latest_request_owner_user_id_for_channel(123);
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }

        assert_eq!(owner, Some(222));
    }

    #[test]
    fn mark_all_inflight_states_restart_mode_marks_saved_states() {
        let _lock = super::super::runtime_store::lock_test_env();
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        struct EnvReset;
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        let _reset = EnvReset;

        let inflight_root = root.join("runtime").join("discord_inflight");
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(&inflight_root, &state).unwrap();

        assert_eq!(
            mark_all_inflight_states_restart_mode(
                &ProviderKind::Codex,
                InflightRestartMode::DrainRestart,
            ),
            1
        );

        let states = load_inflight_states(&ProviderKind::Codex);
        assert_eq!(states.len(), 1);
        assert_eq!(
            states[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(
            states[0].restart_generation,
            Some(super::super::runtime_store::load_generation())
        );
    }

    /// #897 P2 #1: `save_inflight_state_create_new_in_root` must succeed on
    /// a vacant path (atomic create) and reject a second call at the same
    /// path with `AlreadyExists` — this is the guarantee that prevents a
    /// `/api/inflight/rebind` call from overwriting a concurrent live
    /// turn's canonical inflight state.
    #[test]
    fn save_inflight_state_create_new_rejects_existing_path() {
        let temp = TempDir::new().unwrap();
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            1_234_567,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        save_inflight_state_create_new_in_root(temp.path(), &state)
            .expect("first atomic create must succeed on a vacant path");

        match save_inflight_state_create_new_in_root(temp.path(), &state) {
            Err(CreateNewInflightError::AlreadyExists) => {}
            other => panic!(
                "second atomic create must report AlreadyExists, got {:?}",
                other
            ),
        }
    }

    /// #897 P2 #1: a previously-saved `save_inflight_state_in_root` write
    /// must be observed by `save_inflight_state_create_new_in_root` as
    /// `AlreadyExists`. This is the actual race we need to guard against —
    /// a legitimate turn writes its state via `save_inflight_state`, then a
    /// concurrent rebind call must NOT overwrite it.
    #[test]
    fn save_inflight_state_create_new_rejects_path_written_by_normal_save() {
        let temp = TempDir::new().unwrap();
        let live_turn_state = InflightTurnState::new(
            ProviderKind::Codex,
            9_876_543,
            Some("adk-cdx".to_string()),
            123, // live user
            456, // real user_msg_id
            789, // real current_msg_id
            "real user input".to_string(),
            Some("session-live".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        save_inflight_state_in_root(temp.path(), &live_turn_state)
            .expect("legitimate turn write must succeed");

        let rebind_state = InflightTurnState::new(
            ProviderKind::Codex,
            9_876_543,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        match save_inflight_state_create_new_in_root(temp.path(), &rebind_state) {
            Err(CreateNewInflightError::AlreadyExists) => {}
            other => panic!("rebind must not overwrite live turn state; got {:?}", other),
        }

        // Canonical live-turn data must survive.
        let states = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].request_owner_user_id, 123);
        assert_eq!(states[0].user_msg_id, 456);
        assert_eq!(states[0].user_text, "real user input");
    }
}
