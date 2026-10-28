//! Deployment preflight uses the daemon's file selection before a new binary exists.

#[path = "../src/config/config_path/selection.rs"]
mod selection;

fn main() -> std::io::Result<()> {
    use std::io::Write;
    let root = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "runtime root required")
    })?;
    let path = selection::resolve_with(selection::explicit_env(), Some(root.into()), None, None);
    std::io::stdout().write_all(path.as_os_str().as_encoded_bytes())?;
    std::io::stdout().write_all(b"\n")
}
