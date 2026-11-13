mod services {
    #[path = "__ROOT__/src/services/claude_command.rs"]
    pub mod claude_command;
    pub mod process {
        pub fn shell_escape(value: &str) -> String { value.to_owned() }
    }
    pub mod platform {
        use std::collections::BTreeSet;
        use std::ffi::{OsStr, OsString};
        use std::path::{Path, PathBuf};
        use std::process::Command;
        use std::sync::OnceLock;
        pub struct BinaryResolution { pub resolved_path: Option<String> }
        pub fn apply_binary_resolution(_: &mut Command, _: &BinaryResolution) {}
        pub fn augment_exec_path(_: &mut Command, _: &Path) {}
        pub mod binary_resolver {
            pub fn resolve_claude_binary_sealed(_: crate::services::claude_command::ClaudeSeal) -> super::BinaryResolution {
                super::BinaryResolution { resolved_path: None }
            }
        }
        fn merged_runtime_path() -> Option<OsString> { None }
        fn resolve_login_shell_path_os() -> Option<OsString> { None }
        fn standard_fallback_dirs() -> Vec<PathBuf> { Vec::new() }
        fn extend_split_paths(_: Option<OsString>, _: &mut Vec<PathBuf>, _: &mut BTreeSet<PathBuf>) {}
        fn push_unique_path(_: PathBuf, _: &mut Vec<PathBuf>, _: &mut BTreeSet<PathBuf>) {}
        fn join_paths_lossy(_: Vec<PathBuf>) -> Option<OsString> { None }
        fn resolve_in_paths(_: &OsStr, _: Option<OsString>, _: &Path) -> Option<PathBuf> { None }
        fn current_dir_fallback() -> PathBuf { PathBuf::from("/") }
        #[path = "__ROOT__/src/services/platform/binary_resolver/runtime_path.rs"]
        pub mod runtime_path;
    }
}
#[path = "__ROOT__/src/services/decision_gate/runtime/key_initial_env.rs"]
mod initial_env;

fn main() {
    let key = unsafe { initial_env::take() };
    assert_eq!(key.as_deref(), Some("fixture-probe-sentinel"));
    let stub = std::env::args().nth(1).unwrap();
    for mut command in [
        services::platform::runtime_path::runtime_command(&stub).unwrap(),
        services::claude_command::ClaudeCommandBuilder::for_tmux_wrapper_argv(&stub).into_command(),
    ] {
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"unset", "child inherited credential");
    }
    println!("captured=true children=2 inherited=0");
}
