//! Platform abstraction layer.
//!
//! Provides traits for OS-specific operations (binary lookup, process dump,
//! shell execution) so the rest of the codebase can be platform-agnostic.

pub mod binary_resolver;
mod dump_tool;
pub mod network;
mod session_host_label;
pub use session_host_label::session_host_label;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use session_host_label::with_session_host_label_path_for_test;
pub mod shell;
pub mod tmux;
#[cfg(windows)]
pub(crate) mod windows_job;

/// Test builds mark the thread inside the watcher registry's owner publication, so tmux
/// namespace I/O can assert it never runs under that fence.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod owner_publication_fence {
    thread_local! {
        static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Holds the fence until dropped, then restores the outer state.
    pub(crate) struct Scope(bool);

    impl Scope {
        pub(crate) fn enter() -> Self {
            Self(HELD.replace(true))
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            HELD.set(self.0);
        }
    }

    pub(crate) fn held() -> bool {
        HELD.get()
    }
}

pub use binary_resolver::active_provider_context;
pub use binary_resolver::{
    BinaryResolution, apply_binary_resolution, augment_exec_path, merged_runtime_path,
    probe_provider_binary_version, resolve_provider_binary, with_provider_execution_context,
};
pub use dump_tool::capture_process_dump;
pub use shell::hostname_short;

// Compatibility re-exports while git helpers move out of platform::shell.
#[allow(unused_imports)]
pub use crate::services::git::{
    ManagedWorktreeCleanup, cleanup_managed_worktree, find_latest_commit_for_issue,
    find_worktree_for_issue, git_head_commit, is_no_repo_mapping_error, resolve_repo_dir,
    resolve_repo_dir_for_id, resolve_repo_dir_for_target,
};
