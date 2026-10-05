//! Metadata comparison only: native IDs are not ADK turn/lease or launch proof.
use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use std::io::{BufRead, BufReader, Read};

// Install holds source authority: bounded bytes/lines, no waiting or retries.
const HEADER_BYTES: u64 = 64 * 1024;
const HEADER_LINES: usize = 16;

pub(super) fn observe(path: &Path, supplied: Option<&str>) -> &'static str {
    let root = default_codex_sessions_dir().and_then(|p| p.canonicalize().ok());
    let canonical = path.canonicalize().ok();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok();
    let identity = file.as_ref().map(SourceFileIdentity::from_open_file);
    let verdict = match (root.as_ref(), canonical.as_ref(), file) {
        (Some(root), Some(path), Some(file)) if path.starts_with(root) => compare(file, supplied),
        _ => "pending",
    };
    // This install log is the consumer, not durable source selection evidence.
    tracing::info!(
        scope = "source_metadata_only",
        launch_verified = false,
        verdict,
        ?root,
        ?canonical,
        ?identity,
        "Codex rollout metadata observation"
    );
    verdict
}

pub(super) fn compare(file: std::fs::File, supplied: Option<&str>) -> &'static str {
    let Some(supplied) = supplied.and_then(|id| uuid::Uuid::parse_str(id.trim()).ok()) else {
        return "pending";
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return "pending";
    }
    let mut reader = BufReader::new(file.take(HEADER_BYTES));
    let mut line = String::new();
    for _ in 0..HEADER_LINES {
        line.clear();
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            return "pending";
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return "pending";
        };
        if value["type"] != "session_meta" {
            continue;
        }
        let meta = &value["payload"];
        let Some(id) = meta["id"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            return "pending";
        };
        if id != supplied {
            return "rejected";
        }
        let source = &meta["source"];
        if source.get("subagent").is_some() || meta["parent_thread_id"].as_str().is_some() {
            return "child";
        }
        return match source.as_str() {
            Some("cli" | "vscode" | "exec") => "matched",
            _ => "pending",
        };
    }
    "pending"
}

/// Which Codex front end a launch expects the rollout header to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexRolloutSource {
    Cli,
    Exec,
}

impl CodexRolloutSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Exec => "exec",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexHookSourceRoute {
    PayloadPath,
    RolloutIndex,
}

/// One Codex hook's claim: native session id, optional rollout path, launch mode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CodexHookSourceClaim<'a> {
    pub session_id: &'a str,
    pub transcript_path: Option<&'a Path>,
    pub expected_source: CodexRolloutSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedCodexHookSource {
    pub session_id: String,
    pub rollout_path: PathBuf,
    pub identity: SourceFileIdentity,
    pub route: CodexHookSourceRoute,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexHookSourceRejection {
    InvalidSessionId,
    FileNameMismatch,
    OutsideSessionsRoot,
    RolloutUnavailable,
    RolloutReplaced,
    SessionMetaMissing,
    FirstRecordNotSessionMeta,
    SessionMetaIdMismatch,
    SourceMismatch { found: Option<String> },
    IndexIncomplete,
    NoCandidate,
    AmbiguousCandidates(usize),
}

impl CodexHookSourceRejection {
    /// A retry hint only; none of these outcomes is ever an accepted source.
    pub(crate) fn may_resolve_later(&self) -> bool {
        matches!(
            self,
            Self::RolloutUnavailable
                | Self::RolloutReplaced
                | Self::SessionMetaMissing
                | Self::IndexIncomplete
                | Self::NoCandidate
        )
    }
}

/// Accepts a hook's rollout only when its file name, root, header id and source all agree.
pub(crate) fn verify_codex_hook_source(
    sessions_root: &Path,
    claim: &CodexHookSourceClaim<'_>,
) -> Result<VerifiedCodexHookSource, CodexHookSourceRejection> {
    let id = uuid::Uuid::parse_str(claim.session_id)
        .ok()
        .filter(|id| id.hyphenated().to_string() == claim.session_id)
        .ok_or(CodexHookSourceRejection::InvalidSessionId)?;
    let suffix = format!("-{id}.jsonl");
    if let Some(path) = claim.transcript_path {
        return verify_rollout(sessions_root, path, id, &suffix, claim.expected_source)
            .map(|source| source.via(CodexHookSourceRoute::PayloadPath));
    }
    let indexed =
        crate::services::codex_tui::rollout_index::complete_indexed_rollouts(sessions_root)
            .map_err(|_| CodexHookSourceRejection::IndexIncomplete)?;
    let mut candidates: Vec<PathBuf> = indexed
        .into_iter()
        .filter(|item| {
            rollout_file_name_matches(&item.path, &suffix)
                || item
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.id.as_deref())
                    .and_then(|found| uuid::Uuid::parse_str(found).ok())
                    == Some(id)
        })
        .map(|item| item.path)
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [] => Err(CodexHookSourceRejection::NoCandidate),
        [path] => verify_rollout(sessions_root, path, id, &suffix, claim.expected_source)
            .map(|source| source.via(CodexHookSourceRoute::RolloutIndex)),
        many => Err(CodexHookSourceRejection::AmbiguousCandidates(many.len())),
    }
}

impl VerifiedCodexHookSource {
    fn via(mut self, route: CodexHookSourceRoute) -> Self {
        self.route = route;
        self
    }
}

fn rollout_file_name_matches(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(suffix))
}

/// Resolves symlinks and requires the resolved file to keep the rollout name and stay under root.
fn rooted_rollout(
    root: &Path,
    path: &Path,
    suffix: &str,
) -> Result<PathBuf, CodexHookSourceRejection> {
    let canonical = path
        .canonicalize()
        .map_err(|_| CodexHookSourceRejection::RolloutUnavailable)?;
    if !canonical.starts_with(root) {
        return Err(CodexHookSourceRejection::OutsideSessionsRoot);
    }
    if !rollout_file_name_matches(&canonical, suffix) {
        return Err(CodexHookSourceRejection::FileNameMismatch);
    }
    Ok(canonical)
}

fn open_regular_file(path: &Path) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    file.metadata()
        .is_ok_and(|meta| meta.is_file())
        .then_some(file)
}

fn path_identity(path: &Path) -> SourceFileIdentity {
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        return SourceFileIdentity::Unix {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
    }
    let _ = path;
    SourceFileIdentity::Unavailable
}

// Real 0.157.1 headers are about 23 KiB; a longer unterminated line is not a header.
const FIRST_RECORD_BYTES: u64 = 1024 * 1024;

/// Judges only the first record: unfinished is retryable, a finished non-header is final.
fn first_record_session_meta(
    file: &std::fs::File,
) -> Result<
    (
        crate::services::codex_tui::rollout_index::RolloutSessionMeta,
        Option<chrono::DateTime<chrono::Utc>>,
    ),
    CodexHookSourceRejection,
> {
    use CodexHookSourceRejection as Reject;
    let mut line = Vec::new();
    BufReader::new(file.take(FIRST_RECORD_BYTES))
        .read_until(b'\n', &mut line)
        .map_err(|_| Reject::RolloutUnavailable)?;
    if line.last() != Some(&b'\n') {
        return Err(if (line.len() as u64) < FIRST_RECORD_BYTES {
            Reject::SessionMetaMissing
        } else {
            Reject::FirstRecordNotSessionMeta
        });
    }
    let record: Value =
        serde_json::from_slice(&line).map_err(|_| Reject::FirstRecordNotSessionMeta)?;
    let payload = &record["payload"];
    let text = |key: &str| {
        payload[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    match (record["type"].as_str(), text("cwd")) {
        (Some("session_meta"), Some(cwd)) => Ok((
            crate::services::codex_tui::rollout_index::RolloutSessionMeta {
                id: text("id"),
                cwd: PathBuf::from(cwd),
                source: payload.get("source").cloned(),
                parent_thread_id: text("parent_thread_id"),
                originator: payload["originator"].as_str().map(ToString::to_string),
            },
            payload["timestamp"]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|time| time.with_timezone(&chrono::Utc)),
        )),
        _ => Err(Reject::FirstRecordNotSessionMeta),
    }
}

/// Points inside `verify_rollout` where tests may swap files under the open descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyStep {
    AfterOpen,
    AfterHeader,
    BeforeFinalIdentity,
    #[cfg(test)]
    AfterVerified,
}

#[cfg(test)]
thread_local! {
    static VERIFY_HOOKS: std::cell::RefCell<Vec<(VerifyStep, Box<dyn FnOnce()>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn at_verify_step(step: VerifyStep, hook: impl FnOnce() + 'static) {
    VERIFY_HOOKS.with(|hooks| hooks.borrow_mut().push((step, Box::new(hook))));
}

#[cfg(test)]
fn before_final_identity(hook: impl FnOnce() + 'static) {
    at_verify_step(VerifyStep::BeforeFinalIdentity, hook);
}

#[cfg(test)]
fn run_verify_step(step: VerifyStep) {
    let hook = VERIFY_HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        let index = hooks.iter().position(|(at, _)| *at == step)?;
        Some(hooks.remove(index).1)
    });
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn run_verify_step(_step: VerifyStep) {}

fn verify_rollout(
    sessions_root: &Path,
    path: &Path,
    id: uuid::Uuid,
    suffix: &str,
    expected: CodexRolloutSource,
) -> Result<VerifiedCodexHookSource, CodexHookSourceRejection> {
    use CodexHookSourceRejection as Reject;
    if !rollout_file_name_matches(path, suffix) {
        return Err(Reject::FileNameMismatch);
    }
    let root = sessions_root
        .canonicalize()
        .map_err(|_| Reject::RolloutUnavailable)?;
    let canonical = rooted_rollout(&root, path, suffix)?;
    let file = open_regular_file(&canonical).ok_or(Reject::RolloutUnavailable)?;
    run_verify_step(VerifyStep::AfterOpen);
    let identity = SourceFileIdentity::from_open_file(&file);
    if identity == SourceFileIdentity::Unavailable {
        return Err(Reject::RolloutUnavailable);
    }
    let (meta, created_at) = first_record_session_meta(&file)?;
    run_verify_step(VerifyStep::AfterHeader);
    let meta_id = meta
        .id
        .as_deref()
        .and_then(|found| uuid::Uuid::parse_str(found).ok());
    if meta_id != Some(id) {
        return Err(Reject::SessionMetaIdMismatch);
    }
    let source = meta.source.as_ref().and_then(Value::as_str);
    let source_matches = source == Some(expected.as_str())
        && (expected == CodexRolloutSource::Exec || meta.is_tui_compatible());
    if !source_matches {
        return Err(Reject::SourceMismatch {
            found: source.map(ToString::to_string),
        });
    }
    run_verify_step(VerifyStep::BeforeFinalIdentity);
    // The header came from this descriptor; the path must still resolve, inside root, to it.
    let current = rooted_rollout(&root, path, suffix)?;
    if current != canonical || path_identity(&current) != identity {
        return Err(Reject::RolloutReplaced);
    }
    #[cfg(test)]
    run_verify_step(VerifyStep::AfterVerified);
    Ok(VerifiedCodexHookSource {
        session_id: id.hyphenated().to_string(),
        rollout_path: canonical,
        identity,
        route: CodexHookSourceRoute::PayloadPath,
        created_at,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexSourceMode {
    Legacy,
    Shadow,
    Verified,
    Invalid,
}
impl CodexSourceMode {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            None | Some("legacy") => Self::Legacy,
            Some("shadow") => Self::Shadow,
            Some("verified") => Self::Verified,
            _ => Self::Invalid,
        }
    }
    pub(crate) fn launch_policy(self) -> Result<&'static str, &'static str> {
        match self {
            Self::Legacy => Ok("legacy"),
            Self::Shadow => Ok("shadow"),
            Self::Verified => Err("SourceModeVerifiedNotLanded"),
            Self::Invalid => Err("SourceModeInvalid"),
        }
    }
}

/// Validated once at startup; no active nonce or new launch reparses mutable env.
pub(crate) fn codex_source_mode_snapshot() -> CodexSourceMode {
    #[cfg(test)]
    if let Some(mode) = SOURCE_MODE_TEST.with(|mode| mode.get()) {
        return mode;
    }
    static MODE: std::sync::OnceLock<CodexSourceMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let value = std::env::var_os("AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE");
        let mode = CodexSourceMode::parse(value.as_ref().map(|v| v.to_str().unwrap_or("invalid")));
        tracing::info!(
            ?mode,
            eligible = mode.launch_policy().is_ok(),
            "Codex source-mode startup snapshot"
        );
        mode
    })
}
#[cfg(test)]
thread_local! { pub(crate) static SOURCE_MODE_TEST: std::cell::Cell<Option<CodexSourceMode>> = const { std::cell::Cell::new(None) }; }

/// Checks the actual argv bytes; missing or malformed digests never qualify a UPS.
pub(crate) fn first_prompt_matches(expected: Option<&str>, prompt: &Value) -> bool {
    use sha2::{Digest, Sha256};
    let (Some(hex), Some(prompt)) = (
        expected.and_then(|s| s.strip_prefix("sha256:")),
        prompt.as_str(),
    ) else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && hex == format!("{:x}", Sha256::digest(prompt.as_bytes()))
}

pub(crate) struct CodexFirstProof<'a> {
    pub captured: &'a crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    pub prepared: &'a crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    pub current_nonce: Option<&'a str>,
    pub verified_fresh_spawn: bool,
    pub no_prior_claim_or_transition: bool,
    pub event: &'a str,
    pub source: Option<&'a str>,
    pub prompt: &'a Value,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CodexFirstProofRejection {
    Context,
    Ineligible,
    Native(CodexHookSourceRejection),
}
impl CodexFirstProofRejection {
    pub(crate) fn verdict(&self) -> &'static str {
        match self {
            Self::Ineligible => "ineligible",
            Self::Native(error) if error.may_resolve_later() => "pending",
            _ => "rejected",
        }
    }
}

#[cfg(test)]
fn shadow_test_mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_CODEX_SHADOW_TEST_MUTATION").is_ok_and(|value| value == name)
}

/// Read-only initial-parent qualification, not publication or delivery authority.
pub(crate) fn codex_first_proof_candidate(
    launch: &CodexFirstProof<'_>,
    claim: &CodexHookSourceClaim<'_>,
) -> Result<VerifiedCodexHookSource, CodexFirstProofRejection> {
    #[cfg(test)]
    let omit_context = shadow_test_mutant("context");
    #[cfg(not(test))]
    let omit_context = false;
    if !omit_context {
        crate::services::tui_prompt_dedupe::binding_context::codex_context_candidate(
            launch.captured,
            launch.prepared,
            launch.current_nonce,
        )
        .map_err(|_| CodexFirstProofRejection::Context)?;
    }
    #[cfg(test)]
    let omit_fresh = shadow_test_mutant("fresh");
    #[cfg(not(test))]
    let omit_fresh = false;
    #[cfg(test)]
    let omit_digest = shadow_test_mutant("digest");
    #[cfg(not(test))]
    let omit_digest = false;
    if !launch.verified_fresh_spawn
        || !launch.no_prior_claim_or_transition
        || (!omit_fresh
            && (launch.prepared.launch_mode != "fresh"
                || launch.prepared.expected_native_session_id.is_some()))
        || !matches!(
            launch.prepared.source_policy.as_deref(),
            Some("shadow" | "verified")
        )
        || claim.expected_source != CodexRolloutSource::Cli
        || !match launch.event {
            "SessionStart" => launch.source == Some("startup"),
            "UserPromptSubmit" => {
                omit_digest
                    || first_prompt_matches(
                        launch.prepared.first_prompt_digest.as_deref(),
                        launch.prompt,
                    )
            }
            _ => false,
        }
    {
        return Err(CodexFirstProofRejection::Ineligible);
    }
    let root = launch
        .prepared
        .provider_root
        .as_deref()
        .ok_or(CodexFirstProofRejection::Context)?;
    verify_codex_hook_source(root, claim).map_err(CodexFirstProofRejection::Native)
}

#[cfg(test)]
#[path = "source_observation_tests.rs"]
mod source_observation_tests;
