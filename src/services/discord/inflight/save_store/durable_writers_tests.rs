//! Inflight row writers flush nothing past a failed rename, flush the new bytes once published and
//! keep the saved outcome when that flush fails; removals and the archive move sync their dirs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::{SharedTestEnvLockGuard, acquire_shared_test_env_lock};
use crate::services::discord::inflight::{anchor_repost, budget, clear_store, restart_mark};
use crate::services::discord::runtime_store::durable_probe::{Event, Gate, Plan, Step, Watch};
use crate::services::discord::runtime_store::{PARENT_DIR_FSYNC_FLUSHES, rename_fault};

const P: ProviderKind = ProviderKind::Codex;

/// One runtime root named by the process env; other env tests wait for the guard.
struct Env {
    temp: tempfile::TempDir,
    root: PathBuf,
    _env: TestEnvVarGuard,
    _lock: SharedTestEnvLockGuard,
}

impl Env {
    fn new() -> Self {
        let lock = acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
        let root = inflight_runtime_root().unwrap();
        Self {
            temp,
            root,
            _env: env,
            _lock: lock,
        }
    }

    fn path(&self, channel_id: u64) -> PathBuf {
        inflight_state_path(&self.root, &P, channel_id)
    }

    /// A row with a transcript, a nonce and a delivered body, so every guarded writer accepts it.
    fn row(&self, channel_id: u64) -> InflightTurnState {
        let output = self.temp.path().join(format!("{channel_id}.jsonl"));
        fs::write(&output, "x".repeat(64)).unwrap();
        let mut row = InflightTurnState::new(
            P,
            channel_id,
            Some("durable".into()),
            7,
            channel_id + 1,
            channel_id + 2,
            "hello".into(),
            Some("session".into()),
            Some(format!("AgentDesk-codex-durable-{channel_id}")),
            Some(output.display().to_string()),
            None,
            8,
        );
        row.turn_start_offset = Some(16);
        row.turn_nonce = Some(format!("durable-{channel_id}"));
        row.full_response = "durable delivered body".into();
        row.response_sent_offset = row.full_response.len();
        row
    }

    /// Writes the row with the plain fixture writer and returns it as stored, with its bytes.
    fn seed(&self, channel_id: u64) -> (InflightTurnState, String) {
        save_inflight_state_in_root(&self.root, &self.row(channel_id)).unwrap();
        let path = self.path(channel_id);
        let bytes = fs::read_to_string(&path).unwrap();
        (load_inflight_state_unlocked(&path).unwrap(), bytes)
    }
}

/// A writer under test: whether it starts from a stored row, whether it then removes the row,
/// and the call returning whether the writer reported its save.
struct Site {
    name: &'static str,
    seeded: bool,
    removes: bool,
    write: fn(&Env, &InflightTurnState) -> bool,
}

fn identity(row: &InflightTurnState) -> InflightTurnIdentity {
    InflightTurnIdentity::from_state(row)
}

fn sites() -> Vec<Site> {
    vec![
        Site {
            name: "restart mode marker",
            seeded: true,
            removes: false,
            write: |env, row| {
                let outcome = set_inflight_restart_mode_under_lock(
                    &env.path(row.channel_id),
                    InflightRestartMode::DrainRestart,
                );
                matches!(outcome, restart_mark::MarkOutcome::Marked)
            },
        },
        Site {
            name: "anchor repost guard",
            seeded: true,
            removes: false,
            write: |env, row| {
                anchor_repost::mark_anchor_reposted_if_matches_identity_in_root(
                    &env.root,
                    &P,
                    row.channel_id,
                    &identity(row),
                    row.turn_start_offset,
                ) == GuardedSaveOutcome::Saved
            },
        },
        Site {
            name: "recovery attempt counter",
            seeded: true,
            removes: false,
            write: |env, row| {
                budget::bump_recovery_relay_attempts_if_matches_identity_in_root(
                    &env.root,
                    &P,
                    row.channel_id,
                    &identity(row),
                    row.turn_start_offset,
                ) == GuardedSaveOutcome::Saved
            },
        },
        Site {
            name: "delivery mirror",
            seeded: true,
            removes: true,
            write: |env, row| {
                let body = "mirrored delivered body";
                clear_store::clear_inflight_state_if_matches_identity_after_delivery_in_root(
                    &env.root,
                    &P,
                    row.channel_id,
                    &identity(row),
                    row.turn_start_offset,
                    body,
                    body.len(),
                    row.last_offset + 4,
                )
                .1
            },
        },
        Site {
            name: "offset refresh",
            seeded: true,
            removes: false,
            write: |env, row| {
                clear_store::refresh_inflight_last_offset_if_matches_identity_in_root(
                    &env.root,
                    &P,
                    row.channel_id,
                    &identity(row),
                    row.turn_start_offset,
                    row.output_path.as_deref().unwrap(),
                    Some(row.current_msg_id),
                    row.last_offset + 4,
                    row.effective_relay_owner_kind(),
                )
            },
        },
        Site {
            name: "create-new",
            seeded: false,
            removes: false,
            write: |env, row| save_inflight_state_create_new_in_root(&env.root, row).is_ok(),
        },
        Site {
            name: "if-absent",
            seeded: false,
            removes: false,
            write: |env, row| save_inflight_state_if_absent_in_root(&env.root, row) == Ok(true),
        },
        Site {
            name: "delivery rewind",
            seeded: true,
            removes: false,
            write: |env, row| {
                let mut desired = row.clone();
                desired.full_response = "rewound error guidance".into();
                desired.response_sent_offset = 0;
                save_inflight_delivery_rewind_if_matches_identity_in_root(
                    &env.root,
                    &desired,
                    StreamRelayAuthority::from_state(row),
                    InflightDeliveryRewindReason::TerminalErrorReset,
                ) == Ok(true)
            },
        },
        Site {
            name: "recovery anchor",
            seeded: true,
            removes: false,
            write: |_, row| {
                bind_recovery_anchor_if_matches_identity(
                    &P,
                    row.channel_id,
                    (
                        &identity(row),
                        row.turn_start_offset,
                        row.turn_nonce.as_deref(),
                    ),
                    row.current_msg_id,
                    Some(row.current_msg_len),
                    row.current_msg_id + 100,
                    5,
                    None,
                    None,
                ) == GuardedSaveOutcome::Saved
            },
        },
        Site {
            name: "rebind adoption",
            seeded: true,
            removes: false,
            write: |env, row| {
                let mut adopted = row.clone();
                adopted.tmux_session_name = Some("AgentDesk-codex-durable-adopted".into());
                super::super::identity_gate::save_existing_inflight_rebind_adoption_impl_in_root(
                    &env.root,
                    &adopted,
                    &identity(row),
                    None,
                    row.turn_start_offset,
                    None,
                ) == GuardedSaveOutcome::Saved
            },
        },
        Site {
            name: "stream persist",
            seeded: true,
            removes: false,
            write: |env, row| {
                let path = env.path(row.channel_id);
                let _lock = lock_inflight_state_path(&path).unwrap();
                let mut next = row.clone();
                next.full_response.push_str(" and more");
                let saved = store::persist_under_lock_with_snapshot(
                    &env.root,
                    &path,
                    &next,
                    "durable_writers_tests",
                );
                matches!(saved, Ok(Some(_)))
            },
        },
    ]
}

/// Prepares the site's starting state on a fresh channel and returns the row the writer gets
/// with the bytes stored before it ran.
fn prepare(env: &Env, site: &Site, channel_id: u64) -> (InflightTurnState, Option<String>) {
    if site.seeded {
        let (row, bytes) = env.seed(channel_id);
        (row, Some(bytes))
    } else {
        (env.row(channel_id), None)
    }
}

fn flushes(events: &[Event], target: &Path) -> Vec<Event> {
    let flush = [Step::ParentOpen, Step::ParentSync, Step::Ancestors];
    events
        .iter()
        .filter(|event| event.target == target && flush.contains(&event.step))
        .cloned()
        .collect()
}

/// Every writer: a real rename failure is its failure outcome with no directory flush while the
/// row exists; a publication flushes the parent with the new bytes visible, and the chain above.
#[test]
fn p2_writers_sync_only_after_successful_rename() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    for (i, site) in sites().iter().enumerate() {
        let env = Env::new();
        let watch = Watch::new(env.temp.path(), Plan::default());
        let channel_id = 7_234_100 + i as u64;
        let path = env.path(channel_id);
        let (row, before) = prepare(&env, site, channel_id);
        let armed = rename_fault::Armed::new(&path);
        assert!(
            !(site.write)(&env, &row),
            "{}: saved past a failed rename",
            site.name
        );
        let (renames, errors) = armed.evidence();
        assert!(
            renames >= 1 && !errors.is_empty(),
            "{}: no real rename failure",
            site.name
        );
        drop(armed);
        let failed = flushes(&watch.events(), &path);
        assert!(
            failed.iter().all(|event| event.target_bytes.is_none())
                && (site.removes || failed.is_empty()),
            "{}: flushed after a failed rename: {failed:?}",
            site.name
        );
        if !site.removes {
            assert_eq!(fs::read_to_string(&path).ok(), before, "{}", site.name);
        }

        let channel_id = channel_id + 50;
        let path = env.path(channel_id);
        let (row, before) = prepare(&env, site, channel_id);
        let from = watch.events().len();
        assert!((site.write)(&env, &row), "{}: refused its save", site.name);
        let events = watch.events().split_off(from);
        let published = flushes(&events, &path).into_iter().any(|event| {
            event.step == Step::ParentSync
                && event.ok
                && event.target_bytes.is_some()
                && event.target_bytes != before
        });
        assert!(published, "{}: no parent flush of the new bytes", site.name);
        let ancestors: Vec<PathBuf> = events
            .iter()
            .filter(|event| event.step == Step::Ancestors && event.ok)
            .map(|event| event.dir.clone())
            .collect();
        assert!(
            ancestors.contains(&env.root),
            "{}: {ancestors:?}",
            site.name
        );
    }
}

/// Every writer keeps its saved outcome and the new bytes when the flush after the rename
/// fails, and publishes exactly once: no retry, rollback or second publication.
#[test]
fn p2_post_rename_sync_failure_keeps_saved_outcome() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    for (i, site) in sites().iter().enumerate() {
        let env = Env::new();
        let channel_id = 7_234_200 + i as u64;
        let path = env.path(channel_id);
        let (row, before) = prepare(&env, site, channel_id);
        let plan = Plan {
            fail: vec![Step::ParentSync],
            ..Plan::default()
        };
        let watch = Watch::new(env.temp.path(), plan);
        assert!(
            (site.write)(&env, &row),
            "{}: a flush failure undid the save",
            site.name
        );
        let published = watch
            .events()
            .into_iter()
            .filter(|event| event.target == path && event.step == Step::FileSync)
            .count();
        assert_eq!(published, 1, "{}: publications", site.name);
        if !site.removes {
            let after = fs::read_to_string(&path).ok();
            assert!(
                after.is_some() && after != before,
                "{}: rolled back",
                site.name
            );
        }
    }
}

/// A create-new whose flush fails after the rename still returns `Ok` (no retry into
/// `AlreadyExists`, the observer step runs once); the next create of the same row is refused.
#[test]
fn create_new_observer_runs_once_after_publication() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = Env::new();
    let row = env.row(7_234_300);
    let plan = Plan {
        fail: vec![Step::ParentSync, Step::Ancestors],
        ..Plan::default()
    };
    let watch = Watch::new(env.temp.path(), plan);
    save_inflight_state_create_new_in_root(&env.root, &row).unwrap();
    let path = env.path(row.channel_id);
    let published = watch
        .events()
        .into_iter()
        .filter(|event| event.target == path && event.step == Step::FileSync)
        .count();
    assert_eq!(published, 1);
    assert!(matches!(
        save_inflight_state_create_new_in_root(&env.root, &row),
        Err(CreateNewInflightError::AlreadyExists)
    ));
    assert!(!save_inflight_state_if_absent_in_root(&env.root, &row).unwrap());
}

/// A removal of the inflight row whose result the caller reported.
type Retire = fn(&Env, &InflightTurnState) -> bool;

fn retirements() -> Vec<(&'static str, Retire)> {
    vec![
        ("unconditional clear", |_, row| {
            clear_inflight_state(&P, row.channel_id)
        }),
        ("user-message clear", |env, row| {
            clear_store::clear_inflight_state_if_matches_in_root(
                &env.root,
                &P,
                row.channel_id,
                row.user_msg_id,
            ) == GuardedClearOutcome::Cleared
        }),
        ("identity clear", |env, row| {
            clear_store::clear_inflight_state_if_matches_identity_in_root(
                &env.root,
                &P,
                row.channel_id,
                &identity(row),
            ) == GuardedClearOutcome::Cleared
        }),
        ("generation clear", |env, row| {
            clear_store::clear_inflight_state_if_matches_identity_generation_in_root(
                &env.root,
                &P,
                row.channel_id,
                &identity(row),
                row.effective_finalizer_turn_id(),
                &row.updated_at,
                row.save_generation,
            ) == GuardedClearOutcome::Cleared
        }),
        ("policy op", |env, row| {
            matches!(
                remove_inflight_row_for_policy_op(&env.path(row.channel_id)),
                PolicyOpRemoveOutcome::Removed
            )
        }),
    ]
}

/// A removal flushes the parent once the row is gone, and a failed flush neither restores the
/// row nor turns the removal into a failure.
#[test]
fn inflight_retirement_syncs_without_resurrection() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    for (i, (name, retire)) in retirements().into_iter().enumerate() {
        let env = Env::new();
        let channel_id = 7_234_400 + i as u64;
        let (row, _) = env.seed(channel_id);
        let path = env.path(channel_id);
        let watch = Watch::new(env.temp.path(), Plan::default());
        assert!(retire(&env, &row), "{name}: not removed");
        let synced = flushes(&watch.events(), &path)
            .into_iter()
            .any(|e| e.step == Step::ParentSync && e.ok && e.target_bytes.is_none());
        assert!(synced && !path.exists(), "{name}: removal not synced");
        drop(watch);

        let (row, _) = env.seed(channel_id + 50);
        let path = env.path(channel_id + 50);
        let plan = Plan {
            fail: vec![Step::ParentSync],
            ..Plan::default()
        };
        let _failing = Watch::new(env.temp.path(), plan);
        assert!(
            retire(&env, &row),
            "{name}: a flush failure undid the removal"
        );
        assert!(!path.exists(), "{name}: the row came back");
    }
}

/// The archive move flushes the archive directory, the new `archive` entry above it and the
/// provider directory the row left.
#[test]
fn archive_move_syncs_source_and_destination() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = Env::new();
    let (row, bytes) = env.seed(7_234_500);
    let path = env.path(row.channel_id);
    let watch = Watch::new(env.temp.path(), Plan::default());
    let outcome = clear_store::archive_inflight_state_if_matches_identity_generation_in_root(
        &env.root,
        &P,
        row.channel_id,
        &identity(&row),
        &row.updated_at,
        row.save_generation,
        "durable test",
    );
    assert_eq!(outcome, GuardedClearOutcome::Cleared);
    let archive = env.root.join("archive");
    let moved: Vec<PathBuf> = fs::read_dir(&archive)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(moved.len(), 1);
    assert_eq!(fs::read_to_string(&moved[0]).unwrap(), bytes);
    assert!(!path.exists());
    let synced = |dir: &Path| watch.flushed(Step::ParentSync).contains(&dir.to_path_buf());
    assert!(synced(&archive), "destination");
    assert!(synced(path.parent().unwrap()), "source");
    assert!(
        watch.flushed(Step::Ancestors).contains(&env.root),
        "new archive entry"
    );
}

/// A row publication through the helper still writes the `agentdesk::inflight_write` audit
/// line when it replaces a stored row.
#[test]
fn inflight_row_publication_keeps_the_write_audit_log() {
    let env = Env::new();
    let (row, _) = env.seed(7_234_600);
    let logs = capture_logs(|| {
        assert!(
            budget::bump_recovery_relay_attempts_if_matches_identity_in_root(
                &env.root,
                &P,
                row.channel_id,
                &identity(&row),
                row.turn_start_offset,
            ) == GuardedSaveOutcome::Saved
        );
    });
    assert!(
        logs.contains("discord inflight state row content replaced in place"),
        "{logs}"
    );
}

/// One channel's row flush held in the kernel does not hold another channel's row save, its
/// heartbeat-style persist or an unrelated publication.
#[test]
fn blocked_inflight_sync_other_channel_keeps_progress() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = Env::new();
    let (a, _) = env.seed(7_234_700);
    let (b, _) = env.seed(7_234_701);
    let gate = Arc::new(Gate::default());
    let plan = Plan {
        block: Some(gate.clone()),
        ..Plan::default()
    };
    let _blocked = Watch::new(&env.path(a.channel_id), plan);
    let _release = Release(gate.clone());
    std::thread::scope(|scope| {
        let blocked = scope.spawn(|| {
            budget::bump_recovery_relay_attempts_if_matches_identity_in_root(
                &env.root,
                &P,
                a.channel_id,
                &identity(&a),
                a.turn_start_offset,
            )
        });
        assert!(
            gate.wait_entered(Duration::from_secs(10)),
            "A reached its flush"
        );
        let path = env.path(b.channel_id);
        let lock = lock_inflight_state_path(&path).unwrap();
        let mut next = b.clone();
        next.full_response.push_str(" heartbeat");
        let saved = store::persist_under_lock_with_snapshot(&env.root, &path, &next, "durable");
        assert!(matches!(saved, Ok(Some(_))), "B's persist is not held by A");
        drop(lock);
        let created = env.row(7_234_702);
        save_inflight_state_create_new_in_root(&env.root, &created).unwrap();
        assert!(!blocked.is_finished(), "A is still inside its flush");
        gate.release();
        assert_eq!(blocked.join().unwrap(), GuardedSaveOutcome::Saved);
    });
}

/// Lets a held flush go even when an assertion fails first.
struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn capture_logs(run: impl FnOnce()) -> String {
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || Capture(output.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
    String::from_utf8(bytes).unwrap()
}

struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
