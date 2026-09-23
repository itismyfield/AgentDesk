//! Final provider executable selection and provenance.
use super::*;

pub(super) fn finalize_resolution(
    requested_binary: String,
    resolved_path: PathBuf,
    source: String,
    attempts: Vec<String>,
) -> BinaryResolution {
    // The stock npm batch shim adds cmd.exe's 8191-character limit to every
    // prompt. Resolve the executable from that same installation on Windows;
    // explicit registry/env launchers retain their operator-defined semantics.
    #[cfg(windows)]
    let resolved_path = if requested_binary == "codex"
        && matches!(
            source.as_str(),
            "current_path" | "login_shell_path" | "fallback_path"
        ) {
        windows_codex::native_from_npm_shim(&resolved_path, std::env::consts::ARCH)
            .unwrap_or(resolved_path)
    } else {
        resolved_path
    };
    let canonical_path = std::fs::canonicalize(&resolved_path).ok();
    BinaryResolution {
        requested_binary,
        resolved_path: Some(resolved_path.to_string_lossy().to_string()),
        canonical_path: canonical_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        source: Some(source),
        attempts,
        failure_kind: None,
        exec_path: build_exec_path(&resolved_path, canonical_path.as_deref()),
    }
}
