//! Single authority (chokepoint) for launching the Claude CLI.
//!
//! [`ClaudeCommandBuilder`] is the only sanctioned way to obtain a `Command`
//! that launches (or transitively spawns) the Claude CLI. Binary resolution is
//! applied when the builder is constructed, and [`ClaudeBinary`] seals the
//! resolved executable path so a caller physically cannot hand back a Claude
//! command assembled outside this module.
//!
//! The raw [`crate::services::claude_gateway_proxy`] primitives must be reached
//! ONLY through this module. A source-scanning guard test
//! (`chokepoint_guard_tests`) fails the build if any other module references
//! them directly, so the single authority cannot erode.

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

    /// Build a command that launches a wrapper program which transitively
    /// spawns Claude (`agentdesk tmux-wrapper …`, `claude-e …`). The
    /// binary-resolution PATH is supplied separately by the caller because the
    /// wrapper — not the Claude binary — is the program here.
    pub(crate) fn for_wrapper(program: impl AsRef<OsStr>) -> Self {
        Self::build(program, None)
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
