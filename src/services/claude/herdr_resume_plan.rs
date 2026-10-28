//! Read-only Claude migration planning; a plan never grants permission to launch or send input.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_key_with_identity_pg,
};
use crate::db::dispatched_sessions::{ProviderSessionIds, load_provider_session_ids_pg};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::transcript_tail::{default_claude_home, encode_project_path};
use crate::services::session_selector_validity::{
    SelectorFileActivity, activity_with_read_only_growth, choose_provider_session_selector,
};
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, SpawnNonceMarker, execution_context, observe_spawn_nonce_marker_read_only,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingEvent, BindingTarget, SourceId, StrictBindingRecord, records_strict_verified,
};
use crate::services::tui_prompt_dedupe::{
    RuntimeObservation, runtime_observation_for_kind_under_source_authority,
};

mod history;

pub(crate) struct Request<'a> {
    pub session_key: &'a str,
    pub discord_token_hash: &'a str,
    pub logical_key: &'a str,
    pub channel_id: u64,
    pub working_dir: &'a Path,
    pub requested_session_id: Option<&'a str>,
    pub force_fresh: bool,
    pub claude_home: Option<&'a Path>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PlanContext {
    expected_native_session_id: Option<String>,
    exact_source: Option<SourceId>,
    launch_mode: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) enum Plan {
    Fresh(FreshReason, ReadSet, PlanContext),
    Resume(SourceId, ReadSet, PlanContext),
    Hold(HoldReason, ReadSet, PlanContext),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FreshReason {
    Explicit,
    SelectorAbsent,
    TranscriptAbsent,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HoldReason {
    Database(String),
    InvalidSelector(String),
    Runtime(String),
    Context(String),
    History(String),
    FileIo { path: PathBuf, error: String },
    Conflict(String),
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadSet {
    session_key: String,
    discord_token_hash: String,
    logical_key: String,
    channel_id: u64,
    working_dir: PathBuf,
    requested_session_id: Option<String>,
    force_fresh: bool,
    database: Option<DatabaseRead>,
    runtime: Option<RuntimeRead>,
    old_context: Option<BindingContext>,
    latest_binding: Option<BindingEvent>,
    reconciled_source: Option<SourceId>,
    files: Vec<FileRead>,
    missing_files: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DatabaseRead {
    resolved_session_key: String,
    cached: Option<String>,
    raw: Option<String>,
    cwd: Option<String>,
    cache_entry_age_secs: Option<i64>,
    watermark: Option<i64>,
    watermark_session_id: Option<String>,
    growth_proven: bool,
    selected: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuntimeRead {
    state: &'static str,
    kind: String,
    session_id: Option<String>,
    output_path: String,
}

#[derive(Clone, Debug, Serialize)]
struct FileRead {
    path: PathBuf,
    len: u64,
    dev: u64,
    ino: u64,
    mtime_age_secs: Option<i64>,
}

impl Plan {
    pub(crate) fn diagnostic(&self) -> Value {
        let (decision, reason, readset, context, source) = match self {
            Self::Fresh(reason, readset, context) => {
                ("fresh", json!(reason), readset, context, None)
            }
            Self::Resume(source, readset, context) => {
                ("resume", Value::Null, readset, context, Some(source))
            }
            Self::Hold(reason, readset, context) => ("hold", json!(reason), readset, context, None),
        };
        json!({
            "mode": "plan_only", "decision": decision, "reason": reason,
            "expected_native_session_id": context.expected_native_session_id,
            "source": source, "context": context, "readset": readset,
        })
    }
}

impl ReadSet {
    fn new(request: &Request<'_>) -> Self {
        Self {
            session_key: request.session_key.into(),
            discord_token_hash: request.discord_token_hash.into(),
            logical_key: request.logical_key.into(),
            channel_id: request.channel_id,
            working_dir: request.working_dir.into(),
            requested_session_id: normalized(request.requested_session_id).map(str::to_string),
            force_fresh: request.force_fresh,
            database: None,
            runtime: None,
            old_context: None,
            latest_binding: None,
            reconciled_source: None,
            files: Vec::new(),
            missing_files: Vec::new(),
        }
    }

    fn context(&self, source: Option<&SourceId>, mode: Option<&'static str>) -> PlanContext {
        PlanContext {
            expected_native_session_id: source
                .map(|source| source.session_id.clone())
                .or_else(|| self.requested_session_id.clone())
                .or_else(|| self.database.as_ref().and_then(|db| db.selected.clone())),
            exact_source: source.cloned(),
            launch_mode: mode,
        }
    }

    fn hold(self, reason: HoldReason) -> Plan {
        let context = self.context(None, None);
        Plan::Hold(reason, self, context)
    }

    fn fresh(self, reason: FreshReason) -> Plan {
        let context = PlanContext {
            expected_native_session_id: None,
            exact_source: None,
            launch_mode: Some("fresh"),
        };
        Plan::Fresh(reason, self, context)
    }
}

pub(crate) async fn resolve(pool: &PgPool, request: Request<'_>) -> Plan {
    let readset = ReadSet::new(&request);
    if request.force_fresh {
        return readset.fresh(FreshReason::Explicit);
    }
    let database = load_canonical_database(pool, &request).await;
    let (runtime, context) = crate::services::tmux_common::with_tmux_source_authority(
        request.logical_key,
        |authority| {
            let runtime = runtime_observation_for_kind_under_source_authority(
                authority,
                RuntimeHandoffKind::ClaudeTui,
            );
            let context = match observe_spawn_nonce_marker_read_only(request.logical_key) {
                SpawnNonceMarker::Absent => Ok(None),
                SpawnNonceMarker::Unreadable => Err("spawn nonce is unreadable".to_string()),
                SpawnNonceMarker::Known(nonce) => execution_context("claude", &nonce).map(Some),
            };
            (runtime, context)
        },
    );
    let history = records_strict_verified(request.channel_id)
        .map_err(|error| error.to_string())
        .and_then(|records| records.map_err(|error| format!("{error:?}")));
    evaluate_verified(request, database, runtime, context, history)
}

async fn load_canonical_database(
    pool: &PgPool,
    request: &Request<'_>,
) -> Result<Option<ProviderSessionIds>, String> {
    let channel = request.channel_id.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: request.discord_token_hash,
        channel_id: &channel,
    };
    let resolved = resolve_session_key_with_identity_pg(
        pool,
        request.session_key,
        Some("claude"),
        Some(identity),
    )
    .await
    .map_err(|error| format!("canonical selector lookup: {error:?}"))?;
    let Some(resolved) = resolved else {
        return Ok(None);
    };
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM sessions
             WHERE session_key = $1 AND provider = 'claude'
               AND identity_kind = 'discord_channel'
               AND discord_token_hash = $2 AND channel_id = $3
         )",
    )
    .bind(&resolved)
    .bind(request.discord_token_hash)
    .bind(&channel)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("canonical selector owner check: {error}"))?;
    if !owned {
        return Err("resolved locator has another canonical owner".into());
    }
    load_provider_session_ids_pg(pool, &resolved, Some("claude"))
        .await?
        .map(Some)
        .ok_or_else(|| "canonical selector row disappeared or changed provider".into())
}

fn evaluate_verified(
    request: Request<'_>,
    database: Result<Option<ProviderSessionIds>, String>,
    runtime: RuntimeObservation,
    context: Result<Option<BindingContext>, String>,
    history: Result<Vec<StrictBindingRecord>, String>,
) -> Plan {
    let mut readset = ReadSet::new(&request);
    if request.force_fresh {
        return readset.fresh(FreshReason::Explicit);
    }
    let database = match database {
        Ok(database) => database,
        Err(error) => return readset.hold(HoldReason::Database(error)),
    };
    readset.database = database.as_ref().map(|db| DatabaseRead {
        resolved_session_key: db.resolved_session_key.clone(),
        cached: db.claude_session_id.clone(),
        raw: db.raw_provider_session_id.clone(),
        cwd: db.cwd.clone(),
        cache_entry_age_secs: db.cache_entry_age_secs,
        watermark: db.raw_provider_transcript_len_watermark,
        watermark_session_id: db.raw_provider_transcript_watermark_session_id.clone(),
        growth_proven: db.raw_provider_transcript_growth_proven,
        selected: None,
    });
    let runtime_binding = match runtime {
        RuntimeObservation::Missing => None,
        RuntimeObservation::Present(binding) => {
            readset.runtime = Some(runtime_read("present", &binding));
            Some(binding)
        }
        RuntimeObservation::Expired(binding) | RuntimeObservation::Withheld(binding) => {
            readset.runtime = Some(runtime_read("unavailable", &binding));
            return readset.hold(HoldReason::Runtime("binding expired or withheld".into()));
        }
    };
    readset.old_context = match context {
        Ok(context) => context,
        Err(error) => return readset.hold(HoldReason::Context(error)),
    };
    if let Some(context) = &readset.old_context
        && (context.schema != 1
            || context.provider != "claude"
            || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
            || context.tmux_session != request.logical_key
            || context.channel_id != Some(request.channel_id)
            || context.owner_runtime_root
                != crate::services::tmux_common::current_tmux_owner_marker())
    {
        return readset.hold(HoldReason::Context("incarnation owner differs".into()));
    }
    let history = match history {
        Ok(history) => history,
        Err(error) => return readset.hold(HoldReason::History(error)),
    };
    if let Some(context) = &readset.old_context {
        readset.reconciled_source = match history::reconcile(context, &history) {
            Ok(source) => source,
            Err(reason) => return readset.hold(reason),
        };
    }
    readset.latest_binding = history
        .into_iter()
        .map(|record| record.event)
        .rev()
        .find(|event| {
            event.provider == "claude"
                && event.tmux_session == request.logical_key
                && !matches!(event.new, BindingTarget::Rejected { .. })
        });
    if let (Some(context), Some(event)) = (&readset.old_context, &readset.latest_binding)
        && event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str())
    {
        return readset.hold(HoldReason::Conflict(
            "context and latest binding nonce differ".into(),
        ));
    }
    match select(
        &request,
        database.as_ref(),
        runtime_binding.as_ref(),
        &mut readset,
    ) {
        Ok(Some(source)) => {
            let context = readset.context(Some(&source), Some("resume"));
            Plan::Resume(source, readset, context)
        }
        Ok(None) => {
            let reason = if readset.requested_session_id.is_some()
                || readset
                    .database
                    .as_ref()
                    .is_some_and(|db| db.selected.is_some())
            {
                FreshReason::TranscriptAbsent
            } else {
                FreshReason::SelectorAbsent
            };
            readset.fresh(reason)
        }
        Err(reason) => readset.hold(reason),
    }
}

#[cfg(test)]
fn evaluate(
    request: Request<'_>,
    database: Result<Option<ProviderSessionIds>, String>,
    runtime: RuntimeObservation,
    context: Result<Option<BindingContext>, String>,
    history: Result<Vec<BindingEvent>, String>,
) -> Plan {
    evaluate_verified(
        request,
        database,
        runtime,
        context,
        history.map(|events| {
            events
                .into_iter()
                .map(|event| StrictBindingRecord {
                    published_at: Some(event.evidence.received_at),
                    event,
                    verified: false,
                })
                .collect()
        }),
    )
}

fn check_provider_root(
    request: &Request<'_>,
    context: Option<&BindingContext>,
) -> Result<(), HoldReason> {
    let Some(old) = context.and_then(|context| context.provider_root.as_deref()) else {
        return Ok(());
    };
    let current = request
        .claude_home
        .map(Path::to_path_buf)
        .or_else(default_claude_home)
        .ok_or_else(|| HoldReason::Context("Claude home is unavailable".into()))?
        .join("projects");
    if !old.is_absolute() || !current.is_absolute() {
        return Err(HoldReason::Context("provider root is not absolute".into()));
    }
    let root = |path: &Path| match canonicalize(path) {
        Ok(root) => Ok(Some(root)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(file_error(path, error)),
    };
    match (root(old)?, root(&current)?) {
        (Some(a), Some(b)) if a == b => Ok(()),
        (None, None) if old == current => Ok(()),
        _ => Err(HoldReason::Conflict(
            "old provider root differs from current launch root".into(),
        )),
    }
}

fn runtime_read(
    state: &'static str,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> RuntimeRead {
    RuntimeRead {
        state,
        kind: format!("{:?}", binding.runtime_kind),
        session_id: binding.session_id.clone(),
        output_path: binding.output_path.clone(),
    }
}

fn select(
    request: &Request<'_>,
    database: Option<&ProviderSessionIds>,
    runtime: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
    readset: &mut ReadSet,
) -> Result<Option<SourceId>, HoldReason> {
    check_provider_root(request, readset.old_context.as_ref())?;
    let requested = checked_selector(request.requested_session_id)?;
    let cached = database
        .map(|db| checked_selector(db.claude_session_id.as_deref()))
        .transpose()?
        .flatten();
    let raw = database
        .map(|db| checked_selector(db.raw_provider_session_id.as_deref()))
        .transpose()?
        .flatten();
    let mut cwds = vec![request.working_dir.to_path_buf()];
    if let Some(cwd) = database.and_then(|db| normalized(db.cwd.as_deref())) {
        let cwd = PathBuf::from(cwd);
        if !cwds.contains(&cwd) {
            cwds.push(cwd);
        }
    }
    let cached_files = selector_files(&cwds, cached, request.claude_home, readset)?;
    let raw_files = selector_files(&cwds, raw, request.claude_home, readset)?;
    let selected = database.and_then(|db| {
        let raw_activity = raw_files.first().map(activity).map(|activity| {
            let same = raw.is_some_and(|raw| {
                normalized(db.raw_provider_transcript_watermark_session_id.as_deref()) == Some(raw)
            });
            let watermark = same
                .then_some(db.raw_provider_transcript_len_watermark)
                .flatten()
                .and_then(|len| u64::try_from(len).ok())
                .filter(|len| *len > 0);
            activity_with_read_only_growth(
                raw.unwrap_or_default(),
                activity,
                watermark,
                same && db.raw_provider_transcript_growth_proven,
            )
        });
        choose_provider_session_selector(
            cached,
            raw,
            cached.map(|_| cached_files.first().map_or_else(missing_activity, activity)),
            raw.map(|_| raw_activity.unwrap_or_else(missing_activity)),
            db.cache_entry_age_secs,
            crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
        )
    });
    if let Some(db) = &mut readset.database {
        db.selected = selected.map(str::to_string);
    }
    if requested
        .zip(selected)
        .is_some_and(|(requested, selected)| requested != selected)
    {
        return Err(HoldReason::Conflict(
            "requested and canonical selector differ".into(),
        ));
    }
    let runtime_id = if let Some(binding) = runtime {
        if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
            return Err(HoldReason::Runtime("binding is not Claude TUI".into()));
        }
        Some(
            checked_selector(binding.session_id.as_deref())?
                .ok_or_else(|| HoldReason::Runtime("binding has no native selector".into()))?,
        )
    } else {
        None
    };
    let selector = requested.or(selected).or(runtime_id);
    if selector
        .zip(runtime_id)
        .is_some_and(|(selected, live)| selected != live)
    {
        return Err(HoldReason::Conflict(
            "runtime and selected selector differ".into(),
        ));
    }
    if database.is_none()
        && (requested.is_some()
            || runtime.is_some()
            || readset.latest_binding.is_some()
            || readset.old_context.is_some())
    {
        return Err(HoldReason::Conflict(
            "canonical row absent with legacy evidence".into(),
        ));
    }
    let Some(selector) = selector else {
        if readset.latest_binding.is_some() || readset.old_context.is_some() {
            return Err(HoldReason::Conflict(
                "durable execution evidence has no resolved native selector".into(),
            ));
        }
        return Ok(None);
    };
    if readset
        .old_context
        .as_ref()
        .and_then(|context| context.expected_native_session_id.as_deref())
        .is_some_and(|expected| expected != selector)
        && readset
            .reconciled_source
            .as_ref()
            .is_none_or(|source| source.session_id != selector)
    {
        return Err(HoldReason::Conflict(
            "context and selected native selector differ".into(),
        ));
    }
    let mut files = selector_files(&cwds, Some(selector), request.claude_home, readset)?;
    if let Some(binding) = runtime {
        let path = Path::new(&binding.output_path);
        if let Some(file) = read_file(path)? {
            remember_file(readset, &file)?;
            if !files.iter().any(|seen| seen.path == file.path) {
                files.push(file);
            }
        } else {
            return Err(HoldReason::Runtime(
                "exact binding transcript is absent".into(),
            ));
        }
    }
    let logged_source = readset
        .latest_binding
        .as_ref()
        .map(|event| match &event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                Ok(source.clone())
            }
            _ => Err(HoldReason::History(
                "latest binding awaits another source".into(),
            )),
        })
        .transpose()?;
    if let Some(source) = &logged_source {
        if source.session_id != selector {
            return Err(HoldReason::Conflict(
                "logged and selected selector differ".into(),
            ));
        }
        let file = read_file(&source.path)?
            .ok_or_else(|| HoldReason::History("logged transcript is absent".into()))?;
        if (file.dev, file.ino) != (source.dev, source.ino) {
            return Err(HoldReason::Conflict("logged file identity changed".into()));
        }
        remember_file(readset, &file)?;
        if !files.iter().any(|seen| seen.path == file.path) {
            files.push(file);
        }
    }
    if files
        .windows(2)
        .any(|pair| (pair[0].dev, pair[0].ino) != (pair[1].dev, pair[1].ino))
    {
        return Err(HoldReason::Conflict(
            "exact paths name different transcript files".into(),
        ));
    }
    let path = runtime
        .map(|binding| PathBuf::from(&binding.output_path))
        .or_else(|| logged_source.as_ref().map(|source| source.path.clone()));
    let file = path
        .as_ref()
        .and_then(|path| files.iter().find(|file| file.path == *path))
        .or_else(|| files.first());
    Ok(file.map(|file| SourceId {
        session_id: selector.into(),
        path: file.path.clone(),
        dev: file.dev,
        ino: file.ino,
    }))
}

fn normalized(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn checked_selector(value: Option<&str>) -> Result<Option<&str>, HoldReason> {
    normalized(value)
        .map(|value| {
            uuid::Uuid::parse_str(value)
                .map(|_| value)
                .map_err(|_| HoldReason::InvalidSelector(value.into()))
        })
        .transpose()
}

fn selector_files(
    cwds: &[PathBuf],
    selector: Option<&str>,
    home: Option<&Path>,
    readset: &mut ReadSet,
) -> Result<Vec<FileRead>, HoldReason> {
    let Some(selector) = selector else {
        return Ok(Vec::new());
    };
    let home = home
        .map(Path::to_path_buf)
        .or_else(default_claude_home)
        .ok_or_else(|| HoldReason::Context("Claude home is unavailable".into()))?;
    let mut files: Vec<FileRead> = Vec::new();
    for cwd in cwds {
        if !cwd.is_absolute() {
            return Err(HoldReason::Conflict("candidate cwd is not absolute".into()));
        }
        let mut paths = Vec::new();
        match canonicalize(cwd) {
            Ok(canonical) => paths.push(canonical),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_error(cwd, error)),
        }
        if !paths.contains(cwd) {
            paths.push(cwd.clone());
        }
        for cwd in paths {
            let path = home
                .join("projects")
                .join(encode_project_path(&cwd))
                .join(format!("{selector}.jsonl"));
            if let Some(file) = read_file(&path)? {
                remember_file(readset, &file)?;
                if !files.iter().any(|seen| seen.path == path) {
                    files.push(file);
                }
            } else {
                if readset.files.iter().any(|seen| seen.path == path) {
                    return Err(HoldReason::Conflict(
                        "transcript disappeared while planning".into(),
                    ));
                }
                if !readset.missing_files.contains(&path) {
                    readset.missing_files.push(path);
                }
            }
        }
    }
    if files
        .windows(2)
        .any(|pair| (pair[0].dev, pair[0].ino) != (pair[1].dev, pair[1].ino))
    {
        return Err(HoldReason::Conflict(
            "cwd candidates name different transcript files".into(),
        ));
    }
    Ok(files)
}

fn read_file(path: &Path) -> Result<Option<FileRead>, HoldReason> {
    if !path.is_absolute() {
        return Err(HoldReason::Conflict(
            "transcript path is not absolute".into(),
        ));
    }
    let metadata = match metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(file_error(path, error)),
    };
    if !metadata.is_file() {
        return Err(HoldReason::Conflict(
            "transcript is not a regular file".into(),
        ));
    }
    let (dev, ino) = file_identity(&metadata);
    let modified = metadata
        .modified()
        .map_err(|error| file_error(path, error))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    let mtime_age_secs = Some(i64::try_from(age.as_secs()).unwrap_or(i64::MAX));
    Ok(Some(FileRead {
        path: path.into(),
        len: metadata.len(),
        dev,
        ino,
        mtime_age_secs,
    }))
}

fn metadata(path: &Path) -> io::Result<Metadata> {
    #[cfg(test)]
    if let Some(kind) = tests::metadata_error(path) {
        return Err(io::Error::from(kind));
    }
    fs::metadata(path)
}

fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(kind) = tests::canonical_error(path) {
        return Err(io::Error::from(kind));
    }
    fs::canonicalize(path)
}

fn file_error(path: &Path, error: io::Error) -> HoldReason {
    HoldReason::FileIo {
        path: path.into(),
        error: error.to_string(),
    }
}

fn remember_file(readset: &mut ReadSet, file: &FileRead) -> Result<(), HoldReason> {
    if readset.missing_files.contains(&file.path) {
        return Err(HoldReason::Conflict(
            "transcript appeared while planning".into(),
        ));
    }
    if let Some(seen) = readset.files.iter_mut().find(|seen| seen.path == file.path) {
        if (seen.dev, seen.ino) != (file.dev, file.ino) || seen.len > file.len {
            return Err(HoldReason::Conflict(
                "transcript changed while planning".into(),
            ));
        }
        *seen = file.clone();
    } else {
        readset.files.push(file.clone());
    }
    Ok(())
}

fn activity(file: &FileRead) -> SelectorFileActivity {
    SelectorFileActivity {
        exists: true,
        len: file.len,
        mtime_age_secs: file.mtime_age_secs,
        observed_growth_since_previous_sample: false,
    }
}

fn missing_activity() -> SelectorFileActivity {
    SelectorFileActivity {
        exists: false,
        len: 0,
        mtime_age_secs: None,
        observed_growth_since_previous_sample: false,
    }
}

#[cfg(test)]
#[path = "herdr_resume_plan/resolver_tests.rs"]
mod tests;

#[cfg(test)]
mod legacy_handoff;
