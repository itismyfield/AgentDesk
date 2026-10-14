//! Typed observations from the existing inflight classifier and its single scan.
use super::super::finalizer_identity::BackfillOutcome;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ProbeCause {
    RootUnavailable,
    ReadDir(String),
    Entry(String),
    Read(String),
    Parse(String),
    Lock(store::LockCause),
    Backfill(BackfillOutcome),
    ReceiptHeld,
    HiddenStale(String),
    HiddenForeign(&'static str),
    HiddenMalformed,
    GateRefused,
    Excluded,
    InputProtected,
    ScopeMismatch,
}

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct ProbeObservation {
    pub(in crate::services::discord) path: PathBuf,
    pub(in crate::services::discord) cause: ProbeCause,
}

impl ProbeObservation {
    pub(super) fn root() -> Self {
        Self {
            path: PathBuf::new(),
            cause: ProbeCause::RootUnavailable,
        }
    }
    #[cfg(test)]
    pub(super) fn protected(channel: u64) -> Self {
        Self {
            path: PathBuf::from(channel.to_string()),
            cause: if channel == 0 {
                ProbeCause::ScopeMismatch
            } else {
                ProbeCause::InputProtected
            },
        }
    }
}

fn note(observations: &mut Vec<ProbeObservation>, path: &Path, cause: ProbeCause) {
    #[cfg(test)]
    if report_tests::mutant("inflight-hidden")
        && matches!(
            cause,
            ProbeCause::HiddenStale(_) | ProbeCause::HiddenForeign(_) | ProbeCause::HiddenMalformed
        )
    {
        return;
    }
    observations.push(ProbeObservation {
        path: path.to_owned(),
        cause,
    });
}

pub(super) fn log_observations(observations: &[ProbeObservation]) {
    for observation in observations {
        tracing::debug!(path = %observation.path.display(), cause = ?observation.cause, "inflight bulk observation");
    }
}

pub(in crate::services::discord) fn load_inflight_states_excluding_checked(
    provider: &ProviderKind,
    exclude_channel: impl Fn(u64) -> bool,
) -> InflightProbeLoad {
    let root = inflight_runtime_root();
    #[cfg(test)]
    let root = report_tests::root(root);
    let Some(root) = root else {
        return InflightProbeLoad {
            states: Vec::new(),
            complete: false,
            observations: vec![ProbeObservation::root()],
            #[cfg(test)]
            failure: Some(RestoreFailure::retryable("inflight root unavailable")),
        };
    };
    load_inflight_probe_from_root_excluding(&root, provider, exclude_channel)
}

pub(in crate::services::discord) fn load_inflight_states_excluding(
    provider: &ProviderKind,
    exclude_channel: impl Fn(u64) -> bool,
) -> Vec<InflightTurnState> {
    let report = super::super::load_inflight_states_excluding_checked(provider, exclude_channel);
    log_observations(&report.observations);
    report.states
}

/// Takes the row's lock and re-reads it; anything but a parsed row is already
/// the verdict.
fn relock(
    path: &Path,
    content: &str,
    observations: &mut Vec<ProbeObservation>,
) -> Result<(InflightTurnState, InflightStateFileLock), Box<RowVerdict>> {
    #[cfg(test)]
    BEFORE_ROW_LOCK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(path);
        }
    });
    let lock = store::lock_inflight_state_path_observed(path, false).map_err(|(_, cause)| {
        note(observations, path, ProbeCause::Lock(cause));
        Box::new(RowVerdict::Skip(true))
    })?;
    match fs::read_to_string(path) {
        Ok(locked) => match parse_inflight_state_content(&locked) {
            Ok(state) if state.herdr_before_start_stop.is_some() && !receipt_mutant("loader") => {
                note(observations, path, ProbeCause::ReceiptHeld);
                Err(Box::new(RowVerdict::Keep(state, false)))
            }
            Ok(state) => Ok((state, lock)),
            Err(_) if raw_before_start_receipt(&locked) && !receipt_mutant("malformed") => {
                note(observations, path, ProbeCause::ReceiptHeld);
                Err(Box::new(RowVerdict::Skip(true)))
            }
            Err(error) => {
                note(observations, path, ProbeCause::Parse(error.to_string()));
                Err(Box::new(RowVerdict::HideMalformed(
                    content.to_string(),
                    locked,
                    lock,
                )))
            }
        },
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                note(observations, path, ProbeCause::Read(error.to_string()));
            }
            Err(Box::new(RowVerdict::Skip(
                error.kind() != std::io::ErrorKind::NotFound,
            )))
        }
    }
}

pub(super) fn classify_observed(
    path: &Path,
    provider: &ProviderKind,
    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,
    retain_for_restore: bool,
    observations: &mut Vec<ProbeObservation>,
) -> RowVerdict {
    let generation = allocation.generation;
    let mut verdict = || -> Result<RowVerdict, Box<RowVerdict>> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                note(observations, path, ProbeCause::Read(error.to_string()));
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ failed to read inflight state file: {}",
                    path.display()
                );
                return Ok(RowVerdict::Skip(true));
            }
        };
        let (mut state, mut backfill) =
            match parse_inflight_state_content_with_finalizer_backfill(&content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    note(observations, path, ProbeCause::Parse(error.to_string()));
                    (relock(path, &content, observations)?.0, false)
                }
            };
        let receipt_held = raw_before_start_receipt(&content) && !receipt_mutant("loader");
        #[cfg(test)]
        let receipt_held = receipt_held && !report_tests::mutant("inflight-receipt");
        if receipt_held {
            note(observations, path, ProbeCause::ReceiptHeld);
            return Ok(RowVerdict::Keep(state, false));
        }
        let foreign = |row: &InflightTurnState| row.provider_kind().as_ref() != Some(provider);
        if foreign(&state) {
            let (locked, lock) = relock(path, &content, observations)?;
            if foreign(&locked) {
                let label = "load_inflight_states_from_root_provider_mismatch";
                return Ok(RowVerdict::HideForeign(label, state, locked, lock));
            }
            (state, backfill) = (locked, false);
        }
        let retain = retain_for_restore && state.restart_mode.is_none();
        #[cfg(test)]
        let retain = retain || (retain_for_restore && scoped_restore_mutant("stale-planned"));
        if retain || stale_removal_reason_for_path(path, &state, generation).is_none() {
            return Ok(RowVerdict::Keep(state, backfill));
        }
        let (locked, lock) = relock(path, &content, observations)?;
        if foreign(&locked) {
            let label = "load_inflight_states_from_root_stale_provider_mismatch";
            return Ok(RowVerdict::HideForeign(label, state, locked, lock));
        }
        Ok(
            match stale_removal_reason_for_path(path, &locked, generation) {
                Some(_)
                    if !retain_for_restore
                        && loader_gate_refuses_with_allocation(&locked, allocation) =>
                {
                    RowVerdict::KeepGateRefused(locked, lock)
                }
                Some(reason) => RowVerdict::HideStale(reason, state, locked, lock),
                None => RowVerdict::Keep(locked, false),
            },
        )
    };
    verdict().unwrap_or_else(|verdict| *verdict)
}

pub(super) fn load_inflight_probe_with_scope(
    root: &Path,
    provider: &ProviderKind,
    exclude_channel: impl Fn(u64) -> bool,
    scope: Option<u64>,
    observe_scoped: bool,
) -> InflightProbeLoad {
    let dir = inflight_provider_dir(root, provider);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            return InflightProbeLoad {
                states: Vec::new(),
                complete: error.kind() == std::io::ErrorKind::NotFound,
                observations: if error.kind() == std::io::ErrorKind::NotFound {
                    Vec::new()
                } else {
                    vec![ProbeObservation {
                        path: root.to_owned(),
                        cause: ProbeCause::ReadDir(error.to_string()),
                    }]
                },
                #[cfg(test)]
                failure: (error.kind() != std::io::ErrorKind::NotFound)
                    .then(|| RestoreFailure::retryable(error)),
            };
        }
    };
    let mut states = Vec::new();
    let mut complete = true;
    let mut observations = Vec::new();
    #[cfg(test)]
    let mut failure = None;
    let mut tmux_owners: HashMap<String, u64> = HashMap::new();
    let allocation = crate::services::discord::runtime_store::process_generation_binding();
    for entry in entries {
        #[cfg(test)]
        let entry = report_tests::entry(entry);
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                note(
                    &mut observations,
                    root,
                    ProbeCause::Entry(error.to_string()),
                );
                complete = false;
                continue;
            }
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if scope.is_some_and(|channel| channel_id_from_path(&path) != channel) {
            continue;
        }
        // Exclude by canonical filename before classification can lock or backfill an old row.
        let excluded = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u64>().ok())
            .is_some_and(&exclude_channel);
        #[cfg(test)]
        let excluded = excluded && !report_tests::mutant("inflight-exclusion");
        if excluded {
            let protected = crate::services::discord::input_runtime::fence::lookup(
                provider,
                channel_id_from_path(&path),
            )
            .is_some();
            note(
                &mut observations,
                &path,
                if protected {
                    ProbeCause::InputProtected
                } else {
                    ProbeCause::Excluded
                },
            );
            continue;
        }
        let verdict = classify_observed(
            &path,
            provider,
            allocation,
            scope.is_some(),
            &mut observations,
        );
        let state = match verdict {
            RowVerdict::Keep(state, true) => {
                let backfilled = if scope.is_some() {
                    if observe_scoped {
                        let (result, outcome) =
                            scoped_restore::backfill_scoped_restore_observed(root, &path, provider);
                        note(&mut observations, &path, ProbeCause::Backfill(outcome));
                        result
                    } else {
                        backfill_scoped_restore(root, &path, provider)
                    }
                } else {
                    let report =
                        super::super::finalizer_identity::backfill_finalizer_turn_id_observed(
                            root, &path, provider,
                        );
                    note(
                        &mut observations,
                        &path,
                        ProbeCause::Backfill(report.outcome),
                    );
                    report.state.ok_or_else(|| {
                        RestoreFailure::Rejected("legacy backfill incomplete".into())
                    })
                };
                complete &= backfilled.is_ok();
                #[cfg(test)]
                if scope.is_some() {
                    failure = backfilled.as_ref().err().cloned();
                }
                backfilled.unwrap_or(state)
            }
            RowVerdict::Keep(state, false) => state,
            RowVerdict::KeepGateRefused(state, _) => {
                note(&mut observations, &path, ProbeCause::GateRefused);
                state
            }
            RowVerdict::Skip(incomplete) => {
                complete &= !incomplete;
                #[cfg(test)]
                if incomplete && scope.is_some() {
                    failure = Some(RestoreFailure::retryable("scoped row read/lock incomplete"));
                }
                continue;
            }
            verdict @ (RowVerdict::HideStale(..)
            | RowVerdict::HideForeign(..)
            | RowVerdict::HideMalformed(..)) => {
                let cause = match &verdict {
                    RowVerdict::HideStale(reason, ..) => ProbeCause::HiddenStale(reason.clone()),
                    RowVerdict::HideForeign(label, ..) => ProbeCause::HiddenForeign(label),
                    _ => ProbeCause::HiddenMalformed,
                };
                note(&mut observations, &path, cause);
                complete &= scope.is_none();
                continue;
            }
        };
        if scope.is_some_and(|channel| state.channel_id != channel) {
            note(&mut observations, &path, ProbeCause::ScopeMismatch);
            complete = false;
            continue;
        }
        if let Some(tmux_session_name) = state
            .tmux_session_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if let Some(previous_channel_id) =
                tmux_owners.insert(tmux_session_name.to_string(), state.channel_id)
            {
                record_inflight_invariant(
                    false,
                    &state,
                    "inflight_tmux_one_to_one",
                    "src/services/discord/inflight.rs:load_inflight_states_from_root",
                    "one tmux session must not be owned by multiple inflight channel files",
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "previous_channel_id": previous_channel_id,
                        "current_channel_id": state.channel_id,
                        "path": path.display().to_string(),
                    }),
                );
            }
        }
        states.push(state);
    }
    #[cfg(test)]
    if report_tests::mutant("inflight-project") {
        observations.clear();
    }
    #[cfg(test)]
    let complete = complete || report_tests::mutant("inflight-complete");
    log_observations(&observations);
    InflightProbeLoad {
        states,
        complete,
        observations,
        #[cfg(test)]
        failure,
    }
}

#[cfg(test)]
#[path = "probe_report_tests.rs"]
pub(super) mod report_tests;
