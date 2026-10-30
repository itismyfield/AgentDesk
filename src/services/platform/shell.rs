//! Platform helpers: the host's short name, and git helpers re-exported for
//! platform callers.

#[cfg(any(not(unix), test))]
use std::process::Command;
use std::sync::OnceLock;

#[allow(unused_imports)]
pub(crate) use crate::services::git::{
    ManagedWorktreeCleanup, cleanup_managed_worktree, ensure_worktree_for_issue,
    find_latest_commit_for_issue, find_worktree_for_issue, git_best_commit_for_dispatch,
    git_branch_containing_commit, git_branch_name, git_dispatch_baseline_commit, git_head_commit,
    git_latest_commit_for_issue, git_mainline_commit_for_issue_since, git_mainline_head_commit,
    git_mainline_issue_numbers, git_merge_base, git_tracked_change_paths,
    git_tracked_change_paths_strict, is_mainlike_branch, is_no_repo_mapping_error,
    parse_github_repo_from_remote, resolve_repo_dir, resolve_repo_dir_for_id,
    resolve_repo_dir_for_target,
};

/// Get the short hostname of the current machine.
///
/// Equivalent to `hostname -s` on Unix. Falls back to "localhost" on failure.
pub fn hostname_short() -> String {
    static HOSTNAME: OnceLock<String> = OnceLock::new();
    #[cfg(unix)]
    let read = health_hostname_short;
    #[cfg(not(unix))]
    let read = hostname_command_short;
    HOSTNAME.get_or_init(read).clone()
}

#[cfg(any(not(unix), test))]
fn hostname_command_short() -> String {
    #[cfg(test)]
    HOSTNAME_SPAWNS.with(|count| count.set(count.get() + 1));
    let mut command = Command::new("hostname");
    #[cfg(unix)]
    command.arg("-s");
    command
        .output()
        .ok()
        .and_then(|output| parse_short_hostname(output.status.success(), &output.stdout))
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
thread_local! {
    static HOSTNAME_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(unix)]
    static HOSTNAME_NATIVE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The short hostname for `/health/detail`: `gethostname` instead of a `hostname -s` process per
/// request, with the same short-name parsing and "localhost" fallback.
#[cfg(unix)]
pub fn health_hostname_short() -> String {
    #[cfg(test)]
    HOSTNAME_NATIVE_READS.with(|count| count.set(count.get() + 1));
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which is writable and outlives the call;
    // gethostname writes at most `len` bytes.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    health_hostname_from(rc, &buffer)
}

#[cfg(not(unix))]
pub fn health_hostname_short() -> String {
    hostname_short()
}

/// A name that filled the buffer may have been cut without a NUL, so only a NUL-terminated
/// name counts.
#[cfg(unix)]
fn health_hostname_from(rc: i32, buffer: &[u8]) -> String {
    buffer
        .iter()
        .position(|&byte| byte == 0)
        .and_then(|len| parse_short_hostname(rc == 0, &buffer[..len]))
        .unwrap_or_else(|| "localhost".to_string())
}

fn parse_short_hostname(success: bool, stdout: &[u8]) -> Option<String> {
    if !success {
        return None;
    }
    let hostname = std::str::from_utf8(stdout).ok()?.trim();
    if hostname.is_empty() || hostname.chars().any(char::is_whitespace) {
        return None;
    }
    hostname
        .split('.')
        .next()
        .filter(|short| !short.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod hostname_tests {
    use super::*;

    #[test]
    fn accepts_native_and_fully_qualified_hostnames() {
        assert_eq!(
            parse_short_hostname(true, b"worker-1\r\n").as_deref(),
            Some("worker-1")
        );
        assert_eq!(
            parse_short_hostname(true, b"fixture-host.local\n").as_deref(),
            Some("fixture-host")
        );
    }

    #[test]
    fn rejects_failed_empty_or_invalid_hostname_output() {
        for (success, output) in [
            (false, b"worker-1".as_slice()),
            (true, b" \r\n"),
            (true, b"usage: hostname"),
            (true, b".local"),
            (true, b"\xff"),
        ] {
            assert_eq!(parse_short_hostname(success, output), None);
        }
    }

    /// The health detail's native read must name this machine exactly as `hostname -s` does.
    #[cfg(unix)]
    #[test]
    fn health_hostname_native_matches_command() {
        let command = hostname_command_short();
        assert_ne!(
            command, "localhost",
            "`hostname -s` must answer here, or two fallbacks would match"
        );
        assert_eq!(health_hostname_short(), command);
        assert_eq!(hostname_short(), command);
    }

    #[cfg(unix)]
    #[test]
    fn repeated_short_hostname_calls_spawn_no_children() {
        let expected = health_hostname_short();
        assert_eq!(hostname_short(), expected);
        HOSTNAME_SPAWNS.with(|count| count.set(0));
        HOSTNAME_NATIVE_READS.with(|count| count.set(0));
        for _ in 0..100 {
            assert_eq!(hostname_short(), expected);
        }
        assert_eq!(HOSTNAME_SPAWNS.with(|count| count.get()), 0);
        assert_eq!(HOSTNAME_NATIVE_READS.with(|count| count.get()), 0);
    }

    /// Counted from the first call in a fresh process, before the cache holds a name: filling it
    /// reads the name natively once and starts no `hostname` process.
    #[cfg(unix)]
    #[test]
    fn first_short_hostname_call_in_a_fresh_process_spawns_no_child() {
        const CHILD: &str = "ADK_TEST_HOSTNAME_FIRST_CALL_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest worker");
        if std::env::var(CHILD).ok().as_deref() == Some(name) {
            let first = hostname_short();
            assert_eq!(HOSTNAME_SPAWNS.with(|count| count.get()), 0);
            assert_eq!(
                HOSTNAME_NATIVE_READS.with(|count| count.get()),
                1,
                "this call filled the cache"
            );
            assert_eq!(hostname_short(), first);
            assert_eq!(HOSTNAME_NATIVE_READS.with(|count| count.get()), 1);
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{name}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn health_hostname_falls_back_on_failed_empty_invalid_or_cut_names() {
        let mut full = [b'a'; 256];
        full[10] = b'.';
        for (rc, buffer, expected) in [
            (0, b"Mac-mini.local\0".as_slice(), "Mac-mini"),
            (0, b"worker-1\0\0\0", "worker-1"),
            (-1, b"worker-1\0", "localhost"),
            (0, b"\0worker-1\0", "localhost"),
            (0, b"\xff\0", "localhost"),
            (0, &full, "localhost"),
            (0, b".local\0", "localhost"),
            (0, b"two words\0", "localhost"),
        ] {
            assert_eq!(health_hostname_from(rc, buffer), expected, "{buffer:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_hostname_matches_computer_name() {
        let expected = std::env::var("COMPUTERNAME").expect("Windows defines COMPUTERNAME");
        assert!(hostname_short().eq_ignore_ascii_case(&expected));
    }
}
