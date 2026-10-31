use std::collections::HashSet;
use std::env::VarError;
use std::sync::{LazyLock, Mutex};

/// Read a boolean env override, falling back on empty, missing or invalid values.
pub fn env_flag(name: &str, default: bool) -> bool {
    #[cfg(test)]
    if tests::parser_bypassed() {
        return default;
    }
    match std::env::var(name) {
        Ok(value) => parse_value(name, &value, default),
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(_)) => {
            warn_invalid(name);
            default
        }
    }
}

fn parse_value(name: &str, value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        "" => default,
        _ => {
            warn_invalid(name);
            default
        }
    }
}

fn warn_invalid(name: &str) {
    static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    let first = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(name.to_owned());
    if first {
        tracing::warn!(
            variable = name,
            "invalid boolean environment variable; using default"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::Arc;

    pub(super) fn parser_bypassed() -> bool {
        std::env::var("ADK_ENV_FLAG_MUTANT").as_deref() == Ok("default_only")
    }

    #[test]
    fn env_flag_contract_table() {
        const NAME: &str = "ADK_ENV_FLAG_TABLE_TEST";
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _restore = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(NAME);
        for default in [false, true] {
            unsafe { std::env::remove_var(NAME) };
            assert_eq!(env_flag(NAME, default), default);
            for value in ["", " \t\n", "unexpected", "2", "disable", "enabled"] {
                unsafe { std::env::set_var(NAME, value) };
                assert_eq!(env_flag(NAME, default), default, "{value:?}");
            }
            for (values, expected) in [
                (
                    ["1", "true", "yes", "on", " 1 ", " TrUe ", " YeS ", " ON "],
                    true,
                ),
                (
                    ["0", "false", "no", "off", " 0 ", " FaLsE ", " No ", " OFF "],
                    false,
                ),
            ] {
                for value in values {
                    unsafe { std::env::set_var(NAME, value) };
                    assert_eq!(env_flag(NAME, default), expected, "{value:?}");
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                unsafe { std::env::set_var(NAME, std::ffi::OsString::from_vec(vec![0xff])) };
                assert_eq!(env_flag(NAME, default), default);
            }
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn env_flag_invalid_warn_once_without_value() {
        const NAME: &str = "ADK_ENV_FLAG_WARN_TEST";
        const EMPTY: &str = "ADK_ENV_FLAG_EMPTY_TEST";
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _value = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            NAME,
            "private-invalid-token".as_ref(),
        );
        let _empty = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            EMPTY,
            " \t ".as_ref(),
        );
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let dispatch = dispatch.clone();
                scope.spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        assert!(!env_flag(NAME, false));
                        assert!(env_flag(NAME, true));
                        assert!(env_flag(EMPTY, true));
                    })
                });
            }
        });
        let bytes = capture.0.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes);
        assert_eq!(output.matches(NAME).count(), 1, "{output}");
        assert!(!output.contains("private-invalid-token"), "{output}");
        assert!(!output.contains(EMPTY), "{output}");
    }
}
