//! Opt-in destructive Discord controls for the live E2E harness (#4488).
//!
//! The HTTP route subtree is mounted only when `AGENTDESK_E2E_CONTROL=1` was
//! present when dcserver started, and every operation is restricted to channel
//! IDs captured from `AGENTDESK_E2E_CHANNEL_IDS` at boot. Failure injections are
//! scoped to one provider/channel/operation, consumed once, and expire quickly.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use super::health::HealthRegistry;
use crate::services::provider::ProviderKind;

const ENABLE_ENV: &str = "AGENTDESK_E2E_CONTROL";
const CHANNELS_ENV: &str = "AGENTDESK_E2E_CHANNEL_IDS";
const INJECTION_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscordFailureOperation {
    Send,
    Delete,
}

impl DiscordFailureOperation {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "send" => Some(Self::Send),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InjectionState {
    remaining: u32,
    expires_at_unix: i64,
}

static INJECTION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(not(test))]
static ENABLED_AT_BOOT: OnceLock<bool> = OnceLock::new();
#[cfg(not(test))]
static ALLOWED_CHANNELS_AT_BOOT: OnceLock<HashSet<u64>> = OnceLock::new();
#[cfg(test)]
static TEST_ENABLED_OVERRIDE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);
#[cfg(test)]
static TEST_ALLOWED_CHANNELS_OVERRIDE: LazyLock<Mutex<Option<HashSet<u64>>>> =
    LazyLock::new(|| Mutex::new(None));

fn enabled_from_env() -> bool {
    std::env::var(ENABLE_ENV)
        .ok()
        .is_some_and(|value| value.trim() == "1")
}

fn allowed_channels_from_env() -> HashSet<u64> {
    std::env::var(CHANNELS_ENV)
        .unwrap_or_default()
        .split(',')
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .filter(|channel_id| *channel_id > 0)
        .collect()
}

pub(crate) fn enabled() -> bool {
    #[cfg(not(test))]
    {
        *ENABLED_AT_BOOT.get_or_init(enabled_from_env)
    }
    #[cfg(test)]
    {
        match TEST_ENABLED_OVERRIDE.load(std::sync::atomic::Ordering::Acquire) {
            0 => false,
            1 => true,
            _ => enabled_from_env(),
        }
    }
}

fn allowed_channels() -> HashSet<u64> {
    #[cfg(not(test))]
    {
        ALLOWED_CHANNELS_AT_BOOT
            .get_or_init(allowed_channels_from_env)
            .clone()
    }
    #[cfg(test)]
    {
        TEST_ALLOWED_CHANNELS_OVERRIDE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap_or_else(allowed_channels_from_env)
    }
}

pub(crate) fn channel_is_allowed(channel_id: u64) -> bool {
    enabled() && allowed_channels().contains(&channel_id)
}

fn injection_root() -> Option<PathBuf> {
    super::runtime_store::runtime_root().map(|root| root.join("e2e_discord_failures"))
}

fn injection_path(
    provider: &ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
) -> Option<PathBuf> {
    let operation = match operation {
        DiscordFailureOperation::Send => "send",
        DiscordFailureOperation::Delete => "delete",
    };
    injection_root().map(|root| {
        root.join(provider.as_str())
            .join(channel_id.to_string())
            .join(format!("{operation}.json"))
    })
}

fn remove_empty_ancestors(path: &Path, root: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        match std::fs::remove_dir(directory) {
            Ok(()) => current = directory.parent(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = directory.parent();
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_dir(root);
}

fn remove_injection_file(path: &Path, root: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            remove_empty_ancestors(path, root);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "failed to remove E2E Discord failure injection");
            false
        }
    }
}

fn sweep_expired_locked(root: &Path, now: i64) -> usize {
    let mut removed = 0;
    let Ok(providers) = std::fs::read_dir(root) else {
        return 0;
    };
    for provider in providers.flatten().filter(|entry| entry.path().is_dir()) {
        let Ok(channels) = std::fs::read_dir(provider.path()) else {
            continue;
        };
        for channel in channels.flatten().filter(|entry| entry.path().is_dir()) {
            let Ok(files) = std::fs::read_dir(channel.path()) else {
                continue;
            };
            for file in files.flatten().filter(|entry| entry.path().is_file()) {
                let path = file.path();
                let expired = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<InjectionState>(&raw).ok())
                    .is_none_or(|state| state.expires_at_unix <= now || state.remaining == 0);
                if expired && remove_injection_file(&path, root) {
                    removed += 1;
                }
            }
        }
    }
    removed
}

pub(crate) fn sweep_expired() -> usize {
    if !enabled() {
        return 0;
    }
    let Some(root) = injection_root() else {
        return 0;
    };
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    sweep_expired_locked(&root, chrono::Utc::now().timestamp())
}

pub(crate) fn arm_failure(
    provider: ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
    count: u32,
) -> Result<(), &'static str> {
    if !enabled() {
        return Err("E2E Discord controls are disabled");
    }
    if !channel_is_allowed(channel_id) {
        return Err("channel is not in AGENTDESK_E2E_CHANNEL_IDS");
    }
    if count == 0 || count > 10 {
        return Err("count must be between 1 and 10");
    }
    let root = injection_root().ok_or("AgentDesk runtime root is unavailable")?;
    let path = injection_path(&provider, channel_id, operation)
        .ok_or("AgentDesk runtime root is unavailable")?;
    let payload = serde_json::to_string(&InjectionState {
        remaining: count,
        expires_at_unix: chrono::Utc::now().timestamp()
            + i64::try_from(INJECTION_TTL.as_secs()).unwrap_or(300),
    })
    .map_err(|_| "failed to encode injection state")?;
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    sweep_expired_locked(&root, chrono::Utc::now().timestamp());
    super::runtime_store::atomic_write(&path, &payload)
        .map_err(|_| "failed to persist injection state")
}

pub(crate) fn clear_failure(
    provider: &ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
) -> bool {
    if !channel_is_allowed(channel_id) {
        return false;
    }
    let Some(root) = injection_root() else {
        return false;
    };
    let Some(path) = injection_path(provider, channel_id, operation) else {
        return false;
    };
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    remove_injection_file(&path, &root)
}

fn consume_failure(
    provider: &ProviderKind,
    channel_id: ChannelId,
    operation: DiscordFailureOperation,
) -> bool {
    if !channel_is_allowed(channel_id.get()) {
        return false;
    }
    let Some(root) = injection_root() else {
        return false;
    };
    let Some(path) = injection_path(provider, channel_id.get(), operation) else {
        return false;
    };
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    sweep_expired_locked(&root, chrono::Utc::now().timestamp());
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut state) = serde_json::from_str::<InjectionState>(&raw) else {
        remove_injection_file(&path, &root);
        return false;
    };
    if state.expires_at_unix <= chrono::Utc::now().timestamp() || state.remaining == 0 {
        remove_injection_file(&path, &root);
        return false;
    }
    state.remaining -= 1;
    if state.remaining == 0 {
        remove_injection_file(&path, &root);
    } else if let Ok(payload) = serde_json::to_string(&state)
        && super::runtime_store::atomic_write(&path, &payload).is_err()
    {
        remove_injection_file(&path, &root);
    }
    true
}

pub(in crate::services::discord) fn consume_send_failure(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    consume_failure(provider, channel_id, DiscordFailureOperation::Send)
}

pub(in crate::services::discord) fn consume_delete_failure(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    consume_failure(provider, channel_id, DiscordFailureOperation::Delete)
}

pub(crate) async fn delete_message(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<(), String> {
    if !channel_is_allowed(channel_id.get()) {
        return Err("channel is not in AGENTDESK_E2E_CHANNEL_IDS".to_string());
    }
    let http = super::health::resolve_bot_http(registry, provider.as_str())
        .await
        .map_err(|(_, body)| body)?;
    super::http::delete_channel_message(&http, channel_id, message_id)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    if let Some(root) = injection_root() {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
pub(crate) struct TestControlGuard {
    previous_enabled: i8,
    previous_channels: Option<HashSet<u64>>,
}

#[cfg(test)]
impl TestControlGuard {
    pub(crate) fn set(enabled: bool, channel_ids: impl IntoIterator<Item = u64>) -> Self {
        let previous_enabled = TEST_ENABLED_OVERRIDE.swap(
            if enabled { 1 } else { 0 },
            std::sync::atomic::Ordering::AcqRel,
        );
        let mut override_channels = TEST_ALLOWED_CHANNELS_OVERRIDE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_channels = override_channels.replace(channel_ids.into_iter().collect());
        Self {
            previous_enabled,
            previous_channels,
        }
    }
}

#[cfg(test)]
impl Drop for TestControlGuard {
    fn drop(&mut self) {
        TEST_ENABLED_OVERRIDE.store(self.previous_enabled, std::sync::atomic::Ordering::Release);
        *TEST_ALLOWED_CHANNELS_OVERRIDE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.previous_channels.take();
    }
}

#[cfg(test)]
pub(crate) fn expire_for_tests(
    provider: &ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
) {
    let Some(path) = injection_path(provider, channel_id, operation) else {
        return;
    };
    let payload = serde_json::to_string(&InjectionState {
        remaining: 1,
        expires_at_unix: chrono::Utc::now().timestamp() - 1,
    })
    .expect("serialize expired injection");
    super::runtime_store::atomic_write(&path, &payload).expect("persist expired injection");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate_runtime_root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
        let root = tempfile::tempdir().expect("runtime root");
        let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        (root, guard)
    }

    #[test]
    fn disabled_surface_cannot_arm_or_consume() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _root_guard) = isolate_runtime_root();
        let _control = TestControlGuard::set(false, [44]);
        reset_for_tests();

        assert_eq!(
            arm_failure(ProviderKind::Claude, 44, DiscordFailureOperation::Send, 1,),
            Err("E2E Discord controls are disabled")
        );
        assert!(!consume_send_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));
    }

    #[test]
    fn allowlist_rejects_unconfigured_channels() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _root_guard) = isolate_runtime_root();
        let _control = TestControlGuard::set(true, [44]);
        reset_for_tests();

        assert!(channel_is_allowed(44));
        assert!(!channel_is_allowed(45));
        assert_eq!(
            arm_failure(ProviderKind::Claude, 45, DiscordFailureOperation::Send, 1),
            Err("channel is not in AGENTDESK_E2E_CHANNEL_IDS")
        );
        assert!(
            injection_path(&ProviderKind::Claude, 45, DiscordFailureOperation::Send)
                .is_some_and(|path| !path.exists())
        );
    }

    #[test]
    fn injection_is_scoped_consumed_and_clearable() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _root_guard) = isolate_runtime_root();
        let _control = TestControlGuard::set(true, [44]);
        reset_for_tests();

        arm_failure(ProviderKind::Claude, 44, DiscordFailureOperation::Send, 2).unwrap();
        assert!(
            injection_path(&ProviderKind::Claude, 44, DiscordFailureOperation::Send)
                .is_some_and(|path| path.exists()),
            "the injection must survive a dcserver restart in the runtime store"
        );
        assert!(!consume_send_failure(
            &ProviderKind::Codex,
            ChannelId::new(44)
        ));
        assert!(!consume_send_failure(
            &ProviderKind::Claude,
            ChannelId::new(45)
        ));
        assert!(consume_send_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));
        assert!(consume_send_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));
        assert!(!consume_send_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));

        arm_failure(ProviderKind::Claude, 44, DiscordFailureOperation::Delete, 1).unwrap();
        assert!(clear_failure(
            &ProviderKind::Claude,
            44,
            DiscordFailureOperation::Delete,
        ));
        assert!(!consume_delete_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));

        arm_failure(ProviderKind::Claude, 44, DiscordFailureOperation::Delete, 1).unwrap();
        expire_for_tests(&ProviderKind::Claude, 44, DiscordFailureOperation::Delete);
        let expired_path =
            injection_path(&ProviderKind::Claude, 44, DiscordFailureOperation::Delete).unwrap();
        assert!(!consume_delete_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));
        assert!(!expired_path.exists());
    }

    #[test]
    fn arm_sweeps_expired_files_and_empty_directories() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _root_guard) = isolate_runtime_root();
        let _control = TestControlGuard::set(true, [44, 45]);
        reset_for_tests();

        expire_for_tests(&ProviderKind::Claude, 44, DiscordFailureOperation::Delete);
        let expired_path =
            injection_path(&ProviderKind::Claude, 44, DiscordFailureOperation::Delete).unwrap();
        let expired_channel_dir = expired_path.parent().unwrap().to_path_buf();
        assert!(expired_path.exists());

        arm_failure(ProviderKind::Claude, 45, DiscordFailureOperation::Send, 1).unwrap();
        assert!(!expired_path.exists());
        assert!(!expired_channel_dir.exists());
        assert_eq!(sweep_expired(), 0);
    }
}
