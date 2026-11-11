//! An exact marker receipt can authorize one fresh executor observation, never a queued lock.
pub(crate) mod evidence;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedObservation, HostedOwner, HostedRecord, load_marker_owner_candidates_pg,
};
use crate::services::tmux_common::{self as tc, runtime_hint_fence::RuntimeHintFence};
use crate::services::turn_orchestrator::{MarkerRowExpectation, QueueIdentity};
use evidence::AbsenceStamp;
use std::sync::{Arc, Mutex};

pub(crate) type ClaimCheck = Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AStartResult {
    Refused {
        stage: &'static str,
        observed_identity: Option<QueueIdentity>,
        detail: String,
    },
    /// Another executor may have taken effect; keep custody until its actual result is observed.
    Unproved,
    /// Fresh authority was checked; this is not a provider execution or debt acknowledgement.
    Validated,
}
type ResultSink = Arc<Mutex<Option<AStartResult>>>;

#[derive(Clone)]
pub(crate) struct ACarrier {
    context: Arc<Mutex<Option<AStartContext>>>,
    result: ResultSink,
}
pub(crate) struct AStartContext {
    pub(crate) stamp: AbsenceStamp,
    current: ClaimCheck,
    result: ResultSink,
}

fn refused(result: &ResultSink, stage: &'static str, detail: String) -> String {
    let mut sink = result.lock().unwrap_or_else(|error| error.into_inner());
    if sink.is_none() {
        *sink = Some(AStartResult::Refused {
            stage,
            observed_identity: None,
            detail,
        });
    }
    format!("NotSent(\"marker_admission_{stage}\")")
}

impl ACarrier {
    pub(crate) fn new(stamp: AbsenceStamp, current: ClaimCheck) -> Self {
        let result = Arc::new(Mutex::new(None));
        Self {
            context: Arc::new(Mutex::new(Some(AStartContext {
                stamp,
                current,
                result: result.clone(),
            }))),
            result,
        }
    }
    pub(crate) fn take(&self) -> Result<AStartContext, String> {
        let context = self
            .context
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        context.ok_or_else(|| {
            *self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(AStartResult::Unproved);
            "NotSent(\"marker_admission_taken\")".into()
        })
    }
    pub(super) fn is_refused(&self) -> bool {
        matches!(
            *self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            Some(AStartResult::Refused { .. })
        )
    }
    pub(crate) fn take_result(&self) -> Option<AStartResult> {
        self.result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

impl AStartContext {
    pub(crate) fn refuse(&self, stage: &'static str, detail: impl Into<String>) -> String {
        refused(&self.result, stage, detail.into())
    }
    pub(crate) fn has_result(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
    pub(crate) async fn validate(
        &self,
        pool: &sqlx::PgPool,
        owner: &HostedOwner,
        row: &HostedObservation,
        fence: &RuntimeHintFence,
    ) -> Result<(), String> {
        let check = async {
            if !(self.current)() || owner != &self.stamp.owner {
                return Err("marker front or owner changed".to_owned());
            }
            let MarkerRowExpectation::Legacy {
                owner: expected,
                row_id,
                exact_session_key,
            } = &self.stamp.expected
            else {
                return Err("marker Missing row changed before launch".into());
            };
            if row.session_id() != *row_id
                || row.record != HostedRecord::Legacy
                || expected != &owner.into()
            {
                return Err("marker row changed before launch".into());
            }
            let candidates = load_marker_owner_candidates_pg(pool, &owner.logical_key).await?;
            if candidates.len() != 1
                || !candidates.iter().all(|candidate| {
                    candidate.session_id == *row_id
                        && candidate.raw.is_none()
                        && candidate.identity
                            == [
                                Some(owner.provider.clone()),
                                Some("discord_channel".into()),
                                Some(owner.discord_token_hash.clone()),
                                Some(owner.channel_id.clone()),
                            ]
                        && (candidate.session_key.as_deref() == Some(exact_session_key.as_str())
                            || candidate.aliases.contains(exact_session_key))
                })
            {
                return Err("marker owner candidates changed".into());
            }
            tc::try_with_tmux_source_authority(&owner.logical_key, |authority| {
                fence.validate(&owner.logical_key)?;
                let current = AbsenceStamp::capture_under_source(
                    owner,
                    self.stamp.expected.clone(),
                    self.stamp.front.clone(),
                    self.stamp.epoch.clone(),
                    authority,
                )?;
                if !(self.current)()
                    || !self.stamp.matches(&current)
                    || current.paths.iter().any(|path| path.file.is_some())
                {
                    return Err("marker execution evidence changed".into());
                }
                vacant(&owner.logical_key)
            })
            .ok_or("marker Source busy")?
        }
        .await;
        if let Err(detail) = check {
            return Err(self.refuse("stale", detail));
        }
        let mut result = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if result.is_some() {
            return Err("NotSent(\"marker_admission_consumed\")".into());
        }
        *result = Some(AStartResult::Validated);
        Ok(())
    }
}

fn vacant(logical: &str) -> Result<(), String> {
    if crate::services::session_backend::process_session_pid(logical).is_some()
        || crate::services::platform::tmux::session_presence(logical)
            != crate::services::platform::tmux::SessionPresence::Missing
    {
        return Err("marker session vacancy is not proven".into());
    }
    Ok(())
}

/// The caller keeps the physical fence after its database owner checks; only exact hints are removed.
pub(crate) fn cleanup(
    expected: &AbsenceStamp,
    fence: &RuntimeHintFence,
) -> Result<AbsenceStamp, String> {
    let logical = &expected.owner.logical_key;
    tc::try_with_tmux_source_authority(logical, |authority| {
        fence.validate(logical)?;
        vacant(logical)?;
        let current = AbsenceStamp::capture_under_source(
            &expected.owner,
            expected.expected.clone(),
            expected.front.clone(),
            expected.epoch.clone(),
            authority,
        )?;
        if !expected.permits_partial(&current) {
            return Err("marker cleanup evidence changed".into());
        }
        for (index, path) in current.paths.iter().enumerate() {
            if current.paths[..index].iter().any(|old| {
                old.parent == path.parent && old.path.file_name() == path.path.file_name()
            }) {
                continue;
            }
            if path.file.is_some() {
                fence.validate(logical)?;
                if evidence::HintPath::observe(&path.path)? != *path {
                    return Err("marker entry changed before unlink".into());
                }
            }
            path.unlink()?;
        }
        let clean = AbsenceStamp::capture_under_source(
            &expected.owner,
            expected.expected.clone(),
            expected.front.clone(),
            expected.epoch.clone(),
            authority,
        )?;
        if !expected.permits_partial(&clean) || clean.paths.iter().any(|path| path.file.is_some()) {
            return Err("marker cleanup incomplete".into());
        }
        Ok(clean)
    })
    .ok_or("marker Source busy")?
}

#[cfg(test)]
#[path = "legacy_hint_tests.rs"]
mod tests;
