//! Platform-aware binary resolution.
//!
//! Provides a single resolution contract for provider CLIs across macOS,
//! Linux, and Windows.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::runtime_layout::expand_user_path;

const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION_PROBE_MAX_OUTPUT_BYTES: usize = 8 * 1024;
const SHELL_ENV_DELIMITER: &str = "__AGENTDESK_SHELL_ENV__";

thread_local! {
    static ACTIVE_PROVIDER_CONTEXTS: RefCell<Vec<crate::services::provider_cli::ProviderExecutionContext>> =
        const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryResolution {
    pub requested_binary: String,
    // #4627: the public generic resolver `resolve_provider_binary` scrubs these
    // path fields to `None` for the normalized `claude` provider, so a raw Claude
    // executable path is unreachable through the generic seam. The sole sanctioned
    // raw-path seam is `resolve_claude_binary_sealed` (consumed only by
    // `ClaudeBinary::resolve`); diagnostics below (source/attempts/failure_kind)
    // are preserved by the scrub.
    pub resolved_path: Option<String>,
    pub canonical_path: Option<String>,
    pub source: Option<String>,
    pub attempts: Vec<String>,
    pub failure_kind: Option<String>,
    pub exec_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryVersionProbe {
    pub resolution: BinaryResolution,
    pub version_output: Option<String>,
    pub probe_failure_kind: Option<String>,
    pub skipped_candidate_failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BinaryCandidate {
    resolved_path: PathBuf,
    source: String,
    discovery_index: usize,
    priority: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ParsedCliVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

#[derive(Clone, Debug)]
struct SuccessfulBinaryProbe {
    resolution: BinaryResolution,
    version_output: String,
    parsed_version: Option<ParsedCliVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BinaryProbeCacheKey {
    provider: String,
    candidates: Vec<BinaryProbeCandidateKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BinaryProbeCandidateKey {
    resolved_path: Option<String>,
    canonical_path: Option<String>,
    source: Option<String>,
    modified_ns: Option<u128>,
    len: Option<u64>,
}

impl BinaryProbeCacheKey {
    fn new(provider: &str, candidates: &[BinaryResolution]) -> Self {
        Self {
            provider: normalize_name(provider),
            candidates: candidates
                .iter()
                .map(BinaryProbeCandidateKey::new)
                .collect(),
        }
    }
}

impl BinaryProbeCandidateKey {
    fn new(candidate: &BinaryResolution) -> Self {
        let metadata = candidate
            .resolved_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok());
        let modified_ns = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        let len = metadata.as_ref().map(std::fs::Metadata::len);
        Self {
            resolved_path: candidate.resolved_path.clone(),
            canonical_path: candidate.canonical_path.clone(),
            source: candidate.source.clone(),
            modified_ns,
            len,
        }
    }
}

fn probed_binary_cache() -> &'static Mutex<HashMap<BinaryProbeCacheKey, BinaryVersionProbe>> {
    static CACHE: OnceLock<Mutex<HashMap<BinaryProbeCacheKey, BinaryVersionProbe>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn git_binary() -> &'static OsString {
    static GIT_BINARY: OnceLock<OsString> = OnceLock::new();
    GIT_BINARY.get_or_init(|| {
        for key in ["AGENTDESK_TEST_GIT", "AGENTDESK_GIT"] {
            if let Some(configured) = std::env::var_os(key).filter(|value| !value.is_empty()) {
                return configured;
            }
        }
        #[cfg(windows)]
        {
            if let Ok(output) = Command::new(r"C:\Windows\System32\where.exe")
                .arg("git")
                .output()
            {
                if output.status.success() {
                    if let Some(path) = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                    {
                        return path.into();
                    }
                }
            }
            for candidate in [
                r"C:\Program Files\Git\cmd\git.exe",
                r"C:\Program Files\Git\bin\git.exe",
                r"C:\Program Files (x86)\Git\cmd\git.exe",
                r"C:\Program Files (x86)\Git\bin\git.exe",
            ] {
                if Path::new(candidate).exists() {
                    return candidate.into();
                }
            }
            "git.exe".into()
        }
        #[cfg(not(windows))]
        {
            "git".into()
        }
    })
}

pub fn git_command() -> Command {
    Command::new(git_binary())
}

pub fn resolve_binary_with_login_shell(name: &str) -> Option<String> {
    let cwd = current_dir_fallback();
    if let Some(path) = resolve_in_paths(name, std::env::var_os("PATH"), &cwd) {
        return Some(path.to_string_lossy().to_string());
    }
    if let Some(path) = resolve_in_paths(name, resolve_login_shell_path_os(), &cwd) {
        return Some(path.to_string_lossy().to_string());
    }
    resolve_in_paths(name, join_paths_lossy(standard_fallback_dirs()), &cwd)
        .map(|path| path.to_string_lossy().to_string())
}

/// Public generic provider-binary resolver.
///
/// #4627: for the normalized `claude` provider this scrubs the raw path fields
/// (`resolved_path` / `canonical_path` / `exec_path`) to `None` by construction,
/// and additionally redacts the raw path components embedded in the `attempts`
/// diagnostics (several attempt lines carry `Path::display()` output, so leaving
/// them intact would let a caller reconstruct the raw Claude path from
/// `attempts`). The diagnostic *structure* (`source` / `failure_kind` and each
/// attempt's non-path fields) is preserved. A caller therefore cannot obtain a
/// raw Claude executable path through this generic seam. The only sanctioned way
/// to reach the raw Claude path is [`resolve_claude_binary_sealed`], consumed
/// solely by `ClaudeBinary::resolve`. Non-Claude providers are returned
/// unchanged.
pub fn resolve_provider_binary(provider: &str) -> BinaryResolution {
    scrub_sealed_provider_paths(resolve_provider_binary_unsealed(provider))
}

/// Internal unsealed resolver: returns the full [`BinaryResolution`] including the
/// raw Claude path. Deliberately not `pub` — the only callers are the scrubbing
/// public wrapper above and the sanctioned [`resolve_claude_binary_sealed`] seam.
fn resolve_provider_binary_unsealed(provider: &str) -> BinaryResolution {
    match resolve_provider_binary_set(provider) {
        ProviderResolutionSet::Candidates(candidates) => match candidates.len() {
            0 => unresolved_provider_binary(normalize_name(provider), Vec::new()),
            1 => candidates.into_iter().next().unwrap(),
            _ => cached_probe_provider_binary_candidates(provider, candidates).resolution,
        },
        ProviderResolutionSet::Failure(failure) => failure,
    }
}

/// Sole sanctioned raw-path seam for the Claude binary.
///
/// #4627: `ClaudeBinary::resolve` is the only permitted caller (enforced by the
/// `sealed_claude_seam_confined_to_chokepoint` guard in `claude_command.rs`). It
/// returns the unscrubbed resolution so the guarded launch builder can wrap the
/// raw path; every other consumer must go through the generic
/// [`resolve_provider_binary`], which scrubs Claude paths.
pub(crate) fn resolve_claude_binary_sealed() -> BinaryResolution {
    resolve_provider_binary_unsealed("claude")
}

/// Marker substituted for any raw filesystem-path component when a Claude
/// resolution's diagnostics are redacted by [`scrub_sealed_provider_paths`].
const SEALED_PATH_MARKER: &str = "<sealed-path>";

/// Scrub the raw path fields of a Claude resolution while preserving diagnostics.
///
/// This is the by-construction seal for the generic public resolver: any
/// resolution whose normalized provider is `claude` has its `resolved_path`,
/// `canonical_path`, and `exec_path` set to `None`, and every raw-path component
/// embedded in its `attempts` lines redacted (see [`redact_paths_from_attempt`]).
/// Non-Claude resolutions pass through untouched.
fn scrub_sealed_provider_paths(mut resolution: BinaryResolution) -> BinaryResolution {
    if normalize_name(&resolution.requested_binary) == "claude" {
        resolution.resolved_path = None;
        resolution.canonical_path = None;
        resolution.exec_path = None;
        for attempt in &mut resolution.attempts {
            *attempt = redact_paths_from_attempt(attempt);
        }
    }
    resolution
}

/// Redact filesystem-path components from a single diagnostic attempt line while
/// keeping its structural fields (source label, `priority=N`, counts,
/// `version=…`, failure kind).
///
/// The resolver assembles every attempt as `:`-delimited fields and never embeds
/// a `:` inside an emitted path, so any field carrying a path separator (`/` or
/// `\`) is a path token and is replaced wholesale with [`SEALED_PATH_MARKER`].
/// This is deliberately over-inclusive (a `version=…` field that happens to
/// contain a separator is redacted too) because the seal's invariant — no raw
/// path survives in the generic seam's output — must hold regardless of which
/// attempt pattern produced the line (`env_override`, `selected_candidate`,
/// per-source `candidate`, `registry`, or the probe `skipped_candidate_*` /
/// `selected_candidate_version` lines).
fn redact_paths_from_attempt(attempt: &str) -> String {
    attempt
        .split(':')
        .map(|field| {
            if field.contains('/') || field.contains('\\') {
                SEALED_PATH_MARKER
            } else {
                field
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

enum ProviderResolutionSet {
    Candidates(Vec<BinaryResolution>),
    Failure(BinaryResolution),
}

fn resolve_provider_binary_set(provider: &str) -> ProviderResolutionSet {
    if let Some(ctx) = active_provider_context(provider) {
        let provider = normalize_name(provider);
        let resolution = resolve_provider_binary_for_context(&ctx);
        if resolution
            .source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry:"))
        {
            return ProviderResolutionSet::Candidates(vec![resolution]);
        }
        return match resolve_provider_binary_legacy_set(&provider) {
            LegacyProviderResolution::Candidates(candidates) => {
                ProviderResolutionSet::Candidates(candidates)
            }
            LegacyProviderResolution::Failure(failure) => ProviderResolutionSet::Failure(failure),
        };
    }

    match resolve_provider_binary_legacy_set(provider) {
        LegacyProviderResolution::Candidates(candidates) => {
            ProviderResolutionSet::Candidates(candidates)
        }
        LegacyProviderResolution::Failure(failure) => ProviderResolutionSet::Failure(failure),
    }
}

pub fn probe_provider_binary_version(provider: &str) -> BinaryVersionProbe {
    match resolve_provider_binary_set(provider) {
        ProviderResolutionSet::Candidates(candidates) => match candidates.len() {
            0 => probe_single_provider_resolution(
                unresolved_provider_binary(normalize_name(provider), Vec::new()),
                Vec::new(),
            ),
            1 => {
                probe_single_provider_resolution(candidates.into_iter().next().unwrap(), Vec::new())
            }
            _ => cached_probe_provider_binary_candidates(provider, candidates),
        },
        ProviderResolutionSet::Failure(failure) => {
            probe_single_provider_resolution(failure, Vec::new())
        }
    }
}

fn cached_probe_provider_binary_candidates(
    provider: &str,
    candidates: Vec<BinaryResolution>,
) -> BinaryVersionProbe {
    let key = BinaryProbeCacheKey::new(provider, &candidates);
    if let Some(cached) = probed_binary_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned())
    {
        return cached;
    }

    let probe = probe_provider_binary_candidates(candidates);
    if let Ok(mut cache) = probed_binary_cache().lock() {
        cache.insert(key, probe.clone());
    }
    probe
}

fn probe_provider_binary_candidates(candidates: Vec<BinaryResolution>) -> BinaryVersionProbe {
    let requested_binary = candidates
        .first()
        .map(|candidate| candidate.requested_binary.clone())
        .unwrap_or_else(|| "provider".to_string());
    let mut failed_candidates = Vec::new();
    let mut first_failed_probe = None;
    let mut successful_candidates = Vec::new();
    for resolution in candidates {
        let Some(resolved_path) = resolution.resolved_path.clone() else {
            continue;
        };
        let (version_output, probe_failure_kind) =
            probe_resolved_binary_version(std::path::Path::new(&resolved_path), &resolution);
        if let Some(version_output) = version_output
            .as_deref()
            .filter(|output| !output.lines().next().unwrap_or("").trim().is_empty())
        {
            if requested_binary == "codex" {
                successful_candidates.push(SuccessfulBinaryProbe {
                    resolution,
                    version_output: version_output.to_string(),
                    parsed_version: parse_cli_semver(version_output),
                });
                continue;
            }

            return selected_binary_probe(
                resolution,
                version_output.to_string(),
                Vec::new(),
                failed_candidates,
            );
        }

        if first_failed_probe.is_none() {
            first_failed_probe = Some(BinaryVersionProbe {
                resolution: resolution.clone(),
                version_output: version_output.clone(),
                probe_failure_kind: probe_failure_kind.clone(),
                skipped_candidate_failures: Vec::new(),
            });
        }

        let failure = format!(
            "{}:{}",
            resolved_path,
            probe_failure_kind
                .clone()
                .unwrap_or_else(|| "version_probe_empty".to_string())
        );
        failed_candidates.push(failure);
    }

    if !successful_candidates.is_empty() {
        let selected_index =
            select_successful_candidate_index(&requested_binary, &successful_candidates);
        let selected = successful_candidates.remove(selected_index);
        return selected_binary_probe(
            selected.resolution,
            selected.version_output,
            successful_candidates,
            failed_candidates,
        );
    }

    if let Some(probe) = first_failed_probe {
        return probe;
    }

    probe_single_provider_resolution(
        unresolved_provider_binary(requested_binary, Vec::new()),
        Vec::new(),
    )
}

fn selected_binary_probe(
    mut resolution: BinaryResolution,
    version_output: String,
    skipped_successful_candidates: Vec<SuccessfulBinaryProbe>,
    failed_candidates: Vec<String>,
) -> BinaryVersionProbe {
    for failure in &failed_candidates {
        resolution
            .attempts
            .push(format!("skipped_candidate_failure:{failure}"));
    }
    for skipped in &skipped_successful_candidates {
        resolution.attempts.push(format!(
            "skipped_candidate_success:{}:version={}",
            skipped
                .resolution
                .resolved_path
                .as_deref()
                .unwrap_or("<unknown>"),
            first_output_line(&skipped.version_output)
        ));
    }
    resolution.attempts.push(format!(
        "selected_candidate_version:{}:version={}",
        resolution.resolved_path.as_deref().unwrap_or("<unknown>"),
        first_output_line(&version_output)
    ));

    BinaryVersionProbe {
        resolution,
        version_output: Some(version_output),
        probe_failure_kind: None,
        skipped_candidate_failures: failed_candidates,
    }
}

fn select_successful_candidate_index(
    requested_binary: &str,
    candidates: &[SuccessfulBinaryProbe],
) -> usize {
    if requested_binary != "codex" {
        return 0;
    }

    let mut best: Option<(usize, ParsedCliVersion)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(version) = candidate.parsed_version else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(_, best_version)| version > *best_version)
        {
            best = Some((index, version));
        }
    }
    best.map(|(index, _)| index).unwrap_or(0)
}

fn parse_cli_semver(output: &str) -> Option<ParsedCliVersion> {
    for token in output.split_whitespace() {
        let token = token
            .trim_matches(|ch: char| {
                !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_')
            })
            .trim_start_matches('v');
        let version_part = token.split('-').next().unwrap_or(token);
        let mut parts = version_part.split('.');
        let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        let Ok(major) = major.parse::<u64>() else {
            continue;
        };
        let Ok(minor) = minor.parse::<u64>() else {
            continue;
        };
        let Ok(patch) = patch.parse::<u64>() else {
            continue;
        };
        return Some(ParsedCliVersion {
            major,
            minor,
            patch,
        });
    }
    None
}

fn first_output_line(output: &str) -> String {
    output.lines().next().unwrap_or(output).trim().to_string()
}

fn probe_single_provider_resolution(
    resolution: BinaryResolution,
    skipped_candidate_failures: Vec<String>,
) -> BinaryVersionProbe {
    let (version_output, probe_failure_kind) = resolution
        .resolved_path
        .as_ref()
        .map(|path| probe_resolved_binary_version(std::path::Path::new(path), &resolution))
        .unwrap_or((None, None));
    BinaryVersionProbe {
        resolution,
        version_output,
        probe_failure_kind,
        skipped_candidate_failures,
    }
}

fn resolve_provider_binary_legacy(provider: &str) -> BinaryResolution {
    match resolve_provider_binary_legacy_set(provider) {
        LegacyProviderResolution::Candidates(mut candidates) => candidates
            .drain(..)
            .next()
            .unwrap_or_else(|| unresolved_provider_binary(normalize_name(provider), Vec::new())),
        LegacyProviderResolution::Failure(failure) => failure,
    }
}

enum LegacyProviderResolution {
    Candidates(Vec<BinaryResolution>),
    Failure(BinaryResolution),
}

fn resolve_provider_binary_legacy_set(provider: &str) -> LegacyProviderResolution {
    let requested_binary = normalize_name(provider);
    let override_var = override_var_name(&requested_binary);
    let cwd = current_dir_fallback();
    let mut attempts = Vec::new();

    match std::env::var_os(&override_var).filter(|value| !os_value_is_empty(value)) {
        Some(raw_override) => {
            let expanded = expand_user_path(&raw_override.to_string_lossy())
                .unwrap_or_else(|| PathBuf::from(&raw_override));
            match resolve_candidate_path(&expanded, &cwd) {
                Ok(path) => {
                    attempts.push(format!(
                        "env_override:{}=found:{}",
                        override_var,
                        path.display()
                    ));
                    return finalize_resolution(
                        requested_binary,
                        path,
                        "env_override".to_string(),
                        attempts,
                    )
                    .into();
                }
                Err(error) => {
                    attempts.push(format!(
                        "env_override:{}=miss:{}:{}",
                        override_var,
                        expanded.display(),
                        error
                    ));
                    return LegacyProviderResolution::Failure(
                        unresolved_provider_binary_with_error(requested_binary, attempts, error),
                    );
                }
            }
        }
        None => attempts.push(format!("env_override:{}=unset", override_var)),
    }

    let mut candidates = Vec::new();
    let mut discovery_index = 0;
    let current_path = std::env::var_os("PATH").filter(|value| !os_value_is_empty(value));
    collect_candidate_source(
        &requested_binary,
        "current_path",
        current_path,
        "current_path=unset",
        &cwd,
        &mut attempts,
        &mut candidates,
        &mut discovery_index,
    );

    let login_path = resolve_login_shell_path_os().filter(|value| !os_value_is_empty(value));
    collect_candidate_source(
        &requested_binary,
        "login_shell_path",
        login_path,
        "login_shell_path=unavailable",
        &cwd,
        &mut attempts,
        &mut candidates,
        &mut discovery_index,
    );

    let fallback_dirs = provider_fallback_dirs(&requested_binary);
    let fallback_paths = join_paths_lossy(fallback_dirs.clone());
    let fallback_before = candidates.len();
    collect_candidate_source(
        &requested_binary,
        "fallback_path",
        fallback_paths,
        "fallback_path=unavailable",
        &cwd,
        &mut attempts,
        &mut candidates,
        &mut discovery_index,
    );
    if candidates.len() == fallback_before {
        attempts.push(format!("fallback_path=miss:{}dirs", fallback_dirs.len()));
    }

    let candidates = sorted_unique_candidates(candidates);
    if candidates.is_empty() {
        return LegacyProviderResolution::Failure(unresolved_provider_binary_with_error(
            requested_binary,
            attempts,
            "not_found".to_string(),
        ));
    }

    let resolutions = candidates
        .into_iter()
        .map(|candidate| {
            let mut candidate_attempts = attempts.clone();
            candidate_attempts.push(format!(
                "selected_candidate:{}:priority={}:{}",
                candidate.source,
                candidate.priority,
                candidate.resolved_path.display()
            ));
            finalize_resolution(
                requested_binary.clone(),
                candidate.resolved_path,
                candidate.source,
                candidate_attempts,
            )
        })
        .collect();
    LegacyProviderResolution::Candidates(resolutions)
}

impl From<BinaryResolution> for LegacyProviderResolution {
    fn from(value: BinaryResolution) -> Self {
        LegacyProviderResolution::Candidates(vec![value])
    }
}

fn unresolved_provider_binary(requested_binary: String, attempts: Vec<String>) -> BinaryResolution {
    unresolved_provider_binary_with_error(requested_binary, attempts, "not_found".to_string())
}

fn unresolved_provider_binary_with_error(
    requested_binary: String,
    attempts: Vec<String>,
    failure_kind: String,
) -> BinaryResolution {
    BinaryResolution {
        requested_binary,
        resolved_path: None,
        canonical_path: None,
        source: None,
        attempts,
        failure_kind: Some(failure_kind),
        exec_path: merged_runtime_path(),
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_candidate_source(
    requested_binary: &str,
    source: &str,
    paths: Option<OsString>,
    unavailable_attempt: &str,
    cwd: &Path,
    attempts: &mut Vec<String>,
    candidates: &mut Vec<BinaryCandidate>,
    discovery_index: &mut usize,
) {
    let Some(paths) = paths.filter(|value| !os_value_is_empty(value)) else {
        attempts.push(unavailable_attempt.to_string());
        return;
    };

    let found = resolve_all_in_paths(requested_binary, Some(paths), cwd);
    if found.is_empty() {
        attempts.push(format!("{source}=miss"));
        return;
    }

    attempts.push(format!("{source}=candidates:{}", found.len()));
    for path in found {
        let priority = candidate_priority(requested_binary, &path);
        attempts.push(format!(
            "{source}=candidate:priority={priority}:{}",
            path.display()
        ));
        candidates.push(BinaryCandidate {
            resolved_path: path,
            source: source.to_string(),
            discovery_index: *discovery_index,
            priority,
        });
        *discovery_index += 1;
    }
}

fn sorted_unique_candidates(candidates: Vec<BinaryCandidate>) -> Vec<BinaryCandidate> {
    let mut seen = BTreeSet::new();
    let mut unique = candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.resolved_path.to_string_lossy().to_string()))
        .collect::<Vec<_>>();
    unique.sort_by_key(|candidate| {
        (
            candidate.priority,
            candidate.discovery_index,
            candidate.resolved_path.to_string_lossy().to_string(),
        )
    });
    unique
}

pub fn candidate_priority(provider: &str, path: impl AsRef<Path>) -> u8 {
    let provider = normalize_name(provider);
    let path = normalized_path_text(path.as_ref());

    if path.contains("/.bun/bin/") || path.contains("/bun/bin/") {
        return 90;
    }
    if provider == "codex" && is_codex_app_bundle_resource_path(&path) {
        return 5;
    }
    if provider == "claude" && path.contains("/.claude/local/") {
        return 0;
    }
    if path.contains("/.volta/bin/")
        || path.contains("/.asdf/shims/")
        || path.contains("/.local/share/mise/shims/")
        || path.contains("/.local/share/rtx/shims/")
        || path.contains("/.npm-global/bin/")
        || path.contains("/.nvm/")
        || path.contains("/node_modules/.bin/")
        || path.contains("/library/pnpm/")
        || path.contains("/.local/share/pnpm/")
        || path.contains("/pnpm/")
    {
        return 10;
    }
    50
}

fn is_codex_app_bundle_resource_path(path: &str) -> bool {
    path.contains("/codex.app/contents/resources/")
}

fn normalized_path_text(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

struct ProviderContextScope;

impl ProviderContextScope {
    fn push(ctx: crate::services::provider_cli::ProviderExecutionContext) -> Self {
        ACTIVE_PROVIDER_CONTEXTS.with(|contexts| contexts.borrow_mut().push(ctx));
        Self
    }
}

impl Drop for ProviderContextScope {
    fn drop(&mut self) {
        ACTIVE_PROVIDER_CONTEXTS.with(|contexts| {
            contexts.borrow_mut().pop();
        });
    }
}

pub fn with_provider_execution_context<T>(
    ctx: crate::services::provider_cli::ProviderExecutionContext,
    run: impl FnOnce() -> T,
) -> T {
    let _scope = ProviderContextScope::push(ctx);
    run()
}

fn active_provider_context(
    provider: &str,
) -> Option<crate::services::provider_cli::ProviderExecutionContext> {
    ACTIVE_PROVIDER_CONTEXTS.with(|contexts| {
        contexts
            .borrow()
            .iter()
            .rev()
            .find(|ctx| ctx.provider.eq_ignore_ascii_case(provider))
            .cloned()
    })
}

pub fn merged_runtime_path() -> Option<String> {
    join_paths_lossy(runtime_path_entries()).map(|value| value.to_string_lossy().to_string())
}

pub fn apply_runtime_path(command: &mut Command) {
    if let Some(path) = merged_runtime_path() {
        command.env("PATH", path);
    }
}

pub fn augment_exec_path(command: &mut Command, binary_path: impl AsRef<Path>) {
    if let Some(path) = exec_path_for_binary(binary_path.as_ref()) {
        command.env("PATH", path);
    } else {
        apply_runtime_path(command);
    }
}

pub fn apply_binary_resolution(command: &mut Command, resolution: &BinaryResolution) {
    if let Some(path) = &resolution.exec_path {
        command.env("PATH", path);
    } else if let Some(path) = &resolution.resolved_path {
        augment_exec_path(command, path);
    } else {
        apply_runtime_path(command);
    }
}

fn drain_limited_output<R>(mut reader: R) -> Vec<u8>
where
    R: Read + Send + 'static,
{
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = VERSION_PROBE_MAX_OUTPUT_BYTES.saturating_sub(output.len());
                if remaining > 0 {
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    output
}

fn configure_version_probe_command(command: &mut Command, resolution: &BinaryResolution) {
    apply_binary_resolution(command, resolution);
    if resolution.requested_binary == "claude" {
        // `--version` never routes models or spawns subagents, so probes always
        // run native (Scrub). The gateway policy for this launch class lives in
        // the single chokepoint authority (`VersionProbe => Scrub`); turn
        // launches take the `Turn` intent there.
        crate::services::claude_command::ClaudeLaunchEnv::resolve(
            crate::services::claude_command::ClaudeLaunchIntent::VersionProbe,
        )
        .apply_to_command(command);
    }
}

pub fn probe_resolved_binary_version(
    binary_path: impl AsRef<OsStr>,
    resolution: &BinaryResolution,
) -> (Option<String>, Option<String>) {
    let mut command = if resolution.requested_binary == "claude" {
        let Some(binary) =
            crate::services::claude_command::ClaudeBinary::from_resolution(resolution)
        else {
            return (None, Some("version_probe_spawn_failed".to_string()));
        };
        crate::services::claude_command::ClaudeCommandBuilder::for_resolved_version_probe(
            &binary, resolution,
        )
        .into_command()
    } else {
        let mut command = Command::new(binary_path);
        configure_version_probe_command(&mut command, resolution);
        command
    };
    command.arg("--version");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return (None, Some("permission_denied".to_string()));
        }
        Err(_) => return (None, Some("version_probe_spawn_failed".to_string())),
    };
    let stdout_reader = child
        .stdout
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));

    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stderr_reader);
                drop(stdout_reader);
                return (None, Some("version_probe_timeout".to_string()));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stderr_reader);
                drop(stdout_reader);
                return (None, Some("version_probe_failed".to_string()));
            }
        }
    };

    let stdout = stdout_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let _ = stderr_reader.and_then(|reader| reader.join().ok());

    if status.success() {
        let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
        if stdout.is_empty() {
            (None, Some("version_probe_empty".to_string()))
        } else {
            (Some(stdout), None)
        }
    } else {
        (None, Some("version_probe_failed".to_string()))
    }
}

pub async fn async_resolve_binary_with_login_shell(name: &str) -> Option<String> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || resolve_binary_with_login_shell(&name))
        .await
        .ok()
        .flatten()
}

fn finalize_resolution(
    requested_binary: String,
    resolved_path: PathBuf,
    source: String,
    attempts: Vec<String>,
) -> BinaryResolution {
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

fn resolve_in_paths(
    binary_name: impl AsRef<OsStr>,
    paths: Option<OsString>,
    cwd: &Path,
) -> Option<PathBuf> {
    which::which_in(binary_name, paths, cwd).ok()
}

fn resolve_all_in_paths(
    binary_name: impl AsRef<OsStr>,
    paths: Option<OsString>,
    cwd: &Path,
) -> Vec<PathBuf> {
    which::which_in_all(binary_name, paths, cwd)
        .map(|iter| iter.collect())
        .unwrap_or_default()
}

fn resolve_candidate_path(candidate: &Path, cwd: &Path) -> Result<PathBuf, String> {
    if candidate.exists() && !is_effectively_executable(candidate) {
        return Err("permission_denied".to_string());
    }
    which::which_in(candidate.as_os_str(), Option::<OsString>::None, cwd)
        .map_err(|error| error.to_string())
}

fn is_effectively_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn build_exec_path(resolved_path: &Path, canonical_path: Option<&Path>) -> Option<String> {
    join_paths_lossy(exec_path_entries(resolved_path, canonical_path))
        .map(|value| value.to_string_lossy().to_string())
}

fn exec_path_for_binary(binary_path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(binary_path).ok();
    build_exec_path(binary_path, canonical.as_deref())
}

fn runtime_path_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    extend_split_paths(std::env::var_os("PATH"), &mut entries, &mut seen);
    extend_split_paths(resolve_login_shell_path_os(), &mut entries, &mut seen);
    for dir in standard_fallback_dirs() {
        push_unique_path(dir, &mut entries, &mut seen);
    }

    entries
}

fn exec_path_entries(resolved_path: &Path, canonical_path: Option<&Path>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(parent) = resolved_path.parent() {
        push_unique_path(parent.to_path_buf(), &mut entries, &mut seen);
    }
    if let Some(parent) = canonical_path.and_then(Path::parent) {
        push_unique_path(parent.to_path_buf(), &mut entries, &mut seen);
    }
    for entry in runtime_path_entries() {
        push_unique_path(entry, &mut entries, &mut seen);
    }

    entries
}

fn provider_fallback_dirs(provider: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();

    for dir in windows_provider_subdirs(provider) {
        push_unique_path(dir, &mut dirs, &mut seen);
    }
    for dir in provider_app_bundle_dirs(provider) {
        push_unique_path(dir, &mut dirs, &mut seen);
    }
    for dir in standard_fallback_dirs() {
        push_unique_path(dir, &mut dirs, &mut seen);
    }

    dirs
}

fn provider_app_bundle_dirs(provider: &str) -> Vec<PathBuf> {
    if normalize_name(provider) != "codex" {
        return Vec::new();
    }
    codex_app_bundle_resource_dirs()
}

#[cfg(target_os = "macos")]
fn codex_app_bundle_resource_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Applications/Codex.app/Contents/Resources")];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Applications/Codex.app/Contents/Resources"));
    }
    dirs
}

#[cfg(not(target_os = "macos"))]
fn codex_app_bundle_resource_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(windows)]
fn windows_provider_subdirs(provider: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(home) = dirs::home_dir() {
        push_unique_path(
            home.join("AppData/Local/Programs").join(provider),
            &mut dirs,
            &mut seen,
        );
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        push_unique_path(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join(provider),
            &mut dirs,
            &mut seen,
        );
    }
    push_unique_path(
        PathBuf::from("C:/Program Files").join(provider),
        &mut dirs,
        &mut seen,
    );
    push_unique_path(
        PathBuf::from("C:/Program Files (x86)").join(provider),
        &mut dirs,
        &mut seen,
    );

    dirs
}

#[cfg(not(windows))]
fn windows_provider_subdirs(_provider: &str) -> Vec<PathBuf> {
    Vec::new()
}

fn standard_fallback_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();
    let home = dirs::home_dir();

    if let Some(home) = &home {
        push_unique_path(home.join(".local/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join("bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".volta/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".bun/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".asdf/shims"), &mut dirs, &mut seen);
        push_unique_path(home.join(".npm-global/bin"), &mut dirs, &mut seen);
    }

    #[cfg(unix)]
    push_unique_path(PathBuf::from("/usr/local/bin"), &mut dirs, &mut seen);

    #[cfg(target_os = "macos")]
    {
        push_unique_path(PathBuf::from("/opt/homebrew/bin"), &mut dirs, &mut seen);
        if let Some(home) = &home {
            push_unique_path(home.join("Library/pnpm"), &mut dirs, &mut seen);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = &home {
            push_unique_path(home.join(".local/share/pnpm"), &mut dirs, &mut seen);
            push_unique_path(home.join(".local/share/mise/shims"), &mut dirs, &mut seen);
            push_unique_path(home.join(".local/share/rtx/shims"), &mut dirs, &mut seen);
        }
    }

    push_env_dir("PNPM_HOME", None, &mut dirs, &mut seen);
    push_env_dir("VOLTA_HOME", Some("bin"), &mut dirs, &mut seen);
    push_env_dir("BUN_INSTALL", Some("bin"), &mut dirs, &mut seen);
    push_env_dir("ASDF_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("MISE_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("RTX_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("N_PREFIX", Some("bin"), &mut dirs, &mut seen);

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            push_unique_path(PathBuf::from(appdata).join("npm"), &mut dirs, &mut seen);
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let root = PathBuf::from(local_app_data);
            push_unique_path(root.join("Volta/bin"), &mut dirs, &mut seen);
            push_unique_path(root.join("Programs"), &mut dirs, &mut seen);
        }
        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            push_unique_path(
                PathBuf::from(user_profile).join("scoop/shims"),
                &mut dirs,
                &mut seen,
            );
        }
        push_unique_path(
            PathBuf::from("C:/ProgramData/chocolatey/bin"),
            &mut dirs,
            &mut seen,
        );
    }

    dirs
}

#[cfg(unix)]
fn resolve_login_shell_path_os() -> Option<OsString> {
    static LOGIN_SHELL_PATH: OnceLock<Option<OsString>> = OnceLock::new();
    LOGIN_SHELL_PATH
        .get_or_init(resolve_login_shell_path_uncached)
        .clone()
}

#[cfg(not(unix))]
fn resolve_login_shell_path_os() -> Option<OsString> {
    None
}

#[cfg(unix)]
fn resolve_login_shell_path_uncached() -> Option<OsString> {
    let env_cmd = format!(
        "printf '%s' '{delimiter}'; command env; printf '%s' '{delimiter}'; exit",
        delimiter = SHELL_ENV_DELIMITER
    );

    for shell in login_shell_candidates() {
        let mut command = Command::new(&shell);
        command
            .args(["-ilc", &env_cmd])
            .env("DISABLE_AUTO_UPDATE", "true")
            .env("ZSH_TMUX_AUTOSTARTED", "true")
            .env("ZSH_TMUX_AUTOSTART", "false")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let Ok(mut child) = command.spawn() else {
            continue;
        };

        let deadline = Instant::now() + LOGIN_SHELL_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }

        let Ok(output) = child.wait_with_output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some((_, after_start)) = stdout.split_once(SHELL_ENV_DELIMITER) else {
            continue;
        };
        let Some((env_block, _)) = after_start.split_once(SHELL_ENV_DELIMITER) else {
            continue;
        };
        if let Some(path) = env_block
            .lines()
            .find_map(|line| line.strip_prefix("PATH="))
        {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Some(OsString::from(trimmed));
            }
        }
    }

    None
}

#[cfg(unix)]
fn login_shell_candidates() -> Vec<PathBuf> {
    let mut shells = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(shell) = std::env::var_os("SHELL").filter(|value| !os_value_is_empty(value)) {
        push_unique_path(PathBuf::from(shell), &mut shells, &mut seen);
    }
    push_unique_path(PathBuf::from("/bin/zsh"), &mut shells, &mut seen);
    push_unique_path(PathBuf::from("/bin/bash"), &mut shells, &mut seen);

    shells
}

fn current_dir_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn override_var_name(provider: &str) -> String {
    let mut normalized = String::new();
    for ch in provider.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_uppercase());
        } else {
            normalized.push('_');
        }
    }
    format!("AGENTDESK_{}_PATH", normalized)
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn os_value_is_empty(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

fn push_env_dir(
    env_name: &str,
    suffix: Option<&str>,
    entries: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    let Some(value) = std::env::var_os(env_name).filter(|value| !os_value_is_empty(value)) else {
        return;
    };
    let mut path =
        expand_user_path(&value.to_string_lossy()).unwrap_or_else(|| PathBuf::from(&value));
    if let Some(suffix) = suffix {
        path = path.join(suffix);
    }
    push_unique_path(path, entries, seen);
}

fn extend_split_paths(
    value: Option<OsString>,
    entries: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    let Some(value) = value.filter(|value| !os_value_is_empty(value)) else {
        return;
    };
    for entry in std::env::split_paths(&value) {
        push_unique_path(entry, entries, seen);
    }
}

fn push_unique_path(path: PathBuf, entries: &mut Vec<PathBuf>, seen: &mut BTreeSet<String>) {
    let normalized = path.to_string_lossy().trim().to_string();
    if normalized.is_empty() || !seen.insert(normalized) {
        return;
    }
    entries.push(path);
}

fn join_paths_lossy(paths: Vec<PathBuf>) -> Option<OsString> {
    if paths.is_empty() {
        return None;
    }
    std::env::join_paths(paths).ok()
}

/// Context-aware resolver (PR-2).
///
/// Resolution order:
/// 1. Per-agent channel override in registry (`agent_overrides`)
/// 2. Current registry channel
/// 3. Legacy env-override / PATH / login-shell / fallback (unchanged behaviour)
pub fn resolve_provider_binary_for_context(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
) -> BinaryResolution {
    let provider = normalize_name(&ctx.provider);
    if let Some(root) = crate::config::runtime_root() {
        if let Ok(Some(registry)) = crate::services::provider_cli::io::load_registry(&root) {
            if let Some(channels) = registry.providers.get(&provider) {
                // 1. Per-agent override → named channel
                let channel_name = ctx
                    .agent_id
                    .as_deref()
                    .and_then(|id| registry.agent_channel(&provider, id))
                    .unwrap_or("current");

                let selected_channel = match channel_name {
                    "candidate" => channels.candidate.as_ref(),
                    "default" => channels.default.as_ref(),
                    "previous" => channels.previous.as_ref(),
                    _ => channels.current.as_ref(),
                };

                if let Some(channel) = selected_channel {
                    if let Some(resolution) =
                        registry_channel_resolution(ctx, &provider, channel_name, channel)
                    {
                        return resolution;
                    }
                }

                if channel_name != "current" {
                    if let Some(channel) = channels.current.as_ref() {
                        if let Some(resolution) =
                            registry_channel_resolution(ctx, &provider, "current", channel)
                        {
                            return resolution;
                        }
                    }
                }
            }
        }
    }
    // 3. Fall back to legacy resolver.
    resolve_provider_binary_legacy(&provider)
}

fn registry_channel_resolution(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
    requested_binary: &str,
    channel_name: &str,
    channel: &crate::services::provider_cli::ProviderCliChannel,
) -> Option<BinaryResolution> {
    let cwd = current_dir_fallback();
    let expanded = expand_user_path(&channel.path).unwrap_or_else(|| PathBuf::from(&channel.path));
    let resolved_path = resolve_candidate_path(&expanded, &cwd).ok()?;
    let resolution = finalize_resolution(
        requested_binary.to_string(),
        resolved_path,
        format!("registry:{channel_name}"),
        vec![format!("registry:{channel_name}=found:{}", channel.path)],
    );
    record_context_launch_artifact(ctx, &resolution, channel_name, &channel.version);
    Some(resolution)
}

fn record_context_launch_artifact(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
    resolution: &BinaryResolution,
    channel_name: &str,
    cli_version: &str,
) {
    let Some(session_key) = ctx
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    let Some(cli_path) = resolution.resolved_path.clone() else {
        return;
    };
    let canonical_path = resolution
        .canonical_path
        .clone()
        .unwrap_or_else(|| cli_path.clone());

    let artifact = crate::services::provider_cli::LaunchArtifact {
        provider: resolution.requested_binary.clone(),
        agent_id: ctx.agent_id.clone(),
        channel_id: ctx.channel_id.clone(),
        session_key: Some(session_key.to_string()),
        channel: channel_name.to_string(),
        cli_path,
        canonical_path,
        cli_version: cli_version.to_string(),
        process_id: None,
        tmux_session: ctx.tmux_session.clone(),
        launched_at: chrono::Utc::now(),
    };

    let _ = crate::services::provider_cli::io::save_launch_artifact(&root, &artifact);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_probe_env(provider: &str) -> HashMap<String, Option<String>> {
        let resolution = BinaryResolution {
            requested_binary: provider.to_string(),
            resolved_path: Some(format!("/test/bin/{provider}")),
            canonical_path: None,
            source: Some("test".to_string()),
            attempts: Vec::new(),
            failure_kind: None,
            exec_path: Some("/test/bin".to_string()),
        };
        let mut command = Command::new(provider);
        command
            .env("ANTHROPIC_BASE_URL", "http://inherited.example:9999")
            .env(
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
                "inherited-value",
            );
        configure_version_probe_command(&mut command, &resolution);
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

    #[test]
    fn version_probe_scrubs_gateway_env_only_for_claude() {
        let claude_env = configured_probe_env("claude");
        assert_eq!(claude_env.get("ANTHROPIC_BASE_URL"), Some(&None));
        assert_eq!(
            claude_env.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&None)
        );

        for provider in ["codex", "qwen"] {
            let provider_env = configured_probe_env(provider);
            assert_eq!(
                provider_env.get("ANTHROPIC_BASE_URL"),
                Some(&Some("http://inherited.example:9999".to_string()))
            );
            assert_eq!(
                provider_env.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
                Some(&Some("inherited-value".to_string()))
            );
        }
    }

    #[test]
    fn parses_codex_cli_semver_from_version_output() {
        assert_eq!(
            parse_cli_semver("codex-cli 0.142.3"),
            Some(ParsedCliVersion {
                major: 0,
                minor: 142,
                patch: 3
            })
        );
        assert_eq!(
            parse_cli_semver("codex-cli v1.2.3-beta\nextra"),
            Some(ParsedCliVersion {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(parse_cli_semver("codex-cli dev-build"), None);
    }

    #[test]
    fn codex_app_bundle_candidate_gets_codex_only_priority() {
        let app_path = "/Applications/Codex.app/Contents/Resources/codex";

        assert_eq!(candidate_priority("codex", app_path), 5);
        assert_eq!(candidate_priority("claude", app_path), 50);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_fallback_dirs_include_app_bundle_resources_on_macos() {
        let dirs = provider_fallback_dirs("codex");

        assert!(
            dirs.iter().any(|dir| {
                dir.to_string_lossy() == "/Applications/Codex.app/Contents/Resources"
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_probe_selects_newest_successful_candidate_not_first_path_hit() {
        let temp = tempfile::tempdir().unwrap();
        let old_path = temp.path().join("homebrew/bin/codex");
        let new_path = temp
            .path()
            .join("Applications/Codex.app/Contents/Resources/codex");
        write_fake_version_binary(&old_path, "codex-cli 0.139.0");
        write_fake_version_binary(&new_path, "codex-cli 0.142.3");

        let old_resolution = finalize_resolution(
            "codex".to_string(),
            old_path.clone(),
            "current_path".to_string(),
            vec!["current_path=candidates:1".to_string()],
        );
        let new_resolution = finalize_resolution(
            "codex".to_string(),
            new_path.clone(),
            "fallback_path".to_string(),
            vec!["fallback_path=candidates:1".to_string()],
        );

        let probe = probe_provider_binary_candidates(vec![old_resolution, new_resolution]);

        assert_eq!(
            probe.resolution.resolved_path.as_deref(),
            Some(new_path.to_string_lossy().as_ref())
        );
        assert_eq!(probe.version_output.as_deref(), Some("codex-cli 0.142.3"));
        assert!(probe.resolution.attempts.iter().any(|attempt| {
            attempt.contains("skipped_candidate_success:") && attempt.contains("codex-cli 0.139.0")
        }));
        assert!(probe.resolution.attempts.iter().any(|attempt| {
            attempt.contains("selected_candidate_version:") && attempt.contains("codex-cli 0.142.3")
        }));
    }

    #[cfg(unix)]
    fn write_fake_version_binary(path: &Path, version_output: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{}'; exit 0; fi\nexit 1\n",
                version_output
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn write_executable_stub(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    /// Process-global lock serializing the `#[cfg(unix)]` tests that mutate
    /// `AGENTDESK_CLAUDE_PATH` / `PATH`. The Rust harness runs tests in parallel
    /// threads within one binary, so two env-mutating seal tests would otherwise
    /// race on the same variables. Poison is recovered (a mutation-demo panic
    /// while holding the lock must not cascade into unrelated failures).
    #[cfg(unix)]
    fn env_mutation_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Scoped guard that sets an env var to a value and restores the previous
    /// value (or unsets it) on drop.
    #[cfg(unix)]
    struct ScopedEnv {
        key: &'static str,
        previous: Option<OsString>,
    }

    #[cfg(unix)]
    impl ScopedEnv {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    #[cfg(unix)]
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// Scoped guard that forces Claude resolution through the `env_override`
    /// branch by pointing `AGENTDESK_CLAUDE_PATH` at a real executable, then
    /// restores the previous value. Env-mutating seal tests hold
    /// [`env_mutation_lock`] so they do not race each other.
    #[cfg(unix)]
    struct ClaudePathOverrideGuard {
        previous: Option<OsString>,
    }

    #[cfg(unix)]
    impl ClaudePathOverrideGuard {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_CLAUDE_PATH");
            unsafe { std::env::set_var("AGENTDESK_CLAUDE_PATH", path) };
            Self { previous }
        }
    }

    #[cfg(unix)]
    impl Drop for ClaudePathOverrideGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_CLAUDE_PATH", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_CLAUDE_PATH") },
            }
        }
    }

    /// #4627 mutation-proof seal test. With a real Claude executable forced via
    /// `AGENTDESK_CLAUDE_PATH`, the sanctioned sealed seam still surfaces the raw
    /// path (so `ClaudeBinary::resolve` keeps working), while the generic public
    /// `resolve_provider_binary` scrubs the raw path for `claude` and its
    /// whitespace/case variants — preserving diagnostics.
    ///
    /// Mutation proof: delete the `claude` branch in `scrub_sealed_provider_paths`
    /// and the `resolved_path.is_none()` assertions FAIL at runtime (assertion
    /// failure, not a compile error), because the override makes the unscrubbed
    /// `resolved_path` `Some(..)`.
    #[cfg(unix)]
    #[test]
    fn generic_seam_seals_claude_while_sealed_seam_exposes_it() {
        let _env = env_mutation_lock();
        let temp = tempfile::tempdir().unwrap();
        let claude_path = temp.path().join("bin/claude");
        write_executable_stub(&claude_path);
        let _guard = ClaudePathOverrideGuard::set(&claude_path);

        // The sanctioned raw-path seam still exposes the resolved path.
        let sealed = resolve_claude_binary_sealed();
        assert_eq!(
            sealed.resolved_path.as_deref(),
            Some(claude_path.to_string_lossy().as_ref()),
            "the sanctioned sealed seam must still surface the raw Claude path"
        );

        // The generic public seam scrubs the raw path for claude + variants.
        for provider in ["claude", "  CLAUDE ", "Claude"] {
            let resolution = resolve_provider_binary(provider);
            assert_eq!(resolution.requested_binary, "claude");
            assert!(
                resolution.resolved_path.is_none(),
                "claude resolved_path must be sealed for provider {provider:?}"
            );
            assert!(
                resolution.canonical_path.is_none(),
                "claude canonical_path must be sealed for provider {provider:?}"
            );
            assert!(
                resolution.exec_path.is_none(),
                "claude exec_path must be sealed for provider {provider:?}"
            );
            // Diagnostics survive the scrub.
            assert_eq!(resolution.source.as_deref(), Some("env_override"));
            assert!(resolution.failure_kind.is_none());
            assert!(!resolution.attempts.is_empty());
        }
    }

    /// Unit coverage for the scrub itself: only `claude` is sealed; its `attempts`
    /// have every raw-path component redacted while structural fields survive;
    /// other providers pass through untouched.
    #[test]
    fn scrub_seals_only_claude_paths() {
        let sample = |provider: &str| BinaryResolution {
            requested_binary: provider.to_string(),
            resolved_path: Some(format!("/opt/{provider}/bin/{provider}")),
            canonical_path: Some(format!("/opt/{provider}/bin/{provider}")),
            source: Some("env_override".to_string()),
            // Cover all path-bearing attempt patterns the resolver can emit.
            attempts: vec![
                format!(
                    "env_override:AGENTDESK_{provider}_PATH=found:/opt/{provider}/bin/{provider}"
                ),
                format!("current_path=candidate:priority=50:/usr/local/bin/{provider}"),
                format!("selected_candidate:current_path:priority=50:/usr/local/bin/{provider}"),
                format!("registry:current=found:/opt/{provider}/bin/{provider}"),
                format!("selected_candidate_version:/opt/{provider}/bin/{provider}:version=1.2.3"),
                "current_path=candidates:2".to_string(),
            ],
            failure_kind: None,
            exec_path: Some("/opt/bin".to_string()),
        };

        let claude = scrub_sealed_provider_paths(sample("claude"));
        assert!(claude.resolved_path.is_none());
        assert!(claude.canonical_path.is_none());
        assert!(claude.exec_path.is_none());
        assert_eq!(claude.source.as_deref(), Some("env_override"));
        // No attempt retains a raw path; structural prefixes/suffixes survive.
        for attempt in &claude.attempts {
            assert!(
                !attempt.contains('/') && !attempt.contains('\\'),
                "claude attempt still leaks a path: {attempt}"
            );
        }
        assert!(
            claude
                .attempts
                .iter()
                .any(|attempt| attempt == "env_override:AGENTDESK_claude_PATH=found:<sealed-path>"),
            "env_override structure must survive redaction: {:?}",
            claude.attempts
        );
        assert!(
            claude
                .attempts
                .iter()
                .any(|attempt| attempt
                    == "selected_candidate:current_path:priority=50:<sealed-path>"),
            "selected_candidate structure must survive redaction"
        );
        assert!(
            claude
                .attempts
                .iter()
                .any(|attempt| attempt == "selected_candidate_version:<sealed-path>:version=1.2.3"),
            "version diagnostic must survive path redaction"
        );
        assert!(
            claude
                .attempts
                .iter()
                .any(|attempt| attempt == "current_path=candidates:2"),
            "path-free attempts must be untouched"
        );

        for provider in ["codex", "gemini", "qwen", "opencode"] {
            let untouched = scrub_sealed_provider_paths(sample(provider));
            assert_eq!(
                untouched.resolved_path.as_deref(),
                Some(format!("/opt/{provider}/bin/{provider}").as_str()),
                "non-claude provider {provider} resolved_path must be unchanged"
            );
            assert!(untouched.canonical_path.is_some());
            assert!(untouched.exec_path.is_some());
            assert!(
                untouched
                    .attempts
                    .iter()
                    .any(|attempt| attempt.contains(&format!("/opt/{provider}/bin/{provider}"))),
                "non-claude provider {provider} attempts must be unchanged"
            );
        }
    }

    /// #4627 mutation-proof seal test for the `attempts` diagnostics. Two
    /// resolutions are exercised end-to-end through the generic public
    /// `resolve_provider_binary("claude")`:
    ///
    ///   1. `env_override` — attempt line `env_override:…=found:<path>` carries the
    ///      raw path.
    ///   2. `PATH` discovery — attempt lines `current_path=candidate:…:<path>` /
    ///      `selected_candidate:…:<path>` carry the raw path.
    ///
    /// In both cases NO returned attempt may contain a filesystem-path separator,
    /// and the specific temp paths must not appear anywhere in `attempts`.
    ///
    /// Mutation proof: delete the `attempts` redaction loop in
    /// `scrub_sealed_provider_paths` and both phases FAIL at runtime (assertion
    /// failure, not a compile error), because the raw override / candidate path
    /// then survives in `attempts`.
    #[cfg(unix)]
    #[test]
    fn resolve_provider_binary_redacts_claude_paths_in_attempts() {
        let _env = env_mutation_lock();
        let temp = tempfile::tempdir().unwrap();

        // Phase 1: env_override.
        let override_path = temp.path().join("override/bin/claude");
        write_executable_stub(&override_path);
        {
            let _guard = ClaudePathOverrideGuard::set(&override_path);
            let resolution = resolve_provider_binary("claude");
            assert_eq!(resolution.source.as_deref(), Some("env_override"));
            assert!(!resolution.attempts.is_empty());
            assert_attempts_are_path_free(&resolution.attempts, &override_path);
            assert!(
                resolution
                    .attempts
                    .iter()
                    .any(|attempt| attempt.starts_with("env_override:")
                        && attempt.ends_with(SEALED_PATH_MARKER)),
                "env_override attempt must be present and redacted: {:?}",
                resolution.attempts
            );
        }

        // Phase 2: PATH discovery (env_override unset, PATH points at a temp bin
        // holding a `claude` executable). Extra candidates from the login-shell /
        // fallback dirs may also appear; the invariant is that NONE of the
        // returned attempts retains a raw path.
        let path_bin = temp.path().join("pathbin");
        write_executable_stub(&path_bin.join("claude"));
        let _no_override = ScopedEnv::unset("AGENTDESK_CLAUDE_PATH");
        let _path = ScopedEnv::set("PATH", &path_bin);

        let resolution = resolve_provider_binary("claude");
        assert!(!resolution.attempts.is_empty());
        assert_attempts_are_path_free(&resolution.attempts, &path_bin);
        assert!(
            resolution.attempts.iter().any(|attempt| {
                attempt.starts_with("current_path=candidate:")
                    && attempt.ends_with(SEALED_PATH_MARKER)
            }),
            "PATH-discovered candidate attempt must be present and redacted: {:?}",
            resolution.attempts
        );
    }

    /// Assert no attempt line retains a filesystem-path separator, and that the
    /// specific raw path (and its parent dir) never appears in any attempt.
    #[cfg(unix)]
    fn assert_attempts_are_path_free(attempts: &[String], raw_path: &Path) {
        let raw = raw_path.to_string_lossy();
        let parent = raw_path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default();
        for attempt in attempts {
            assert!(
                !attempt.contains('/') && !attempt.contains('\\'),
                "attempt still contains a path separator: {attempt}"
            );
            assert!(
                !attempt.contains(raw.as_ref()),
                "attempt leaks the raw path {raw}: {attempt}"
            );
            assert!(
                parent.is_empty() || !attempt.contains(&parent),
                "attempt leaks the raw parent dir {parent}: {attempt}"
            );
        }
    }
}
