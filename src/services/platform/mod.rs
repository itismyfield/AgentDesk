//! Platform abstraction layer.
//!
//! Provides traits for OS-specific operations (binary lookup, process dump,
//! shell execution) so the rest of the codebase can be platform-agnostic.

pub mod binary_resolver;
mod dump_tool;
pub mod shell;
pub mod tmux;

pub use binary_resolver::{
    BinaryResolution, apply_binary_resolution, async_resolve_binary_with_login_shell,
    augment_exec_path, merged_runtime_path, probe_resolved_binary_version, resolve_provider_binary,
    with_provider_execution_context,
};
pub use dump_tool::capture_process_dump;
pub use shell::hostname_short;

// Compatibility re-exports while git helpers move out of platform::shell.
#[allow(unused_imports)]
pub use crate::services::git::{
    find_latest_commit_for_issue, find_worktree_for_issue, git_head_commit, resolve_repo_dir,
    resolve_repo_dir_for_id, resolve_repo_dir_for_target,
};
