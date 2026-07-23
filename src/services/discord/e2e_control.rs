//! Opt-in destructive Discord controls for the live E2E harness (#4488).
//!
//! The HTTP routes that call this module are mounted in every build but remain
//! fail-closed unless `AGENTDESK_E2E_CONTROL=1` was present when dcserver
//! started. Failure injections are scoped to one provider/channel/operation,
//! consumed once, and expire quickly so they cannot leak into a later scenario.

use std::path::PathBuf;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use super::health::HealthRegistry;
use crate::services::provider::ProviderKind;

const ENABLE_ENV: &str = "AGENTDESK_E2E_CONTROL";
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
#[cfg(test)]
static TEST_ENABLED_OVERRIDE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

fn enabled_from_env() -> bool {
    std::env::var(ENABLE_ENV)
        .ok()
        .is_some_and(|value| value.trim() == "1")
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

fn injection_path(
    provider: &ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
) -> Option<PathBuf> {
    let operation = match operation {
        DiscordFailureOperation::Send => "send",
        DiscordFailureOperation::Delete => "delete",
    };
    super::runtime_store::runtime_root().map(|root| {
        root.join("e2e_discord_failures")
            .join(provider.as_str())
            .join(channel_id.to_string())
            .join(format!("{operation}.json"))
    })
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
    if count == 0 || count > 10 {
        return Err("count must be between 1 and 10");
    }
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
    super::runtime_store::atomic_write(&path, &payload)
        .map_err(|_| "failed to persist injection state")
}

pub(crate) fn clear_failure(
    provider: &ProviderKind,
    channel_id: u64,
    operation: DiscordFailureOperation,
) -> bool {
    let Some(path) = injection_path(provider, channel_id, operation) else {
        return false;
    };
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "failed to clear E2E Discord failure injection");
            false
        }
    }
}

fn consume_failure(
    provider: &ProviderKind,
    channel_id: ChannelId,
    operation: DiscordFailureOperation,
) -> bool {
    if !enabled() {
        return false;
    }
    let Some(path) = injection_path(provider, channel_id.get(), operation) else {
        return false;
    };
    let _lock = INJECTION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut state) = serde_json::from_str::<InjectionState>(&raw) else {
        let _ = std::fs::remove_file(&path);
        return false;
    };
    if state.expires_at_unix <= chrono::Utc::now().timestamp() || state.remaining == 0 {
        let _ = std::fs::remove_file(&path);
        return false;
    }
    state.remaining -= 1;
    if state.remaining == 0 {
        let _ = std::fs::remove_file(&path);
    } else if let Ok(payload) = serde_json::to_string(&state)
        && super::runtime_store::atomic_write(&path, &payload).is_err()
    {
        let _ = std::fs::remove_file(&path);
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
    if !enabled() {
        return Err("E2E Discord controls are disabled".to_string());
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
    for operation in [
        DiscordFailureOperation::Send,
        DiscordFailureOperation::Delete,
    ] {
        if let Some(path) = injection_path(&ProviderKind::Claude, 44, operation) {
            let _ = std::fs::remove_file(path);
        }
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

    struct EnabledOverrideGuard(i8);

    impl EnabledOverrideGuard {
        fn set(enabled: bool) -> Self {
            let previous = TEST_ENABLED_OVERRIDE.swap(
                if enabled { 1 } else { 0 },
                std::sync::atomic::Ordering::AcqRel,
            );
            Self(previous)
        }
    }

    impl Drop for EnabledOverrideGuard {
        fn drop(&mut self) {
            TEST_ENABLED_OVERRIDE.store(self.0, std::sync::atomic::Ordering::Release);
        }
    }

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
        let _guard = EnabledOverrideGuard::set(false);
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
    fn injection_is_scoped_consumed_and_clearable() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _root_guard) = isolate_runtime_root();
        let _guard = EnabledOverrideGuard::set(true);
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
        assert!(!consume_delete_failure(
            &ProviderKind::Claude,
            ChannelId::new(44)
        ));
    }
}
