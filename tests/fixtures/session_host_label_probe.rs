#[cfg(test)]
compile_error!("the production getter probe must compile without cfg(test)");

extern crate self as tempfile;
extern crate self as tracing;

#[macro_export]
macro_rules! warn {
    (%$error:ident, $message:literal) => {
        crate::unexpected_warning($message, &$error)
    };
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
pub struct PersistError {
    pub error: std::io::Error,
}
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
        Some(
            std::env::var_os("AGENTDESK_ROOT_DIR")
                .expect("explicit fixture root")
                .into(),
        )
    }
}
mod domain {
    pub mod file_sync {
        pub fn fsync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
            assert!(path.starts_with(crate::config::runtime_root().unwrap()));
            assert!(path.is_file());
            Ok(())
        }
    }
}
mod shell {
    pub fn hostname_short() -> String {
        std::env::var("ADK_LABEL_PROBE_HOST").expect("explicit fixture hostname")
    }
}
#[path = "../../src/services/platform/session_host_label.rs"]
mod session_host_label;

fn main() {
    use session_host_label::session_host_label;
    use shell::hostname_short;
    let root = config::runtime_root().unwrap();
    let path = root.join("runtime/session_host_label");
    let expected = std::env::var("ADK_LABEL_PROBE_EXPECTED").unwrap();
    assert_eq!(session_host_label(), expected, "restore stored spelling");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{expected}\n")
    );
    assert_eq!(session_host_label(), expected);
    if std::env::args().any(|arg| arg == "--change-inputs") {
        std::fs::write(&path, "Other-Host\n").unwrap();
        // SAFETY: this ordinary probe is single-threaded and its adapters start no threads.
        unsafe { std::env::set_var("ADK_LABEL_PROBE_HOST", "renamed-host") };
        assert_eq!(
            hostname_short(),
            "renamed-host",
            "hostname input really changed"
        );
        assert_eq!(
            session_host_label(),
            expected,
            "keep the first cached label"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Other-Host\n");
    }
    println!("SESSION_HOST_LABEL_PROBE_OK");
}
