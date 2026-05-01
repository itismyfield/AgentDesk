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

/// Read a bot token from the canonical runtime credential path.
/// Legacy `config/credential/` entries are migrated into `credential/` on read.
pub fn read_bot_token(name: &str) -> Option<String> {
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

    use std::sync::OnceLock;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    // A helper to safely manage environment variables in tests.
    struct EnvVarGuard<'a> {
        key: String,
        previous_value: Option<std::ffi::OsString>,
        _lock: MutexGuard<'a, ()>,
    }

    impl<'a> EnvVarGuard<'a> {
        fn set_path(key: &str, path: &Path) -> Self {
            let lock = ENV_MUTEX.get_or_init(|| Mutex::new(())).lock().unwrap();
            let previous_value = std::env::var_os(key);
            unsafe { std::env::set_var(key, path) };
            Self {
                key: key.to_string(),
                previous_value,
                _lock: lock,
            }
        }
    }

    impl<'a> Drop for EnvVarGuard<'a> {
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
