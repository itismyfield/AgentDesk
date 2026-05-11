use std::path::{Path, PathBuf};

fn agentdesk_root() -> Option<PathBuf> {
    crate::config::runtime_root()
}

fn read_trimmed_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Validate a bot name as a safe path segment.
///
/// Issue #2047 Finding 8 — `name` is concatenated into a filesystem path inside
/// `runtime_layout::credential_token_path`, so an attacker (or buggy caller)
/// that smuggles `..`, `/`, NUL, or other separators could traverse outside
/// the credential directory and trick the loader into reading
/// (and later sending as `Bot <secret>`) an arbitrary file like
/// `../auth_token` or `../../etc/passwd`.
///
/// Restrict the alphabet to `[A-Za-z0-9_-]` and the length to 1..=32 so the
/// resulting path is always a child of `credential/`. Whitespace, empty
/// strings, and path separators are rejected.
pub fn is_valid_bot_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Read a bot token from the canonical runtime credential path.
/// Legacy `config/credential/` entries are migrated into `credential/` on read.
///
/// Returns `None` (without touching disk) when `name` fails
/// [`is_valid_bot_name`] — see Issue #2047 Finding 8.
pub fn read_bot_token(name: &str) -> Option<String> {
    if !is_valid_bot_name(name) {
        tracing::warn!(
            bot_name = %name,
            "rejecting read_bot_token call with non-conforming bot name (Issue #2047 Finding 8)"
        );
        return None;
    }
    let root = agentdesk_root()?;
    let _ = crate::runtime_layout::ensure_credential_layout(&root);
    let path = crate::runtime_layout::credential_token_path(&root, name);
    read_trimmed_token(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use std::sync::MutexGuard;

    // A helper to safely manage environment variables in tests.
    struct EnvVarGuard {
        key: String,
        previous_value: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set_path(key: &str, path: &Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous_value = std::env::var_os(key);
            unsafe { std::env::set_var(key, path) };
            Self {
                key: key.to_string(),
                previous_value,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous_value {
                Some(val) => unsafe { std::env::set_var(&self.key, val) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
    }

    #[test]
    fn test_read_bot_token_success() {
        let temp = TempDir::new().unwrap();
        let _guard = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let root = temp.path();
        let _ = crate::runtime_layout::ensure_credential_layout(root);
        let path = crate::runtime_layout::credential_token_path(root, "my_bot");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "  my_secret_token  ").unwrap();

        assert_eq!(
            read_bot_token("my_bot"),
            Some("my_secret_token".to_string())
        );
    }

    #[test]
    fn test_read_bot_token_not_found() {
        let temp = TempDir::new().unwrap();
        let _guard = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let root = temp.path();
        let _ = crate::runtime_layout::ensure_credential_layout(root);

        assert_eq!(read_bot_token("missing_bot"), None);
    }

    #[test]
    fn is_valid_bot_name_accepts_canonical_names() {
        assert!(is_valid_bot_name("announce"));
        assert!(is_valid_bot_name("notify"));
        assert!(is_valid_bot_name("agent_alpha-7"));
        assert!(is_valid_bot_name("A"));
        assert!(is_valid_bot_name(&"a".repeat(32)));
    }

    #[test]
    fn is_valid_bot_name_rejects_traversal_attempts() {
        // Issue #2047 Finding 8 — none of these should be allowed to reach the
        // filesystem layer.
        assert!(!is_valid_bot_name(""));
        assert!(!is_valid_bot_name(".."));
        assert!(!is_valid_bot_name("../auth_token"));
        assert!(!is_valid_bot_name("nested/notify"));
        assert!(!is_valid_bot_name("notify\\windows"));
        assert!(!is_valid_bot_name("notify\0"));
        assert!(!is_valid_bot_name("notify.token"));
        assert!(!is_valid_bot_name(" announce"));
        assert!(!is_valid_bot_name("announce "));
        assert!(!is_valid_bot_name(&"a".repeat(33)));
    }

    #[test]
    fn read_bot_token_rejects_traversal_without_touching_disk() {
        let temp = TempDir::new().unwrap();
        let _guard = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        // Even if the caller controls AGENTDESK_ROOT, an invalid name must be
        // refused before the path join happens.
        assert_eq!(read_bot_token("../auth_token"), None);
        assert_eq!(read_bot_token(""), None);
        assert_eq!(read_bot_token("nested/notify"), None);
    }

    #[test]
    fn test_read_bot_token_empty_or_whitespace() {
        let temp = TempDir::new().unwrap();
        let _guard = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let root = temp.path();
        let _ = crate::runtime_layout::ensure_credential_layout(root);
        let path = crate::runtime_layout::credential_token_path(root, "empty_bot");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "   \n \t ").unwrap();

        assert_eq!(read_bot_token("empty_bot"), None);
    }
}
