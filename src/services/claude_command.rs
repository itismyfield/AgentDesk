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

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;
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
/// through that seam (including by parsing `attempts`). The sole sanctioned
/// raw-path seam is `binary_resolver::resolve_claude_binary_sealed`, consumed
/// only by [`ClaudeBinary::resolve`] below; `FORBIDDEN_RAW_SPAWN` remains
/// defense-in-depth.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeBinary {
    program: OsString,
}

impl std::fmt::Debug for ClaudeBinary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClaudeBinary(..)")
    }
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
        let resolution = crate::services::platform::binary_resolver::resolve_claude_binary_sealed();
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

    pub(crate) fn append_claude_e_bin_arg(&self, args: &mut Vec<String>) {
        args.push("--claude-bin".to_string());
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

/// Marker env var that managed dcserver callers (the legacy tmux-wrapper launch
/// script and the ProcessBackend wrapper) set on the `agentdesk tmux-wrapper`
/// process. The wrapper runs as a separate process with no installed config, so
/// it cannot itself run the config-gated [`ClaudeLaunchEnv::resolve`]. Its
/// managed parent — which *does* have config — resolves the gateway decision and
/// applies it to the wrapper's environment; this marker tells the wrapper that
/// an authority already decided, so it reconstructs that decision from the
/// inherited env instead of re-resolving (which, config-less, would collapse to
/// Scrub and strip a managed Inject). A direct public-CLI invocation carries no
/// marker, so the wrapper resolves fresh (Scrub without config — the safe native
/// default that also strips any stale gateway env from the operator's shell).
pub(crate) const TMUX_WRAPPER_GATEWAY_RESOLVED_ENV: &str = "AGENTDESK_CLAUDE_GATEWAY_RESOLVED";

/// Resolved launch environment for a single Claude spawn.
///
/// This is the only value that carries the gateway Inject|Scrub decision to a
/// launch site. It is produced solely by [`ClaudeLaunchEnv::resolve`] /
/// [`ClaudeLaunchEnv::for_tmux_wrapper`] (or the test-only constructors) so the
/// resolution policy is centralised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeLaunchEnv {
    gateway: ClaudeGatewayProxyEnv,
}

impl ClaudeLaunchEnv {
    /// Resolve the gateway launch env for the given intent. This is the single
    /// place that maps a launch intent onto the gateway Inject|Scrub decision.
    pub(crate) fn resolve(intent: ClaudeLaunchIntent) -> Self {
        Self {
            gateway: resolve_gateway(
                intent,
                crate::services::claude_gateway_proxy::resolve_for_launch,
            ),
        }
    }

    /// Launch env for the `agentdesk tmux-wrapper` process's own Claude spawn.
    ///
    /// The wrapper is a config-less separate process, so the decision authority
    /// depends on who launched it:
    ///   * managed caller (marker present) — the parent resolved with config and
    ///     applied the decision to this process's env; reconstruct and re-apply
    ///     it (idempotent: Inject with the inherited base URL, or Scrub).
    ///   * public CLI (no marker) — resolve fresh as a `Turn`; with no installed
    ///     config that is Scrub, stripping any stale gateway env.
    pub(crate) fn for_tmux_wrapper() -> Self {
        Self {
            gateway: tmux_wrapper_gateway(
                std::env::var_os(TMUX_WRAPPER_GATEWAY_RESOLVED_ENV).is_some(),
                crate::services::claude_gateway_proxy::reconstruct_launch_env_from_process,
                crate::services::claude_gateway_proxy::resolve_for_launch,
            ),
        }
    }

    /// Apply the resolved gateway env to a `Command` (Inject sets the proxy
    /// vars, Scrub removes any inherited values). Used by launch sites that
    /// build a `Command` outside [`ClaudeCommandBuilder`] (e.g. the wrapper
    /// command assembled inside `session_backend`).
    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        self.gateway.apply_to_command(command);
    }

    /// Apply this launch env to a **managed ProcessBackend** (pipe-mode) wrapper
    /// `Command`: the resolved gateway decision, then the managed-launch marker
    /// ([`mark_managed_launch_command`]) that tells the spawned
    /// `agentdesk tmux-wrapper` to *reconstruct* this decision rather than
    /// re-resolve config-less to a bare Scrub. Folding both steps here (rather
    /// than leaving them as two lines in the `session_backend` launch closure)
    /// makes the Command-leg marker wiring mutation-testable: deleting the
    /// `mark_managed_launch_command` call below fails
    /// `managed_process_command_marks_wrapper_env`.
    pub(crate) fn apply_to_managed_process_command(&self, command: &mut Command) {
        self.apply_to_command(command);
        mark_managed_launch_command(command);
    }

    /// Render the resolved gateway env as `export`/`unset` shell lines for
    /// launch sites that write a bash launch script rather than spawning a
    /// `Command` directly (Claude-TUI launch script, legacy tmux wrapper).
    pub(crate) fn append_shell_env(&self, output: &mut String) {
        self.gateway.append_shell_env(output);
    }

    /// Borrow the resolved gateway decision this launch env carries.
    ///
    /// The `claude_compact_context` launch-provenance / auto-compact-window
    /// helpers (#4591) read the effective Inject|Scrub decision to record pane
    /// provenance and derive the immutable launch window. They are pure
    /// consumers of the *already-resolved* decision (they never launch), so
    /// exposing it here keeps [`ClaudeLaunchEnv`] the single carrier: those
    /// sites obtain the gateway env through this chokepoint value rather than
    /// re-resolving it themselves.
    pub(crate) fn gateway_proxy_env(&self) -> &ClaudeGatewayProxyEnv {
        &self.gateway
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

/// Map a launch intent onto a gateway decision. `turn_gateway` supplies the
/// config-gated `Turn` decision (production: `resolve_for_launch`). Factored out
/// of [`ClaudeLaunchEnv::resolve`] so the `VersionProbe => Scrub` arm can be
/// mutation-tested against a *distinguishable* `Turn` decision without touching
/// process-global config (in unit tests `resolve_for_launch` collapses to Scrub,
/// which would otherwise hide a flip of the probe arm).
fn resolve_gateway(
    intent: ClaudeLaunchIntent,
    turn_gateway: impl FnOnce() -> ClaudeGatewayProxyEnv,
) -> ClaudeGatewayProxyEnv {
    match intent {
        ClaudeLaunchIntent::Turn => turn_gateway(),
        ClaudeLaunchIntent::VersionProbe => ClaudeGatewayProxyEnv::Scrub,
    }
}

/// Choose the tmux-wrapper's gateway decision. Factored out of
/// [`ClaudeLaunchEnv::for_tmux_wrapper`] so the managed-vs-public branch is
/// testable without mutating the process environment: a managed marker means an
/// upstream authority already decided (reconstruct it), otherwise resolve fresh.
fn tmux_wrapper_gateway(
    managed_marker_present: bool,
    reconstruct: impl FnOnce() -> ClaudeGatewayProxyEnv,
    resolve_fresh: impl FnOnce() -> ClaudeGatewayProxyEnv,
) -> ClaudeGatewayProxyEnv {
    if managed_marker_present {
        reconstruct()
    } else {
        resolve_fresh()
    }
}

/// Stamp the managed-launch marker ([`TMUX_WRAPPER_GATEWAY_RESOLVED_ENV`]) that
/// tells the `agentdesk tmux-wrapper` process the gateway decision now in this
/// environment was resolved by a config-holding dcserver authority, so the
/// wrapper reconstructs it rather than re-resolving to a bare Scrub. Shared by
/// the two managed launch sites (legacy tmux launch script + ProcessBackend) so
/// the marker literal lives in exactly one place next to the const it uses.
pub(crate) fn append_managed_launch_marker_shell(output: &mut String) {
    output.push_str(&format!("export {TMUX_WRAPPER_GATEWAY_RESOLVED_ENV}=1\n"));
}

/// `Command` counterpart of [`append_managed_launch_marker_shell`] for the
/// ProcessBackend wrapper launch path.
pub(crate) fn mark_managed_launch_command(command: &mut Command) {
    command.env(TMUX_WRAPPER_GATEWAY_RESOLVED_ENV, "1");
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
        binary: &ClaudeBinary,
        resolution: &BinaryResolution,
        intent: ClaudeLaunchIntent,
    ) -> Self {
        Self::build(
            binary.program(),
            Some(resolution),
            ClaudeLaunchEnv::resolve(intent),
        )
    }

    /// Binary-launch variant of [`for_binary`] that threads in an
    /// already-resolved launch env instead of resolving it from an intent.
    ///
    /// The direct-stream native launch (#4591) resolves the launch env once so
    /// the same gateway decision drives both the auto-compact-window
    /// computation and the by-construction gateway guard; resolving twice could
    /// probe the proxy twice and disagree. Applies the binary-resolution PATH
    /// plus the supplied launch env through the shared `build` guard arm.
    pub(crate) fn for_binary_with_env(
        binary: &ClaudeBinary,
        resolution: &BinaryResolution,
        launch_env: ClaudeLaunchEnv,
    ) -> Self {
        Self::build(binary.program(), Some(resolution), launch_env)
    }

    /// Build a command that launches a wrapper program which transitively
    /// spawns Claude (`agentdesk tmux-wrapper …`, `claude-e …`). The gateway env
    /// is applied by construction to the wrapper and the wrapped Claude child
    /// inherits it. The binary-resolution PATH is supplied separately by the
    /// caller because the wrapper — not the Claude binary — is the program here.
    pub(crate) fn for_wrapper(program: impl AsRef<OsStr>, intent: ClaudeLaunchIntent) -> Self {
        Self::build(program, None, ClaudeLaunchEnv::resolve(intent))
    }

    /// Wrapper-launch variant of [`for_wrapper`] that threads in an
    /// already-resolved launch env instead of resolving it from an intent.
    ///
    /// The ProcessBackend `claude-e` launch (#4591) must resolve the launch env
    /// exactly once up front so the *same* gateway decision drives both the
    /// auto-compact-window computation and the by-construction gateway guard;
    /// resolving twice could probe the proxy twice and disagree. As with
    /// [`for_wrapper`], no binary-resolution PATH is applied (the wrapper — not
    /// the Claude binary — is the program), and the gateway env is still applied
    /// through the shared `build` guard arm.
    pub(crate) fn for_wrapper_with_env(
        program: impl AsRef<OsStr>,
        launch_env: ClaudeLaunchEnv,
    ) -> Self {
        Self::build(program, None, launch_env)
    }

    /// Build the native Claude `--version` probe from a resolved capability.
    /// The generic platform resolver owns candidate discovery; production Claude
    /// launches enter this builder only after the candidate is wrapped.
    pub(crate) fn for_resolved_version_probe(
        binary: &ClaudeBinary,
        resolution: &BinaryResolution,
    ) -> Self {
        Self::build(
            binary.program(),
            Some(resolution),
            ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::VersionProbe),
        )
    }

    /// Build the native Claude version-smoke probe. This uses the CLI boundary
    /// string only after the provider discriminator selected `claude`.
    pub(crate) fn for_version_smoke(program: &str, canonical_path: &str) -> Self {
        let binary = ClaudeBinary::from_cli_boundary(program);
        let canonical = ClaudeBinary::from_cli_boundary(canonical_path);
        let mut builder = Self::build(
            binary.program(),
            None,
            ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::VersionProbe),
        );
        canonical.augment_exec_path(builder.command_mut());
        builder
    }

    /// Build a command that launches the Claude binary delivered by the
    /// `agentdesk tmux-wrapper` boundary. The wrapper boundary is the sole
    /// untyped CLI argv ingress; once it is wrapped, the path cannot escape this
    /// module. Applies the exec-path PATH plus the supplied (already-resolved)
    /// launch env by construction.
    pub(crate) fn for_tmux_wrapper_argv(program: &str, launch_env: ClaudeLaunchEnv) -> Self {
        let binary = ClaudeBinary::from_tmux_wrapper_argv(program);
        let mut builder = Self::build(binary.program(), None, launch_env);
        binary.augment_exec_path(builder.command_mut());
        builder
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

    #[cfg(test)]
    fn build_for_binary_test(
        binary: &ClaudeBinary,
        resolution: Option<&BinaryResolution>,
        launch_env: ClaudeLaunchEnv,
    ) -> Self {
        Self::build(binary.program(), resolution, launch_env)
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

    fn claude_binary() -> ClaudeBinary {
        ClaudeBinary::from_tmux_wrapper_argv("claude")
    }

    // Mutation target: `ClaudeCommandBuilder::build` applies the gateway env via
    // `launch_env.apply_to_command`. Every Claude spawn site funnels through
    // `build` (via `for_binary` / `for_wrapper`), so deleting that single line
    // makes ALL of these assertions fail — proving the guard is applied by
    // construction rather than remembered per site.

    #[test]
    fn for_binary_injects_gateway_env_by_construction() {
        let resolution = claude_resolution();
        let binary = claude_binary();
        let builder = ClaudeCommandBuilder::build_for_binary_test(
            &binary,
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
        let binary = claude_binary();
        let builder = ClaudeCommandBuilder::build_for_binary_test(
            &binary,
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
    fn tmux_wrapper_argv_applies_launch_env_by_construction() {
        // The tmux-wrapper constructor: gateway env applied through the shared
        // `build` guard arm (removing it fails this), PATH added on top.
        let builder = ClaudeCommandBuilder::for_tmux_wrapper_argv(
            "/opt/claude/bin/claude",
            ClaudeLaunchEnv::inject_for_test("http://127.0.0.1:10100"),
        );
        let envs = command_env_map(&builder.into_command());
        assert_eq!(
            envs.get(BASE_URL_ENV),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&Some("1".to_string())));
        // exec-path PATH is derived from the binary path (augment_exec_path).
        assert!(envs.get("PATH").is_some());
    }

    fn inject(url: &str) -> ClaudeGatewayProxyEnv {
        ClaudeGatewayProxyEnv::Inject {
            base_url: url.to_string(),
        }
    }

    fn scrub() -> ClaudeGatewayProxyEnv {
        ClaudeGatewayProxyEnv::Scrub
    }

    #[test]
    fn version_probe_arm_ignores_the_turn_gateway_decision() {
        // Real mutation coverage for `resolve_gateway`'s probe arm: feed a Turn
        // gateway that is DISTINGUISHABLE from Scrub (Inject) so flipping
        // `VersionProbe => Scrub` to `=> turn_gateway()` is detectable — unlike
        // the config-less unit-test `resolve_for_launch`, which is itself Scrub.
        let turn_gateway = || inject("http://turn.example/");
        let probe = resolve_gateway(ClaudeLaunchIntent::VersionProbe, turn_gateway);
        assert_eq!(probe, scrub());

        let turn_gateway = || inject("http://turn.example/");
        let turn = resolve_gateway(ClaudeLaunchIntent::Turn, turn_gateway);
        assert_eq!(turn, inject("http://turn.example/"));
    }

    #[test]
    fn tmux_wrapper_marker_present_reconstructs_managed_decision() {
        // Managed marker present → reconstruct the authority's decision
        // (idempotent), and IGNORE the fresh-resolve path. Inverting the branch
        // in `tmux_wrapper_gateway` would return Scrub here.
        let decision = tmux_wrapper_gateway(true, || inject("http://managed/"), scrub);
        assert_eq!(decision, inject("http://managed/"));
    }

    #[test]
    fn tmux_wrapper_without_marker_resolves_fresh_and_ignores_stale() {
        // Public CLI (no marker) → resolve fresh; the (stale) reconstruct path
        // is IGNORED so a stale inherited ANTHROPIC_BASE_URL cannot leak through.
        let decision = tmux_wrapper_gateway(false, || inject("http://stale/"), scrub);
        assert_eq!(decision, scrub());
    }

    // Mutation coverage for the ProcessBackend (pipe-mode) managed launch. The
    // #4559 R-B finding was that `mark_managed_launch_command` on the Command
    // leg had NO test — deleting it left every test green. This exercises the
    // exact production wiring (`apply_to_managed_process_command`, which the
    // `session_backend` launch closure calls) with a real resolved launch env —
    // it does NOT stamp the marker directly. Deleting the
    // `mark_managed_launch_command` call in that method drops the marker entry
    // and fails the marker assertion; deleting `apply_to_command` fails the
    // gateway assertion.
    #[test]
    fn managed_process_command_marks_wrapper_env() {
        let mut command = Command::new("agentdesk");
        ClaudeLaunchEnv::inject_for_test("http://managed.proxy/")
            .apply_to_managed_process_command(&mut command);
        let envs = command_env_map(&command);
        // Gateway decision is applied to the wrapper Command…
        assert_eq!(
            envs.get(BASE_URL_ENV),
            Some(&Some("http://managed.proxy/".to_string()))
        );
        assert_eq!(envs.get(DISCOVERY_ENV), Some(&Some("1".to_string())));
        // …and the managed marker is stamped so the wrapper reconstructs rather
        // than re-resolving config-less to a bare Scrub.
        assert_eq!(
            envs.get(TMUX_WRAPPER_GATEWAY_RESOLVED_ENV),
            Some(&Some("1".to_string())),
            "managed ProcessBackend launch must mark the wrapper env"
        );
    }

    // Scrub counterpart: even when the managed authority decided Scrub (proxy
    // disabled/unreachable), the marker is STILL stamped so the wrapper knows an
    // authority already decided and reconstructs the Scrub instead of resolving
    // fresh. Guards against a "only mark on Inject" regression.
    #[test]
    fn managed_process_command_marks_wrapper_env_even_when_scrubbed() {
        let mut command = Command::new("agentdesk");
        ClaudeLaunchEnv::scrub_for_test().apply_to_managed_process_command(&mut command);
        let envs = command_env_map(&command);
        assert_eq!(envs.get(BASE_URL_ENV), Some(&None));
        assert_eq!(
            envs.get(TMUX_WRAPPER_GATEWAY_RESOLVED_ENV),
            Some(&Some("1".to_string()))
        );
    }
}

#[cfg(test)]
mod chokepoint_guard_tests {
    //! Text-heuristic chokepoint guard. It catches direct primitives and
    //! function-item aliases, but cannot fully detect direct `Command::env`
    //! gateway setup or raw spawns hidden by renamed bindings. Those cases need
    //! an AST-based follow-up, in the same family as the raw-spawn limitation
    //! self-disclosed below.

    use std::path::{Path, PathBuf};

    /// Exact crate-relative definition sites permitted to define or apply the raw
    /// gateway primitives. These are authorities, not consumer exemptions.
    const DEFINITION_SITES: &[&str] = &[
        "src/services/claude_gateway_proxy.rs",
        "src/services/claude_command.rs",
    ];

    /// The sole sanctioned consumer path. Full-path matching prevents a file with
    /// the same basename in another directory from inheriting this exemption.
    const SANCTIONED: &[&str] = &["src/services/claude_compact_context.rs"];

    /// Substrings whose presence outside the chokepoint definition sites signals
    /// a launch site reaching around the chokepoint. The gateway type and module
    /// path are permitted only for the narrow sanctioned consumer above.
    const FORBIDDEN_PRIMITIVES: &[&str] = &[
        "ClaudeGatewayProxyEnv",
        "resolve_for_launch",
        "claude_gateway_proxy::",
    ];

    /// Behaviors that remain forbidden even in the sanctioned consumer. The
    /// accessor-chain needles close the method-syntax bypass where the concrete
    /// gateway type is absent from the call expression; path needles catch
    /// function-item aliases that omit the call's opening parenthesis.
    const FORBIDDEN_CONSUMER_BEHAVIORS: &[&str] = &[
        "resolve_for_launch",
        "::resolve_for_launch",
        "ClaudeGatewayProxyEnv::apply_to_command",
        "ClaudeGatewayProxyEnv::append_shell_env",
        "apply_to_command(",
        "append_shell_env(",
        ".gateway_proxy_env().apply",
        ".gateway_proxy_env().append",
    ];

    /// Defense-in-depth text guard for raw `Command::new(<claude binary var>)`
    /// idioms. The primary defense is [`ClaudeBinary`]: its private path field
    /// and deliberately absent `AsRef<OsStr>`/`Deref` implementations make a
    /// resolved Claude binary unusable with `Command::new`, including through
    /// aliases, re-bindings, helpers, and closures. This scan remains as a cheap
    /// regression tripwire for raw string argv received at the public wrapper
    /// boundary.
    const FORBIDDEN_RAW_SPAWN: &[&str] = &[
        "Command::new(claude_bin",
        "Command::new(&claude_bin",
        "Command::new(claude_e_bin",
        "Command::new(&claude_e_bin",
        // Full-path `std::process::Command::new(...)` equivalents. A caller could
        // otherwise dodge the bare-`Command::new(` literals above by spelling out
        // the module path. This is a cheap breadth extension only; it does NOT
        // close the class (a `let p = claude_bin; Command::new(p)` or a renamed
        // binding still slips through). The by-construction (type/AST) promotion
        // is tracked as the #4559 raw-spawn follow-up.
        "std::process::Command::new(claude_bin",
        "std::process::Command::new(&claude_bin",
        "std::process::Command::new(claude_e_bin",
        "std::process::Command::new(&claude_e_bin",
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

    fn crate_relative_path(file: &Path) -> Option<String> {
        file.strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    }

    /// All `.rs` files under the crate's `src/` tree. Scans the WHOLE crate (not
    /// just `src/services`) because the gateway primitives are `pub(crate)` and
    /// reachable from `src/cli` / `src/server` too — R-B's bypass was invoked via
    /// a public CLI subcommand in `src/cli`.
    fn crate_sources() -> Vec<PathBuf> {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&src_dir, &mut files);
        assert!(
            !files.is_empty(),
            "guard scan found no source files under {}",
            src_dir.display()
        );
        files
    }

    fn gateway_primitive_violations(relative_path: &str, contents: &str) -> Vec<String> {
        if DEFINITION_SITES.contains(&relative_path) {
            return Vec::new();
        }

        let needles = if SANCTIONED.contains(&relative_path) {
            FORBIDDEN_CONSUMER_BEHAVIORS
        } else {
            FORBIDDEN_PRIMITIVES
        };
        needles
            .iter()
            .filter(|needle| contents.contains(**needle))
            .map(|needle| format!("{relative_path} references `{needle}`"))
            .collect()
    }

    /// Read the crate's actual source set into the same path/content entry form
    /// consumed by the scanner. Keeping collection separate lets synthetic tests
    /// exercise the production scan loop without filesystem setup.
    fn crate_source_entries() -> Vec<(String, String)> {
        crate_sources()
            .into_iter()
            .filter_map(|file| {
                let relative_path = crate_relative_path(&file)?;
                let contents = std::fs::read_to_string(file).ok()?;
                Some((relative_path, contents))
            })
            .collect()
    }

    /// Run the primitive classifier over path/content entries. Both the full-crate
    /// guard and mutation tests use this pipeline, so test fixtures cannot bypass
    /// the path classification that selects a sanctioned consumer exemption.
    fn scan_gateway_primitive_sources<'a>(
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<String> {
        sources
            .into_iter()
            .flat_map(|(relative_path, contents)| {
                gateway_primitive_violations(relative_path, contents)
            })
            .collect()
    }

    fn raw_spawn_violations(relative_path: &str, contents: &str) -> Vec<String> {
        if DEFINITION_SITES.contains(&relative_path) {
            return Vec::new();
        }

        FORBIDDEN_RAW_SPAWN
            .iter()
            .filter(|needle| contents.contains(**needle))
            .map(|needle| format!("{relative_path} raw-spawns via `{needle}…)`"))
            .collect()
    }

    fn scan_raw_spawn_sources<'a>(
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<String> {
        sources
            .into_iter()
            .flat_map(|(relative_path, contents)| raw_spawn_violations(relative_path, contents))
            .collect()
    }

    /// By-construction guard. Fails if any module other than the sanctioned
    /// definition sites references raw gateway launch primitives. The compact
    /// context consumer may inspect only the already-resolved gateway decision;
    /// resolution and application remain confined to this chokepoint.
    #[test]
    fn gateway_primitives_are_confined_to_the_chokepoint() {
        let sources = crate_source_entries();
        let violations = scan_gateway_primitive_sources(
            sources
                .iter()
                .map(|(relative_path, contents)| (relative_path.as_str(), contents.as_str())),
        );

        assert!(
            violations.is_empty(),
            "gateway launch primitives leaked outside the chokepoint \
             (route these through claude_command::ClaudeCommandBuilder / ClaudeLaunchEnv):\n{}",
            violations.join("\n")
        );
    }

    /// Defense-in-depth regression tripwire for the specific raw-spawn spelling
    /// R-B found. `ClaudeBinary` is the primary by-construction guard; this scan
    /// stays intentionally narrow to catch an untyped wrapper-argv regression.
    #[test]
    fn claude_binaries_are_not_raw_spawned_outside_the_chokepoint() {
        let sources = crate_source_entries();
        let violations = scan_raw_spawn_sources(
            sources
                .iter()
                .map(|(relative_path, contents)| (relative_path.as_str(), contents.as_str())),
        );

        assert!(
            violations.is_empty(),
            "Claude/claude-e binary raw-spawned outside the chokepoint \
             (build it with claude_command::ClaudeCommandBuilder instead):\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn sanctioned_consumer_still_rejects_gateway_application_and_raw_spawns() {
        let consumer_source = r#"
            launch_env.gateway_proxy_env().apply_to_command(&mut command);
            launch_env.gateway_proxy_env().append_shell_env(&mut output);
            let direct_apply = value.apply_to_command(&mut command);
            let direct_append = value.append_shell_env(&mut output);
            let resolve = resolve_for_launch();
            let apply_item = ClaudeGatewayProxyEnv::apply_to_command;
            let append_item = ClaudeGatewayProxyEnv::append_shell_env;
            let resolve_item = crate::services::claude_gateway_proxy::resolve_for_launch;
        "#;
        let gateway_violations = scan_gateway_primitive_sources([(SANCTIONED[0], consumer_source)]);
        let raw_spawn_violations =
            scan_raw_spawn_sources([(SANCTIONED[0], "let command = Command::new(claude_bin);")]);

        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`.gateway_proxy_env().apply`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`.gateway_proxy_env().append`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`apply_to_command(`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`append_shell_env(`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`resolve_for_launch`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`ClaudeGatewayProxyEnv::apply_to_command`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`ClaudeGatewayProxyEnv::append_shell_env`"))
        );
        assert!(
            gateway_violations
                .iter()
                .any(|violation| violation.contains("`::resolve_for_launch`"))
        );
        assert!(!raw_spawn_violations.is_empty());
    }

    #[test]
    fn scanner_pipeline_keeps_sanctioned_consumer_exemption_path_exact() {
        let consumer_source = r#"
            use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;
            use crate::services::claude_command::ClaudeLaunchEnv;
            impl From<&ClaudeGatewayProxyEnv> for ClaudeLaunchProvenance {}
            fn consume(gateway: &ClaudeGatewayProxyEnv, launch: &ClaudeLaunchEnv) {}
        "#;
        let matching_basename_outside_sanctioned_path = "src/other/claude_compact_context.rs";
        let violations = scan_gateway_primitive_sources([
            (SANCTIONED[0], consumer_source),
            (matching_basename_outside_sanctioned_path, consumer_source),
        ]);

        assert!(
            !violations
                .iter()
                .any(|violation| violation.starts_with(SANCTIONED[0])),
            "the exact sanctioned consumer may inspect the resolved gateway type"
        );
        // If SANCTIONED matching is regressed to a basename comparison, this
        // assert fails: the second entry inherits the exemption and produces no
        // violation despite not being the one sanctioned path.
        assert!(
            violations.iter().any(|violation| {
                violation.starts_with(matching_basename_outside_sanctioned_path)
                    && violation.contains("`ClaudeGatewayProxyEnv`")
            }),
            "a matching basename outside the exact consumer path must not be sanctioned"
        );
    }

    /// #4627: crate-relative sites permitted to define or call the sealed
    /// raw-path Claude resolver seam. The definition lives in `binary_resolver.rs`
    /// and the sole caller is `ClaudeBinary::resolve` in `claude_command.rs`; every
    /// other module must obtain Claude paths through the scrubbing generic
    /// `resolve_provider_binary`.
    const SEALED_CLAUDE_SEAM_SITES: &[&str] = &[
        "src/services/platform/binary_resolver.rs",
        "src/services/claude_command.rs",
    ];

    /// The sealed-seam symbol. Any crate-relative source outside
    /// [`SEALED_CLAUDE_SEAM_SITES`] that names it is reaching around the scrubbing
    /// generic resolver for a raw Claude path.
    const SEALED_CLAUDE_SEAM_NEEDLE: &str = "resolve_claude_binary_sealed";

    fn sealed_claude_seam_violations(relative_path: &str, contents: &str) -> Vec<String> {
        if SEALED_CLAUDE_SEAM_SITES.contains(&relative_path) {
            return Vec::new();
        }
        if contents.contains(SEALED_CLAUDE_SEAM_NEEDLE) {
            vec![format!(
                "{relative_path} references sealed Claude seam `{SEALED_CLAUDE_SEAM_NEEDLE}`"
            )]
        } else {
            Vec::new()
        }
    }

    fn scan_sealed_claude_seam_sources<'a>(
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<String> {
        sources
            .into_iter()
            .flat_map(|(relative_path, contents)| {
                sealed_claude_seam_violations(relative_path, contents)
            })
            .collect()
    }

    /// By-construction guard: the sealed raw-path Claude seam must be referenced
    /// only by its definition site and the single sanctioned chokepoint caller.
    #[test]
    fn sealed_claude_seam_confined_to_chokepoint() {
        let sources = crate_source_entries();
        let violations = scan_sealed_claude_seam_sources(
            sources
                .iter()
                .map(|(relative_path, contents)| (relative_path.as_str(), contents.as_str())),
        );

        assert!(
            violations.is_empty(),
            "the sealed Claude resolver seam leaked outside its sanctioned sites \
             (obtain Claude paths through platform::resolve_provider_binary, which \
             scrubs them):\n{}",
            violations.join("\n")
        );
    }

    /// Mutation coverage for the sealed-seam guard: a foreign reference is flagged
    /// while the sanctioned sites stay exempt. Inverting the exemption check or
    /// broadening the site list is caught here.
    #[test]
    fn sealed_claude_seam_guard_flags_foreign_reference_only() {
        let foreign = scan_sealed_claude_seam_sources([(
            "src/services/other.rs",
            "let r = resolve_claude_binary_sealed();",
        )]);
        assert!(
            foreign
                .iter()
                .any(|violation| violation.contains(SEALED_CLAUDE_SEAM_NEEDLE)),
            "a foreign reference to the sealed seam must be flagged"
        );

        let sanctioned = scan_sealed_claude_seam_sources([
            (
                "src/services/claude_command.rs",
                "resolve_claude_binary_sealed();",
            ),
            (
                "src/services/platform/binary_resolver.rs",
                "pub(crate) fn resolve_claude_binary_sealed()",
            ),
        ]);
        assert!(
            sanctioned.is_empty(),
            "the definition site and sanctioned chokepoint caller must stay exempt"
        );
    }
}
