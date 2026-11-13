use std::fmt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub(crate) struct SecretString(String);

impl SecretString {
    pub(super) fn new(value: String) -> Option<Self> {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return None;
        }
        crate::utils::redact::register_known_secret(&value);
        Some(Self(value))
    }
    pub(super) fn header(&self) -> Option<reqwest::header::HeaderValue> {
        let mut value = reqwest::header::HeaderValue::from_str(&self.0).ok()?;
        value.set_sensitive(true);
        Some(value)
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyLookupOutcome {
    On,
    NoKey,
    LookupTimeout,
    AuthFailed,
    Cancelled,
}
impl KeyLookupOutcome {
    pub(super) fn boot_label(self) -> &'static str {
        match self {
            Self::On => "On",
            Self::NoKey => "Off(no_key)",
            Self::LookupTimeout => "Off(lookup_timeout)",
            Self::AuthFailed => "Off(auth_failed)",
            Self::Cancelled => "Off(cancelled)",
        }
    }
}

pub(in crate::services::decision_gate) async fn load(
    captured: Option<SecretString>,
    command: &mut Command,
    cancel: &CancellationToken,
) -> (Option<SecretString>, KeyLookupOutcome) {
    if captured.is_some() {
        return (captured, KeyLookupOutcome::On);
    }
    command
        .args(["read", "op://AgentDesk/Gemini API/credential"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let Ok(mut child) = command.spawn() else {
        return (None, KeyLookupOutcome::NoKey);
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let mut bytes = Vec::new();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(KeyLookupOutcome::Cancelled),
        result = tokio::time::timeout_at(deadline, async {
            stdout.take(4097).read_to_end(&mut bytes).await?;
            child.wait().await
        }) => match result {
            Err(_) => Err(KeyLookupOutcome::LookupTimeout),
            Ok(Err(_)) => Err(KeyLookupOutcome::AuthFailed),
            Ok(Ok(status)) if !status.success() => Err(KeyLookupOutcome::AuthFailed),
            Ok(Ok(_)) => Ok(()),
        }
    };
    if let Err(outcome) = result {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return (None, outcome);
    }
    if bytes.len() > 4096 {
        return (None, KeyLookupOutcome::AuthFailed);
    }
    match String::from_utf8(bytes).ok().and_then(SecretString::new) {
        Some(key) => (Some(key), KeyLookupOutcome::On),
        None => (None, KeyLookupOutcome::NoKey),
    }
}

/// # Safety
/// Call only on the initial single-threaded binary path, before starting children.
pub(crate) unsafe fn capture_initial_key() -> Option<SecretString> {
    unsafe { initial_env::take() }.and_then(SecretString::new)
}

#[cfg(all(test, unix))]
#[path = "key_tests.rs"]
mod tests;

#[path = "key_initial_env.rs"]
mod initial_env;
