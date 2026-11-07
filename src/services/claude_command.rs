//! Single authority (chokepoint) for launching the Claude CLI.
//!
//! [`ClaudeCommandBuilder`] is the only sanctioned way to obtain a `Command`
//! that launches (or transitively spawns) the Claude CLI. Binary resolution is
//! applied when the builder is constructed, and [`ClaudeBinary`] seals the
//! resolved executable path so a caller physically cannot hand back a Claude
//! command assembled outside this module.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::services::platform::BinaryResolution;

/// Opaque capability for the builder's resolved Claude executable path.
///
/// This type deliberately does not implement `AsRef<OsStr>`, `AsRef<Path>`,
/// `Deref`, `Display`, or expose any path getter, so `Command::new(ClaudeBinary)`
/// is a compile error. That seals the builder's typed path, including aliases,
/// re-bindings, helpers, and closures that receive this capability.
///
/// Resolver-layer sealing (#4627) closes the remaining gap: the public generic
/// `resolve_provider_binary("claude")` now scrubs `resolved_path` /
/// `canonical_path` / `exec_path` to `None` AND redacts the raw-path components
/// embedded in the `attempts` diagnostics, so no raw Claude path is reachable
/// through that seam (including by parsing `attempts`). The sole raw-path seam is
/// `binary_resolver::resolve_claude_binary_sealed`, which takes a [`ClaudeSeal`]
/// that only [`ClaudeBinary::resolve`] below can build.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeBinary {
    program: OsString,
}

impl std::fmt::Debug for ClaudeBinary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClaudeBinary(..)")
    }
}

/// Proof that the caller is this module; the private field keeps every other module from
/// building one, so only [`ClaudeBinary::resolve`] reaches the unscrubbed Claude resolver.
pub(crate) struct ClaudeSeal {
    _private: (),
}

impl ClaudeBinary {
    pub(crate) fn from_resolution(resolution: &BinaryResolution) -> Option<Self> {
        resolution.resolved_path.as_ref().map(|path| Self {
            program: OsString::from(path),
        })
    }

    pub(crate) fn resolve() -> Result<(Self, BinaryResolution), String> {
        // #4627: the sole sanctioned raw-path seam. The generic
        // `resolve_provider_binary("claude")` scrubs the raw path, so this
        // chokepoint uses the sealed resolver to obtain the unscrubbed
        // resolution that the guarded builder wraps.
        let resolution =
            crate::services::platform::binary_resolver::resolve_claude_binary_sealed(ClaudeSeal {
                _private: (),
            });
        let binary = Self::from_resolution(&resolution)
            .ok_or_else(|| "Claude CLI not found. Is Claude CLI installed?".to_string())?;
        Ok((binary, resolution))
    }

    pub(crate) fn from_tmux_wrapper_argv(program: &str) -> Self {
        Self {
            program: OsString::from(program),
        }
    }

    fn from_cli_boundary(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
        }
    }

    fn program(&self) -> &OsStr {
        &self.program
    }

    // The established wrapper/script contracts below require string argv egress.
    // These controlled conversions are not general path getters or raw-spawn
    // escape hatches.
    pub(crate) fn append_process_backend_wrapper_args(&self, args: &mut Vec<String>) {
        args.push("--".to_string());
        args.push(self.program.to_string_lossy().into_owned());
    }

    pub(crate) fn append_shell_escaped_to(&self, output: &mut String) {
        output.push_str(&crate::services::process::shell_escape(
            self.program.to_string_lossy().as_ref(),
        ));
    }

    pub(crate) fn augment_exec_path(&self, command: &mut Command) {
        crate::services::platform::augment_exec_path(command, Path::new(&self.program));
    }
}

/// By-construction builder for a Claude-launching `Command`.
///
/// The binary-resolution PATH (when the program is the Claude binary itself) is
/// applied the moment the builder is created. Callers finish configuring the
/// command through [`ClaudeCommandBuilder::command_mut`] (args, cwd, env,
/// stdio, process group) and extract it with
/// [`ClaudeCommandBuilder::into_command`].
pub(crate) struct ClaudeCommandBuilder {
    command: Command,
}

impl ClaudeCommandBuilder {
    /// Shared construction path for every builder flavour.
    fn build(program: impl AsRef<OsStr>, resolution: Option<&BinaryResolution>) -> Self {
        let mut command = Command::new(program);
        if let Some(resolution) = resolution {
            crate::services::platform::apply_binary_resolution(&mut command, resolution);
        }
        Self { command }
    }

    /// Build a command that launches the Claude binary directly. Applies the
    /// binary-resolution PATH by construction.
    pub(crate) fn for_binary(binary: &ClaudeBinary, resolution: &BinaryResolution) -> Self {
        Self::build(binary.program(), Some(resolution))
    }

    /// Build the native Claude `--version` probe from a resolved capability.
    /// The generic platform resolver owns candidate discovery; production Claude
    /// launches enter this builder only after the candidate is wrapped.
    pub(crate) fn for_resolved_version_probe(
        binary: &ClaudeBinary,
        resolution: &BinaryResolution,
    ) -> Self {
        Self::build(binary.program(), Some(resolution))
    }

    /// Build the native Claude version-smoke probe. This uses the CLI boundary
    /// string only after the provider discriminator selected `claude`.
    pub(crate) fn for_version_smoke(program: &str, canonical_path: &str) -> Self {
        let binary = ClaudeBinary::from_cli_boundary(program);
        let canonical = ClaudeBinary::from_cli_boundary(canonical_path);
        let mut builder = Self::build(binary.program(), None);
        canonical.augment_exec_path(builder.command_mut());
        builder
    }

    /// Build a command that launches the Claude binary delivered by the
    /// `agentdesk tmux-wrapper` boundary. The wrapper boundary is the sole
    /// untyped CLI argv ingress; once it is wrapped, the path cannot escape this
    /// module. Applies the exec-path PATH by construction.
    pub(crate) fn for_tmux_wrapper_argv(program: &str) -> Self {
        let binary = ClaudeBinary::from_tmux_wrapper_argv(program);
        let mut builder = Self::build(binary.program(), None);
        binary.augment_exec_path(builder.command_mut());
        builder
    }

    /// Mutable access to the wrapped command for site-specific configuration
    /// (args, cwd, env, stdio, process group). The binary-resolution PATH is
    /// already applied.
    pub(crate) fn command_mut(&mut self) -> &mut Command {
        &mut self.command
    }

    /// Consume the builder and return the fully-guarded `Command`.
    pub(crate) fn into_command(self) -> Command {
        self.command
    }
}
