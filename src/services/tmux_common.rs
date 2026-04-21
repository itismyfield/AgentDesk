use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::services::tmux_diagnostics::clear_tmux_exit_reason;

/// Format a tmux session name as an exact-match target.
///
/// tmux `-t` flags perform prefix matching by default: `-t foo` matches
/// both `foo` and `foo-bar`.  Prefixing with `=` forces exact matching,
/// preventing the wrong session from being targeted when session names
/// share a common prefix (e.g. main vs thread sessions).
pub fn tmux_exact_target(session_name: &str) -> String {
    format!("={}", session_name)
}

/// Subdirectory under the runtime root where session temp files live.
const SESSIONS_SUBDIR: &str = "runtime/sessions";

/// Returns the persistent AgentDesk sessions directory, if a runtime root
/// is configured. This is the new canonical location for session temp files
/// (jsonl, input FIFO, owner markers, prompt, etc.).
///
/// Returns None when `runtime_root()` is unavailable (rare; only during
/// very early bootstrap or broken environments). Callers should fall back
/// to `std::env::temp_dir()` in that case — see `agentdesk_temp_dir()`.
pub fn persistent_sessions_dir() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(SESSIONS_SUBDIR))
}

/// Get the platform-appropriate directory for AgentDesk session runtime files.
///
/// Prefers the persistent path under `runtime_root()/runtime/sessions/` so
/// that session jsonl/FIFO/owner markers survive across dcserver restarts
/// (see issue #892). Falls back to `std::env::temp_dir()` only when a
/// runtime root is not available.
pub fn agentdesk_temp_dir() -> String {
    match persistent_sessions_dir() {
        Some(dir) => {
            // Best-effort lazy create so early callers (tests, one-off tools)
            // don't fail before the dcserver startup bootstrap runs. The
            // startup code also calls `ensure_sessions_dir_on_startup()` so
            // wrappers spawned after boot write into the right place.
            let _ = ensure_sessions_dir_inner(&dir);
            dir.display().to_string()
        }
        None => std::env::temp_dir().display().to_string(),
    }
}

fn ensure_sessions_dir_inner(dir: &PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o700 {
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(dir, perms);
            }
        }
    }
    Ok(())
}

/// Startup hook: create the persistent sessions directory (0o700) so that
/// wrappers spawned after dcserver boot write into the canonical location.
/// Idempotent; safe to call multiple times.
pub fn ensure_sessions_dir_on_startup() -> Result<(), String> {
    let Some(dir) = persistent_sessions_dir() else {
        return Ok(()); // nothing to do when no runtime_root
    };
    ensure_sessions_dir_inner(&dir)
        .map_err(|e| format!("Failed to create sessions dir '{}': {}", dir.display(), e))
}

fn host_temp_namespace() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn session_temp_prefix(session_name: &str) -> String {
    let host = host_temp_namespace();
    let mut hasher = Sha256::new();
    hasher.update(current_tmux_owner_marker().as_bytes());
    hasher.update(b"|");
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let runtime_hash = format!("{:x}", digest);
    format!(
        "agentdesk-{}-{}-{}",
        &runtime_hash[..12],
        host,
        session_name
    )
}

/// Build a path for an AgentDesk runtime temp file in the **canonical**
/// (persistent) location.
///
/// Example: `session_temp_path("mySession", "jsonl")`
///   → `~/.adk/release/runtime/sessions/agentdesk-<runtime>-<host>-mySession.jsonl`
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        agentdesk_temp_dir(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Build a path to the *legacy* `/tmp/`-based location for a session temp
/// file. Wrappers spawned before the migration hold open fds to these files;
/// readers must be able to still find them during the migration window.
pub fn legacy_tmp_session_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        std::env::temp_dir().display(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Resolve whichever location actually holds the session temp file.
/// Prefers the new persistent path when both exist. Returns `None` when
/// neither location has the file. Used by read-side code (e.g. the
/// `session_usable` check and the watcher skip-on-missing-output file)
/// so they accept either location during the migration window.
pub fn resolve_session_temp_path(session_name: &str, extension: &str) -> Option<String> {
    let new_path = session_temp_path(session_name, extension);
    if std::path::Path::new(&new_path).exists() {
        return Some(new_path);
    }
    let legacy = legacy_tmp_session_path(session_name, extension);
    if std::path::Path::new(&legacy).exists() {
        return Some(legacy);
    }
    None
}

/// Delete all known session temp files for the given tmux session.
/// Idempotent — missing files are not errors. Hits both the new persistent
/// location and the legacy `/tmp/` location so cleanup is total regardless
/// of where the wrapper originally wrote.
pub fn cleanup_session_temp_files(session_name: &str) {
    // All extensions we ever allocate under the session prefix.
    const EXTS: &[&str] = &[
        "jsonl",
        "input",
        "prompt",
        "owner",
        "sh",
        "generation",
        "exit_reason",
    ];
    for ext in EXTS {
        let _ = std::fs::remove_file(session_temp_path(session_name, ext));
        let _ = std::fs::remove_file(legacy_tmp_session_path(session_name, ext));
    }
}

/// Get the current AgentDesk runtime root marker for tmux session ownership.
pub fn current_tmux_owner_marker() -> String {
    crate::config::runtime_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".adk/release".to_string())
}

/// Path to the owner marker file for a tmux session.
pub fn tmux_owner_path(tmux_session_name: &str) -> String {
    session_temp_path(tmux_session_name, "owner")
}

/// Write the owner marker file so this runtime claims the tmux session.
pub fn write_tmux_owner_marker(tmux_session_name: &str) -> Result<(), String> {
    clear_tmux_exit_reason(tmux_session_name);
    let owner_path = tmux_owner_path(tmux_session_name);
    std::fs::write(&owner_path, current_tmux_owner_marker())
        .map_err(|e| format!("Failed to write tmux owner marker: {}", e))
}

// ── Rolling head-truncate for session jsonl ─────────────────────────────
//
// We cap session jsonl files at SIZE_CAP_BYTES. When they exceed the cap,
// we truncate from the head keeping ~TARGET_KEEP_BYTES worth of the most
// recent complete lines. A partial leading line after truncation is dropped
// so downstream stream-json parsers never see half of a record.

/// Soft cap at which we trigger head-truncation.
pub const JSONL_SIZE_CAP_BYTES: u64 = 20 * 1024 * 1024;
/// Target size to keep after truncation.
pub const JSONL_TARGET_KEEP_BYTES: u64 = 15 * 1024 * 1024;

/// Truncate a jsonl file from the head, keeping only complete lines totaling
/// at most `target_keep_bytes`. A leading partial line after the keep-window
/// is dropped so the first byte of the rewritten file is the first byte of a
/// complete line.
///
/// Returns `Ok(Some(new_size))` if the file was rewritten, `Ok(None)` if the
/// file is under cap or missing.
pub fn truncate_jsonl_head_safe(
    path: &str,
    size_cap_bytes: u64,
    target_keep_bytes: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = meta.len();
    if size <= size_cap_bytes {
        return Ok(None);
    }

    // Figure out the byte offset we *want* to start keeping from.
    let start_offset = size.saturating_sub(target_keep_bytes);

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::with_capacity((size - start_offset) as usize);
    file.read_to_end(&mut buf)?;
    drop(file);

    // Drop any partial leading line: advance past the first newline so the
    // kept buffer begins at a line boundary. If no newline exists in buf
    // at all, we're keeping a single partial line — drop everything rather
    // than risk emitting a garbled record. (This is the rare case where
    // target_keep_bytes lands in the middle of an exceptionally huge line.)
    let keep_start = if start_offset == 0 {
        0 // no truncation needed at the head
    } else {
        match buf.iter().position(|b| *b == b'\n') {
            Some(idx) => idx + 1,
            None => buf.len(), // nothing complete to keep
        }
    };

    let kept = &buf[keep_start..];
    let new_size = kept.len() as u64;

    // Atomic-ish rewrite: write to sibling temp then rename.
    let tmp_path = format!("{}.truncate.tmp", path);
    {
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        out.write_all(kept)?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(Some(new_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_temp_path_is_namespaced_by_runtime_root() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        unsafe {
            std::env::set_var("HOSTNAME", "test-host");
            std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-a");
        }
        let path_a = session_temp_path("tmux-a", "jsonl");

        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-b") };
        let path_b = session_temp_path("tmux-a", "jsonl");

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }

        assert_ne!(path_a, path_b);
        assert!(path_a.contains("tmux-a"));
        assert!(path_b.contains("tmux-a"));
    }

    #[test]
    fn session_temp_path_uses_persistent_runtime_dir_when_root_is_set() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");

        // tmpdir we own for the test
        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-persistent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
        }

        let path = session_temp_path("tmux-persistent-test", "jsonl");
        let expected_prefix = tdir.join("runtime").join("sessions");
        assert!(
            path.starts_with(&expected_prefix.display().to_string()),
            "expected {} to start with {}",
            path,
            expected_prefix.display()
        );

        // agentdesk_temp_dir() should have created the directory as a side
        // effect — verify it's there and accessible.
        assert!(
            expected_prefix.exists(),
            "persistent sessions dir not created"
        );

        // Restore env and clean up.
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn agentdesk_temp_dir_uses_persistent_sessions_subpath() {
        // Verify that when runtime_root() is Some(root), agentdesk_temp_dir()
        // returns a path ending in `runtime/sessions`. We don't clear HOME in
        // this test because other concurrent tests rely on env stability —
        // instead we assert the structural property.
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-subpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
        }

        let dir = agentdesk_temp_dir();
        let expected = tdir.join("runtime").join("sessions");
        assert_eq!(dir, expected.display().to_string());

        // Fallback branch: when persistent_sessions_dir() is None
        // (no runtime_root available) we must return std::env::temp_dir().
        // We can't easily force runtime_root()→None without clobbering HOME
        // for concurrent tests, so we test the inner decision explicitly
        // by asserting persistent_sessions_dir is Some(expected) — its
        // presence exercises the Some arm; the None arm is trivially
        // `std::env::temp_dir().display().to_string()`.
        assert_eq!(persistent_sessions_dir(), Some(expected));

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn resolve_session_temp_path_prefers_new_over_legacy() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "resolve-host");
        }

        let session = format!("issue-892-resolve-sess-{}", std::process::id());

        // No files anywhere → None.
        assert!(
            resolve_session_temp_path(&session, "jsonl").is_none(),
            "expected None when neither location has the file"
        );

        // Create the legacy file only.
        let legacy = legacy_tmp_session_path(&session, "jsonl");
        std::fs::write(&legacy, b"legacy").unwrap();
        assert_eq!(
            resolve_session_temp_path(&session, "jsonl"),
            Some(legacy.clone()),
            "expected legacy path when only legacy exists"
        );

        // Create the new persistent file — should win.
        let new_path = session_temp_path(&session, "jsonl");
        std::fs::create_dir_all(std::path::Path::new(&new_path).parent().unwrap()).unwrap();
        std::fs::write(&new_path, b"new").unwrap();
        assert_eq!(
            resolve_session_temp_path(&session, "jsonl"),
            Some(new_path.clone()),
            "expected new path to be preferred over legacy"
        );

        // Cleanup.
        let _ = std::fs::remove_file(&legacy);
        let _ = std::fs::remove_file(&new_path);
        let _ = std::fs::remove_dir_all(&tdir);

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }

    #[test]
    fn truncate_jsonl_head_safe_drops_partial_leading_line() {
        let tdir = std::env::temp_dir().join(format!("adk-issue-892-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&tdir).unwrap();
        let path = tdir.join("session.jsonl");

        // Build a file: several known-length lines, each ending in \n.
        // Each line: "line-NN:<pad>\n" — 100 bytes total so it's easy to reason about.
        let line_size = 100usize;
        let lines: Vec<String> = (0..200)
            .map(|i| {
                let prefix = format!("line-{:03}:", i);
                let pad = line_size - prefix.len() - 1; // -1 for \n
                format!("{}{}\n", prefix, "x".repeat(pad))
            })
            .collect();
        let content: String = lines.concat();
        std::fs::write(&path, &content).unwrap();

        // Cap at 5 KB, keep ~3.5 KB → must preserve a whole number of lines
        // ending with the last line of input.
        let cap = 5_000u64;
        let keep = 3_500u64;
        let result =
            truncate_jsonl_head_safe(path.to_str().unwrap(), cap, keep).expect("truncate ok");
        assert!(result.is_some(), "file should have been truncated");

        let after = std::fs::read_to_string(&path).unwrap();

        // 1. Every kept line must be complete (file ends with \n).
        assert!(
            after.ends_with('\n'),
            "truncated file must end with newline"
        );

        // 2. Last line of output equals last line of input.
        let last_out = after.lines().last().unwrap();
        let last_in = lines.last().unwrap().trim_end_matches('\n');
        assert_eq!(
            last_out, last_in,
            "last kept line should be last input line"
        );

        // 3. No partial first line. Every output line must match a whole input line.
        for out_line in after.lines() {
            assert!(
                lines.iter().any(|l| l.trim_end_matches('\n') == out_line),
                "unexpected partial line in output: {out_line}"
            );
        }

        // 4. Size is within the keep target (give or take one whole line).
        let new_size = after.len() as u64;
        assert!(
            new_size <= keep,
            "new size {} should be <= target keep {}",
            new_size,
            keep
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn truncate_jsonl_head_safe_no_op_under_cap() {
        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-trunc-noop-{}", std::process::id()));
        std::fs::create_dir_all(&tdir).unwrap();
        let path = tdir.join("small.jsonl");
        std::fs::write(&path, b"line1\nline2\n").unwrap();
        let result = truncate_jsonl_head_safe(path.to_str().unwrap(), 1_000_000, 500_000)
            .expect("truncate ok");
        assert!(result.is_none(), "small file should not be truncated");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line1\nline2\n");
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn truncate_jsonl_head_safe_missing_file_returns_none() {
        let result =
            truncate_jsonl_head_safe("/tmp/issue-892-does-not-exist-xyz.jsonl", 1_000, 500)
                .expect("missing file should be ok");
        assert!(result.is_none());
    }
}
