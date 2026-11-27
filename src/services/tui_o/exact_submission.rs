use super::exact_episode::{EpisodeEvidence, EpisodeMetadata, ExactEpisodePin, SubmissionBasis};
use super::exact_pg::record_episode_evidence;
use std::cell::RefCell;
use uuid::Uuid;

#[derive(Clone)]
struct Capability {
    pool: sqlx::PgPool,
    pin: ExactEpisodePin,
    attempt: Option<Uuid>,
    input_blocked: bool,
    untouched: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<Capability>> = const { RefCell::new(None) };
}

// Installation remains test-only until association and preservation guards are deployed.
#[cfg(any(test, feature = "test-support"))]
pub struct Installed(Option<Capability>);
#[cfg(any(test, feature = "test-support"))]
impl Drop for Installed {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}
#[cfg(any(test, feature = "test-support"))]
pub fn install(pool: sqlx::PgPool, pin: ExactEpisodePin) -> Installed {
    Installed(CURRENT.with(|current| {
        current.replace(Some(Capability {
            pool,
            pin,
            attempt: None,
            input_blocked: false,
            untouched: false,
        }))
    }))
}

#[cfg(any(test, feature = "test-support"))]
pub fn logical_key_for_test() -> Option<String> {
    logical_key()
}

pub(crate) fn logical_key() -> Option<String> {
    CURRENT.with(|current| current.borrow().as_ref().map(|c| c.pin.owner.clone()))
}

fn append(capability: &Capability, evidence: EpisodeEvidence) -> Result<(), String> {
    let metadata = EpisodeMetadata::new(capability.pin.episode, Uuid::new_v4(), evidence);
    tokio::runtime::Handle::try_current()
        .map_err(|error| error.to_string())?
        .block_on(record_episode_evidence(true, &capability.pool, &metadata))?
        .ok_or_else(|| "strict evidence ACK unavailable".to_owned())?;
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn begin_input_for_test() -> Result<(), String> {
    begin_input()
}

pub(crate) fn begin_input() -> Result<(), String> {
    let capability = CURRENT.with(|current| current.borrow().clone());
    let Some(capability) = capability else {
        return Ok(());
    };
    let nonce = Uuid::new_v4();
    CURRENT.with(|current| {
        if let Some(capability) = current.borrow_mut().as_mut() {
            capability.input_blocked = true;
        }
    });
    append(&capability, EpisodeEvidence::InputAttemptBegun { nonce })?;
    CURRENT.with(|current| {
        if let Some(capability) = current.borrow_mut().as_mut() {
            capability.attempt = Some(nonce);
            capability.input_blocked = false;
            capability.untouched = false;
        }
    });
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn observe_untouched_for_test(untouched: bool) {
    observe_untouched(untouched)
}

pub(crate) fn observe_untouched(untouched: bool) {
    CURRENT.with(|current| {
        if let Some(capability) = current.borrow_mut().as_mut() {
            capability.untouched = untouched;
        }
    });
}

// Every executor exit converges here; error labels never decide whether input was sent.
pub fn dispatch<T>(run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let result = run();
    let capability = CURRENT.with(|current| current.borrow().clone());
    let Some(capability) = capability else {
        return result;
    };
    if capability.input_blocked {
        return result;
    }
    let basis = match capability.attempt {
        None => SubmissionBasis::NoAttempt,
        Some(nonce) if capability.untouched => SubmissionBasis::GateRefused { nonce },
        Some(_) => return result,
    };
    append(
        &capability,
        EpisodeEvidence::SubmissionClosed {
            generation: capability.pin.born_generation,
            basis,
            policy_version: capability.pin.context.policy_version,
        },
    )?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_submission_none_preserves_result_without_runtime() {
        assert_eq!(dispatch(|| Ok::<_, String>(73)), Ok(73));
        let error = "unchanged legacy error".to_string();
        assert_eq!(dispatch(|| Err::<(), _>(error.clone())), Err(error));
        begin_input().unwrap();
        observe_untouched(true);
        assert!(logical_key().is_none());
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use commit_barrier::{COMMIT_BARRIER, CommitBarrier, before_commit};
#[cfg(any(test, feature = "test-support"))]
mod commit_barrier {
    use uuid::Uuid;
    pub struct CommitBarrier {
        pub episode: Uuid,
        pub entered: tokio::sync::oneshot::Sender<i32>,
        pub release: tokio::sync::oneshot::Receiver<()>,
    }
    pub static COMMIT_BARRIER: std::sync::Mutex<Option<CommitBarrier>> =
        std::sync::Mutex::new(None);
    pub async fn before_commit(
        transaction: &mut sqlx::PgConnection,
        payload: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let barrier = {
            let mut slot = COMMIT_BARRIER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.as_ref().is_some_and(|b| {
                payload.get("episode").and_then(serde_json::Value::as_str)
                    == Some(b.episode.to_string().as_str())
            }) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(barrier) = barrier {
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *transaction)
                .await?;
            let _ = barrier.entered.send(pid);
            let _ = barrier.release.await;
        }
        Ok(())
    }
}
