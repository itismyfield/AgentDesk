use std::path::Path;
use std::process::Command;

const PROBE: &str = r#"
#[cfg(test)]
compile_error!("the production getter probe must compile without cfg(test)");

extern crate self as tracing;
extern crate self as tempfile;

#[macro_export]
macro_rules! warn {
    (%$error:ident, $message:literal) => { crate::unexpected_warning($message, &$error) };
}
pub fn unexpected_warning(message: &str, error: &dyn std::fmt::Display) {
    panic!("{message}: {error}");
}

// These cases must never enter corrupt-label replacement; fail closed if they do.
pub struct NamedTempFile;
impl NamedTempFile {
    pub fn new_in(_: &std::path::Path) -> std::io::Result<Self> {
        panic!("unexpected corrupt-label replacement")
    }
    pub fn as_file(&self) -> &std::fs::File {
        panic!("unexpected replacement file sync")
    }
    pub fn persist(self, _: &std::path::Path) -> Result<(), PersistError> {
        panic!("unexpected replacement file publish")
    }
}
pub struct PersistError { pub error: std::io::Error }
impl std::io::Write for NamedTempFile {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        panic!("unexpected replacement file write")
    }
    fn flush(&mut self) -> std::io::Result<()> {
        panic!("unexpected replacement file flush")
    }
}

mod config {
    pub fn runtime_root() -> Option<std::path::PathBuf> {
        Some(std::env::var_os("AGENTDESK_ROOT_DIR").expect("explicit fixture root").into())
    }
}
mod services {
    pub mod discord {
        pub mod runtime_store {
            pub fn fsync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
                assert!(path.starts_with(crate::config::runtime_root().unwrap()));
                assert!(path.is_file());
                Ok(())
            }
        }
    }
    pub mod platform {
        pub mod shell {
            pub fn hostname_short() -> String {
                std::env::var("ADK_LABEL_PROBE_HOST").expect("explicit fixture hostname")
            }
        }
        #[path = __LABEL_SOURCE__]
        pub mod session_host_label;
    }
}

fn main() {
    use services::platform::{session_host_label::session_host_label, shell::hostname_short};
    let root = config::runtime_root().unwrap();
    let path = root.join("runtime/session_host_label");
    let expected = std::env::var("ADK_LABEL_PROBE_EXPECTED").unwrap();
    assert_eq!(session_host_label(), expected, "restore stored spelling");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{expected}\n"));
    assert_eq!(session_host_label(), expected);
    if std::env::args().any(|arg| arg == "--change-inputs") {
        std::fs::write(&path, "Other-Host\n").unwrap();
        // SAFETY: this ordinary probe is single-threaded and its adapters start no threads.
        unsafe { std::env::set_var("ADK_LABEL_PROBE_HOST", "renamed-host") };
        assert_eq!(hostname_short(), "renamed-host", "hostname input really changed");
        assert_eq!(session_host_label(), expected, "keep the first cached label");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Other-Host\n");
    }
    println!("SESSION_HOST_LABEL_PROBE_OK");
}
"#;

fn run_probe(binary: &Path, root: &Path, host: &str, expected: &str, change_inputs: bool) {
    let mut command = Command::new(binary);
    command
        .env("AGENTDESK_ROOT_DIR", root)
        .env("ADK_LABEL_PROBE_HOST", host)
        .env("ADK_LABEL_PROBE_EXPECTED", expected)
        .env_remove("AGENTDESK_CONFIG");
    if change_inputs {
        command.arg("--change-inputs");
    }
    let output = command
        .output()
        .expect("execute ordinary production getter probe");
    assert!(
        output.status.success(),
        "{host} -> {expected}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "SESSION_HOST_LABEL_PROBE_OK"
    );
}

#[test]
fn ordinary_getter_restores_creates_and_caches_host_spelling() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("probe.rs");
    let binary = fixture
        .path()
        .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
    let production =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/platform/session_host_label.rs");
    std::fs::write(
        &source,
        PROBE.replace("__LABEL_SOURCE__", &format!("{production:?}")),
    )
    .unwrap();
    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-name", "session_host_label_probe"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile ordinary production getter probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let saved = fixture.path().join("saved");
    std::fs::create_dir_all(saved.join("runtime")).unwrap();
    std::fs::write(saved.join("runtime/session_host_label"), "MacBook-Pro-14\n").unwrap();
    run_probe(&binary, &saved, "macbook-pro-14", "MacBook-Pro-14", true);

    let fresh = fixture.path().join("fresh");
    assert!(!fresh.exists());
    run_probe(&binary, &fresh, "MacBook-Pro-14", "MacBook-Pro-14", false);
    assert_eq!(
        std::fs::read_to_string(fresh.join("runtime/session_host_label")).unwrap(),
        "MacBook-Pro-14\n"
    );
    run_probe(&binary, &fresh, "macbook-pro-14", "MacBook-Pro-14", true);
}
