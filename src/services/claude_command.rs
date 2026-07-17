//! Single authority (chokepoint) for launching the Claude CLI.
//!
//! Historically every Claude spawn site assembled its own `Command` and then
//! *remembered* to apply the gateway launch env (`ANTHROPIC_BASE_URL` /
//! `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, #4553). That made the guard a
//! per-site obligation, and #4553's R3 review caught a spawn site that had
//! silently bypassed it. Enumerating sites is only a snapshot — a future
//! seventh site would bypass the guard again.
//!
//! This module closes the class by *construction* instead of by enumeration:
//!
//!   * [`ClaudeLaunchEnv`] is the only carrier of the resolved gateway
//!     Inject|Scrub decision, and it is produced solely by
//!     [`ClaudeLaunchEnv::resolve`]. The launch-vs-probe policy therefore lives
//!     in exactly one place, keyed off [`ClaudeLaunchIntent`].
//!   * [`ClaudeCommandBuilder`] is the only sanctioned way to obtain a
//!     `Command` that launches (or transitively spawns) the Claude CLI. Binary
//!     resolution and the gateway launch env are applied when the builder is
//!     constructed, so a caller physically cannot hand back a Claude command
//!     that skipped the guard.
//!
//! The raw [`crate::services::claude_gateway_proxy`] primitives
//! (`ClaudeGatewayProxyEnv`, `resolve_for_launch`, `apply_to_command`,
//! `append_shell_env`) must be reached ONLY through this module. A
//! source-scanning guard test (`chokepoint_guard_tests`) fails the build if any
//! other module references them directly, so the single authority cannot erode.

use std::ffi::OsStr;
use std::process::Command;

use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;
use crate::services::platform::BinaryResolution;

/// Why a Claude process is being spawned. Selects the gateway env policy so the
/// launch-vs-probe decision is made in exactly one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeLaunchIntent {
    /// A real turn / model-routing launch. The gateway env is resolved from
    /// live config + reachability (Inject when the proxy is enabled and
    /// reachable, Scrub otherwise).
    Turn,
    /// A `--version` (or otherwise non-model-routing) probe. `--version` never
    /// routes models or spawns subagents, so probes always run native (Scrub),
    /// independent of gateway/config state.
    VersionProbe,
}

/// Resolved launch environment for a single Claude spawn.
///
/// This is the only value that carries the gateway Inject|Scrub decision to a
/// launch site. It is produced solely by [`ClaudeLaunchEnv::resolve`] (or the
/// test-only constructors) so the resolution policy is centralised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeLaunchEnv {
    gateway: ClaudeGatewayProxyEnv,
}

impl ClaudeLaunchEnv {
    /// Resolve the gateway launch env for the given intent. This is the single
    /// place that maps a launch intent onto the gateway Inject|Scrub decision.
    pub(crate) fn resolve(intent: ClaudeLaunchIntent) -> Self {
        let gateway = match intent {
            ClaudeLaunchIntent::Turn => crate::services::claude_gateway_proxy::resolve_for_launch(),
            ClaudeLaunchIntent::VersionProbe => ClaudeGatewayProxyEnv::Scrub,
        };
        Self { gateway }
    }

    /// Apply the resolved gateway env to a `Command` (Inject sets the proxy
    /// vars, Scrub removes any inherited values). Used by launch sites that
    /// build a `Command` outside [`ClaudeCommandBuilder`] (e.g. the wrapper
    /// command assembled inside `session_backend`).
    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        self.gateway.apply_to_command(command);
    }

    /// Render the resolved gateway env as `export`/`unset` shell lines for
    /// launch sites that write a bash launch script rather than spawning a
    /// `Command` directly (Claude-TUI launch script, legacy tmux wrapper).
    pub(crate) fn append_shell_env(&self, output: &mut String) {
        self.gateway.append_shell_env(output);
    }

    #[cfg(test)]
    pub(crate) fn inject_for_test(base_url: &str) -> Self {
        Self {
            gateway: crate::services::claude_gateway_proxy::launch_env_for_test(
                true, base_url, true,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn scrub_for_test() -> Self {
        Self {
            gateway: crate::services::claude_gateway_proxy::launch_env_for_test(
                false,
                "http://unused.invalid",
                true,
            ),
        }
    }
}

/// By-construction builder for a Claude-launching `Command`.
///
/// The binary-resolution PATH (when the program is the Claude binary itself)
/// and the gateway launch env are applied the moment the builder is created, so
/// the wrapped `Command` is guarded from the first instant it exists. Callers
/// finish configuring the command through [`ClaudeCommandBuilder::command_mut`]
/// (args, cwd, other env, stdio, process group) and extract it with
/// [`ClaudeCommandBuilder::into_command`]. No launch site can produce a Claude
/// command that skipped the guard because the builder is the only constructor.
pub(crate) struct ClaudeCommandBuilder {
    command: Command,
}

impl ClaudeCommandBuilder {
    /// Shared construction path for every builder flavour. This is the single
    /// authority: `launch_env.apply_to_command` here is the gateway guard that
    /// every Claude spawn site depends on. Removing it must break the mutation
    /// test in `chokepoint_gateway_mutation_tests`.
    fn build(
        program: impl AsRef<OsStr>,
        resolution: Option<&BinaryResolution>,
        launch_env: ClaudeLaunchEnv,
    ) -> Self {
        let mut command = Command::new(program);
        if let Some(resolution) = resolution {
            crate::services::platform::apply_binary_resolution(&mut command, resolution);
        }
        launch_env.apply_to_command(&mut command);
        Self { command }
    }

    /// Build a command that launches the Claude binary directly. Applies the
    /// binary-resolution PATH and the gateway env for `intent` by construction.
    pub(crate) fn for_binary(
        program: impl AsRef<OsStr>,
        resolution: &BinaryResolution,
        intent: ClaudeLaunchIntent,
    ) -> Self {
        Self::build(program, Some(resolution), ClaudeLaunchEnv::resolve(intent))
    }

    /// Build a command that launches a wrapper program which transitively
    /// spawns Claude (`agentdesk tmux-wrapper …`, `claude-e …`). The gateway env
    /// is applied by construction to the wrapper and the wrapped Claude child
    /// inherits it. The binary-resolution PATH is supplied separately by the
    /// caller because the wrapper — not the Claude binary — is the program here.
    pub(crate) fn for_wrapper(program: impl AsRef<OsStr>, intent: ClaudeLaunchIntent) -> Self {
        Self::build(program, None, ClaudeLaunchEnv::resolve(intent))
    }

    /// Test-only constructor that injects a pre-resolved launch env, letting a
    /// test exercise the exact production `build` path (and thus the gateway
    /// guard arm) without a live config.
    #[cfg(test)]
    pub(crate) fn build_for_test(
        program: impl AsRef<OsStr>,
        resolution: Option<&BinaryResolution>,
        launch_env: ClaudeLaunchEnv,
    ) -> Self {
        Self::build(program, resolution, launch_env)
    }

    /// Mutable access to the wrapped command for site-specific configuration
    /// (args, cwd, other env, stdio, process group). The gateway env and PATH
    /// are already applied; sites never set the gateway vars themselves.
    pub(crate) fn command_mut(&mut self) -> &mut Command {
        &mut self.command
    }

    /// Consume the builder and return the fully-guarded `Command`.
    pub(crate) fn into_command(self) -> Command {
        self.command
    }
}

#[cfg(test)]
fn command_env_map(command: &Command) -> std::collections::HashMap<String, Option<String>> {
    command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

#[cfg(test)]
mod chokepoint_gateway_mutation_tests {
    use super::*;
    use crate::services::platform::BinaryResolution;

    const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
    const DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

    fn claude_resolution() -> BinaryResolution {
        BinaryResolution {
            requested_binary: "claude".to_string(),
            resolved_path: Some("claude".to_string()),
            canonical_path: None,
            source: Some("test".to_string()),
            attempts: Vec::new(),
            failure_kind: None,
            exec_path: None,
        }
    }

    // Mutation target: `ClaudeCommandBuilder::build` applies the gateway env via
    // `launch_env.apply_to_command`. Every Claude spawn site funnels through
    // `build` (via `for_binary` / `for_wrapper`), so deleting that single line
    // makes ALL of these assertions fail — proving the guard is applied by
    // construction rather than remembered per site.

    #[test]
    fn for_binary_injects_gateway_env_by_construction() {
        let resolution = claude_resolution();
        let builder = ClaudeCommandBuilder::build_for_test(
            "claude",
            Some(&resolution),
            ClaudeLaunchEnv::inject_for_test("http://127.0.0.1:10100"),
        );
        let envs = command_env_map(&builder.into_command());
        // If the gateway arm is removed from `build`, ANTHROPIC_BASE_URL is
        // never set and this assertion fails.
        assert_eq!(
            envs.get(BASE_URL_ENV),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&Some("1".to_string())));
    }

    #[test]
    fn for_binary_scrubs_inherited_gateway_env_by_construction() {
        let resolution = claude_resolution();
        let builder = ClaudeCommandBuilder::build_for_test(
            "claude",
            Some(&resolution),
            ClaudeLaunchEnv::scrub_for_test(),
        );
        // Scrub records an `env_remove` for each gateway var, so `get_envs`
        // reports `(var, None)`. If the gateway arm is removed from `build`, no
        // removal is recorded and `get` returns `None` (not `Some(&None)`),
        // failing these assertions.
        let envs = command_env_map(&builder.into_command());
        assert_eq!(envs.get(BASE_URL_ENV), Some(&None));
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&None));
    }

    #[test]
    fn for_wrapper_applies_gateway_env_without_binary_resolution() {
        let builder = ClaudeCommandBuilder::build_for_test(
            "claude-e",
            None,
            ClaudeLaunchEnv::inject_for_test("http://127.0.0.1:10100"),
        );
        let envs = command_env_map(&builder.into_command());
        assert_eq!(
            envs.get(BASE_URL_ENV),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&Some("1".to_string())));
    }

    #[test]
    fn version_probe_intent_always_scrubs() {
        // The probe policy lives in one place (`ClaudeLaunchEnv::resolve`).
        // Removing the `VersionProbe => Scrub` arm would flip this to Inject
        // (or read live config) and break the assertion.
        let env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::VersionProbe);
        let mut command = Command::new("claude");
        command
            .env(BASE_URL_ENV, "http://inherited.example:9999")
            .env(DISCOVERY_ENV, "inherited-value");
        env.apply_to_command(&mut command);
        let envs = command_env_map(&command);
        assert_eq!(envs.get(BASE_URL_ENV), Some(&None));
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&None));
    }
}

#[cfg(test)]
mod chokepoint_guard_tests {
    use std::path::{Path, PathBuf};

    /// Files that are permitted to reference the raw gateway primitives: the
    /// definition site and this chokepoint module.
    const SANCTIONED: &[&str] = &["claude_gateway_proxy.rs", "claude_command.rs"];

    /// Substrings whose presence outside the sanctioned files signals a launch
    /// site reaching around the chokepoint. `claude_gateway_proxy::` catches any
    /// module-path access (including `launch_env_for_test`), while the type and
    /// `resolve_for_launch` catch re-exported / directly-named usage.
    const FORBIDDEN: &[&str] = &[
        "ClaudeGatewayProxyEnv",
        "resolve_for_launch",
        "claude_gateway_proxy::",
    ];

    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// By-construction guard. Fails if any module other than the sanctioned two
    /// references the raw gateway launch primitives, forcing every Claude spawn
    /// site through `ClaudeCommandBuilder` / `ClaudeLaunchEnv`. This is the
    /// "grep guard" from the issue's acceptance criteria, enforced at test time
    /// instead of at review time so the class stays closed as new sites land.
    #[test]
    fn gateway_primitives_are_confined_to_the_chokepoint() {
        let services_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services");
        let mut files = Vec::new();
        collect_rs_files(&services_dir, &mut files);
        assert!(
            !files.is_empty(),
            "guard scan found no source files under {}",
            services_dir.display()
        );

        let mut violations = Vec::new();
        for file in files {
            let file_name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if SANCTIONED.contains(&file_name) {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&file) else {
                continue;
            };
            for needle in FORBIDDEN {
                if contents.contains(needle) {
                    violations.push(format!("{} references `{}`", file.display(), needle));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "gateway launch primitives leaked outside the chokepoint \
             (route these through claude_command::ClaudeCommandBuilder / ClaudeLaunchEnv):\n{}",
            violations.join("\n")
        );
    }
}
