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
#[cfg(test)]
pub(crate) struct Installed(Option<Capability>);
#[cfg(test)]
impl Drop for Installed {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}
#[cfg(test)]
pub(crate) fn install(pool: sqlx::PgPool, pin: ExactEpisodePin) -> Installed {
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

pub(crate) fn observe_untouched(untouched: bool) {
    CURRENT.with(|current| {
        if let Some(capability) = current.borrow_mut().as_mut() {
            capability.untouched = untouched;
        }
    });
}

// Every executor exit converges here; error labels never decide whether input was sent.
pub(crate) fn dispatch<T>(run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
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
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::exact_episode::{Authority, EpisodeEvidence};
    use crate::services::tui_o::exact_pg::resolve_in_tx;

    fn pin() -> ExactEpisodePin {
        let record = crate::services::tui_o::exact_episode::tests::fixture().remove(0);
        let EpisodeEvidence::Pin(mut pin) = record.evidence else {
            panic!("fixture pin");
        };
        pin.episode = Uuid::new_v4();
        pin.source = None;
        pin
    }

    async fn persist_pin(pool: &sqlx::PgPool, pin: &ExactEpisodePin) {
        record_episode_evidence(
            true,
            pool,
            &EpisodeMetadata::new(
                pin.episode,
                Uuid::new_v4(),
                EpisodeEvidence::Pin(pin.clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn exact_submission_none_preserves_result_without_runtime() {
        assert_eq!(dispatch(|| Ok::<_, String>(73)), Ok(73));
        let error = "unchanged legacy error".to_string();
        assert_eq!(dispatch(|| Err::<(), _>(error.clone())), Err(error));
        begin_input().unwrap();
        observe_untouched(true);
        assert!(logical_key().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_submission_pg_executor_classifies_no_attempt_and_typed_refusal() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for scenario in 0..4 {
            let pin = pin();
            persist_pin(&pool, &pin).await;
            let p = pool.clone();
            let copy = pin.clone();
            tokio::task::spawn_blocking(move || {
                let _installed = install(p, copy);
                assert!(crate::services::provider::herdr_provider_terminal_only(None).is_some());
                let result = dispatch(|| {
                    if scenario > 0 {
                        begin_input()?;
                        observe_untouched(scenario == 1);
                    }
                    Err::<(), _>("executor exit after gate or launch".into())
                });
                assert_eq!(result, Err("executor exit after gate or launch".into()));
            })
            .await
            .unwrap();
            let mut connection = pool.acquire().await.unwrap();
            let result = resolve_in_tx(&mut connection, pin.episode).await.unwrap();
            assert_eq!(
                result.authority(),
                if scenario < 2 {
                    Authority::Policy
                } else {
                    Authority::Pending
                }
            );
            let closures: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events WHERE canonical_payload->>'episode'=$1 AND canonical_payload->'evidence'->>'type'='SubmissionClosed'")
                .bind(pin.episode.to_string()).fetch_one(&mut *connection).await.unwrap();
            assert_eq!(closures, i64::from(scenario < 2));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_submission_pg_reopen_process_resolves_policy_without_source() {
        if let Ok(url) = std::env::var("C2A_SUBMISSION_READER") {
            let pool = crate::db::postgres::connect_test_pool(&url, "submission child")
                .await
                .unwrap();
            let mut connection = pool.acquire().await.unwrap();
            let episode = std::env::var("C2A_SUBMISSION_EPISODE")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                resolve_in_tx(&mut connection, episode)
                    .await
                    .unwrap()
                    .authority(),
                Authority::Policy
            );
            assert!(
                logical_key().is_none(),
                "no process-local capability survives"
            );
            return;
        }
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let pin = pin();
        persist_pin(&pool, &pin).await;
        let p = pool.clone();
        let copy = pin.clone();
        tokio::task::spawn_blocking(move || {
            let _installed = install(p, copy);
            assert_eq!(
                dispatch(|| Err::<(), _>("launch refused".into())),
                Err("launch refused".into())
            );
        })
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "services::tui_o::exact_submission::tests::exact_submission_pg_reopen_process_resolves_policy_without_source", "--nocapture"])
            .env("C2A_SUBMISSION_READER", &db.database_url)
            .env("C2A_SUBMISSION_EPISODE", pin.episode.to_string())
            .env("AGENTDESK_ROOT_DIR", root.path()).output().unwrap();
        assert!(
            child.status.success(),
            "{} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            0,
            "PG-only policy replay writes nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_submission_pg_closed_admission_rejects_late_attempt() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let pin = pin();
        persist_pin(&pool, &pin).await;
        let closure = EpisodeMetadata::new(
            pin.episode,
            Uuid::new_v4(),
            EpisodeEvidence::SubmissionClosed {
                generation: pin.born_generation,
                basis: SubmissionBasis::NoAttempt,
                policy_version: pin.context.policy_version,
            },
        );
        let (entered, reached) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *COMMIT_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CommitBarrier {
            episode: pin.episode,
            entered,
            release: released,
        });
        let p = pool.clone();
        let copy = closure.clone();
        let close = tokio::spawn(async move { record_episode_evidence(true, &p, &copy).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), reached)
            .await
            .expect("closure reached commit barrier")
            .unwrap();
        let attempt = EpisodeMetadata::new(
            pin.episode,
            Uuid::new_v4(),
            EpisodeEvidence::InputAttemptBegun {
                nonce: Uuid::new_v4(),
            },
        );
        let p = pool.clone();
        let mut late =
            tokio::spawn(async move { record_episode_evidence(true, &p, &attempt).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut late)
                .await
                .is_err(),
            "late attempt must wait for closure transaction"
        );
        release.send(()).unwrap();
        close.await.unwrap().unwrap();
        assert!(
            late.await.unwrap().is_err(),
            "closed admission rejects racing attempt"
        );
        // Exact retry remains acknowledged, not a second closure.
        record_episode_evidence(true, &pool, &closure)
            .await
            .unwrap();
        let p = pool.clone();
        let copy = pin.clone();
        tokio::task::spawn_blocking(move || {
            let _installed = install(p, copy);
            assert!(begin_input().is_err());
            assert_eq!(
                dispatch(|| Err::<(), _>("input blocked".into())),
                Err("input blocked".into())
            );
        })
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            resolve_in_tx(&mut connection, pin.episode)
                .await
                .unwrap()
                .authority(),
            Authority::Policy
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_submission_pg_attempt_before_closure_never_becomes_no_attempt() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let pin = pin();
        persist_pin(&pool, &pin).await;
        let attempt = EpisodeMetadata::new(
            pin.episode,
            Uuid::new_v4(),
            EpisodeEvidence::InputAttemptBegun {
                nonce: Uuid::new_v4(),
            },
        );
        record_episode_evidence(true, &pool, &attempt)
            .await
            .unwrap();
        let closure = EpisodeMetadata::new(
            pin.episode,
            Uuid::new_v4(),
            EpisodeEvidence::SubmissionClosed {
                generation: pin.born_generation,
                basis: SubmissionBasis::NoAttempt,
                policy_version: pin.context.policy_version,
            },
        );
        assert!(
            record_episode_evidence(true, &pool, &closure)
                .await
                .is_err()
        );
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            resolve_in_tx(&mut connection, pin.episode)
                .await
                .unwrap()
                .authority(),
            Authority::Pending
        );
    }
}

#[cfg(test)]
pub(crate) use commit_barrier::{COMMIT_BARRIER, CommitBarrier, before_commit};
#[cfg(test)]
mod commit_barrier {
    use uuid::Uuid;
    pub(crate) struct CommitBarrier {
        pub episode: Uuid,
        pub entered: tokio::sync::oneshot::Sender<i32>,
        pub release: tokio::sync::oneshot::Receiver<()>,
    }
    pub(crate) static COMMIT_BARRIER: std::sync::Mutex<Option<CommitBarrier>> =
        std::sync::Mutex::new(None);
    pub(crate) async fn before_commit(
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
