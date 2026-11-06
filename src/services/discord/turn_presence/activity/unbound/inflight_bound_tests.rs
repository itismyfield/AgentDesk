use super::super::review_tests::{PAUSES, Pause, PauseGuard, Rig};
use super::*;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::turn_presence::activity::tests::{log_turn_source, settled_reason};
use crate::services::discord::turn_presence::supervisor::tests::LiveSupervisor;
use crate::services::platform::tmux::session_inventory::tests::ChannelCommand;
use std::io::Write;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// The relaunched channel's own turn: every input names its one live session.
    Own,
    /// The in-flight row names a session the channel's other inputs do not.
    RowConflict,
    /// The session is restored as another channel's.
    ForeignOwner,
    /// The channel's canonical sessions row is another session's.
    CanonicalElsewhere,
    /// No sessions row says whose the session is.
    NoSessionRow,
    /// No local claim, while two other registered runtimes restore it for two other channels.
    RegisteredConflict,
    /// Another registered runtime claims it and the routine ownership read fails.
    ForeignClaimUnread,
}

/// What presence settles on, what a supervisor tick publishes, and the held Unknown health shows.
type Observed3 = (&'static str, String, Option<(String, Option<String>)>);

/// A relaunched tmux channel mid-turn with no watcher yet, its four candidates naming one live
/// session `name`, as a case leaves its row, owners and sessions rows.
struct Relaunched {
    rig: Rig,
    name: String,
    _registry: Option<(Arc<HealthRegistry>, Vec<Arc<SharedData>>)>,
    _inventory: ChannelCommand,
    _turn: crate::services::tui_o::turn_mode::TestConfirmation,
}

impl Relaunched {
    async fn new(id: u64, case: Case) -> Self {
        let mut rig = Rig::new(id, false, false).await;
        let provider = ProviderKind::Claude;
        let name = provider.build_tmux_session_name(&format!("s1e-{id}"));
        let pool = rig.shared.pg_pool.clone().unwrap();
        let key = |token: &str, name: &str| {
            adk_session::build_namespaced_session_key(token, &provider, name)
        };
        let token = rig.shared.token_hash.clone();
        match case {
            Case::CanonicalElsewhere => {
                let earlier = provider.build_tmux_session_name(&format!("before-clear-{id}"));
                inflight::seed_session_row_hashed(&pool, &key(&token, &earlier), id, &token, None)
                    .await;
            }
            Case::NoSessionRow => {}
            _ => {
                inflight::seed_session_row_hashed(&pool, &key(&token, &name), id, &token, None)
                    .await
            }
        }
        let claims: &[u64] = match case {
            Case::RegisteredConflict => &[id + 1, id + 2],
            Case::ForeignClaimUnread => &[id + 1],
            _ => &[],
        };
        let registry = if claims.is_empty() {
            None
        } else {
            let registry = Arc::new(HealthRegistry::new());
            Arc::get_mut(&mut rig.shared).unwrap().health_registry = Arc::downgrade(&registry);
            registry.register("claude".into(), rig.shared.clone()).await;
            let mut runtimes = Vec::new();
            for owner in claims {
                let mut runtime = crate::services::discord::make_shared_data_for_tests_with_storage(
                    Some(pool.clone()),
                );
                let mutable = Arc::get_mut(&mut runtime).unwrap();
                mutable.provider = provider.clone();
                mutable.token_hash = format!("discord_{owner:016x}");
                mutable.health_registry = Arc::downgrade(&registry);
                registry.register("claude".into(), runtime.clone()).await;
                runtime
                    .tmux_watchers
                    .restore_owner_channel_for_tmux_session(&name, ChannelId::new(*owner));
                runtimes.push(runtime);
            }
            Some((registry, runtimes))
        };
        if case == Case::ForeignClaimUnread {
            sqlx::query(
                "ALTER TABLE routine_runs RENAME COLUMN owned_tmux_session TO owned_tmux_unread",
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        let row_name = match case {
            Case::RowConflict => provider.build_tmux_session_name(&format!("other-{id}")),
            _ => name.clone(),
        };
        write_row(id, &row_name);
        if case == Case::ForeignOwner {
            rig.shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&name, ChannelId::new(id + 1));
        }
        let transcript = rig.root.path().join("resumed.jsonl");
        let mut file = std::fs::File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","uuid":"a","message":{{"content":"turn"}}}}"#
        )
        .unwrap();
        log_turn_source(&provider, id, &name, "resumed", &transcript);
        let inventory = rig.root.path().join("inventory.txt");
        std::fs::write(&inventory, format!("{name}\n")).unwrap();
        let executable = rig.root.path().join("inventory-reader");
        let reader = format!(
            "#!/bin/sh\nexec /bin/cat {}\n",
            crate::services::process::shell_escape(inventory.to_str().unwrap())
        );
        std::fs::write(&executable, reader).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self {
            _inventory: ChannelCommand::new(id, executable),
            _turn: crate::services::tui_o::turn_mode::TestConfirmation::new(id),
            _registry: registry,
            rig,
            name,
        };
        let watcher = fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel);
        assert!(watcher.is_none());
        fixture
    }

    /// The settled reason, then a supervisor tick taken past the notice grace.
    async fn observed(&self) -> Observed3 {
        let (provider, id) = (ProviderKind::Claude, self.rig.channel.get());
        let settled = settled_reason(&self.rig.shared, &provider, id).await;
        let supervisor = LiveSupervisor::new(self.rig.shared.clone(), provider, id);
        let grace = std::time::Duration::from_secs(61);
        supervisor
            .tick_at(tokio::time::Instant::now().checked_sub(grace).unwrap())
            .await;
        let activity = supervisor.state()["activity"].as_str().unwrap().to_owned();
        let held = crate::services::discord::turn_presence::supervisor::unknown_snapshots()
            .into_iter()
            .find(|snapshot| snapshot.channel_id == id)
            .map(|snapshot| (snapshot.reason, snapshot.hold_leaf));
        (settled, activity, held)
    }
}

/// Writes the channel's in-flight row naming `name` as its tmux session.
fn write_row(id: u64, name: &str) {
    let provider = ProviderKind::Claude;
    let row = InflightTurnState::new(
        provider.clone(),
        id,
        None,
        200,
        id + 1,
        id + 2,
        "resumed turn".into(),
        None,
        Some(name.into()),
        None,
        None,
        0,
    );
    let path =
        inflight::inflight_state_path(&inflight::inflight_runtime_root().unwrap(), &provider, id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
}

// A turn of a session relaunched by /resume or a restart reads busy before any watcher claims it,
// so presence holds no Unknown and no notice can start.
#[tokio::test(flavor = "current_thread")]
async fn relaunched_tmux_turn_reads_its_own_session_busy_without_a_watcher_pg() {
    let observed = Relaunched::new(74_240_100, Case::Own)
        .await
        .observed()
        .await;
    assert_eq!(observed, ("open", "busy".to_owned(), None));
}

// Inputs that do not all name one session of this channel, or leave its owner unresolved, keep
// the Unbound identity Hold, so its Unknown and notice stay.
#[tokio::test(flavor = "current_thread")]
async fn relaunched_tmux_turn_keeps_the_identity_hold_when_inputs_disagree_pg() {
    let held = Some((
        "session_identity_unresolved".to_owned(),
        Some("candidate_inflight".to_owned()),
    ));
    let unresolved = ("session_identity_unresolved", "unknown".to_owned(), held);
    let mut wrong = Vec::new();
    for (index, case) in [
        Case::RowConflict,
        Case::ForeignOwner,
        Case::CanonicalElsewhere,
        Case::NoSessionRow,
        Case::RegisteredConflict,
        Case::ForeignClaimUnread,
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Relaunched::new(74_240_110 + index as u64, case).await;
        let observed = fixture.observed().await;
        if observed != unresolved {
            wrong.push((case, observed));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

// An admitted reading whose in-flight row names another session before publication is never
// published on the evidence that admitted it.
#[tokio::test(flavor = "current_thread")]
async fn admitted_reading_is_not_published_after_its_row_moves_to_another_session_pg() {
    let id = 74_240_130;
    let fixture = Relaunched::new(id, Case::Own).await;
    let provider = ProviderKind::Claude;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &provider, id).await,
        "open"
    );
    let pause = Arc::new(Pause {
        at: 2,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(id, pause.clone());
    let _pause = PauseGuard(id);
    let shared = fixture.rig.shared.clone();
    let read = tokio::spawn(async move {
        crate::services::discord::turn_presence::activity::reading_now(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(id),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), pause.entered.notified())
        .await
        .expect("admission never reached its second file collection");
    write_row(
        id,
        &provider.build_tmux_session_name(&format!("moved-{id}")),
    );
    pause.resume.notify_one();
    let reading = read.await.unwrap();
    assert_eq!(reading.observed, observed(Activity::Busy, "open"));
    assert_eq!(reading.session(), Some(fixture.name.as_str()));
    assert!(
        reading.publish_if_current(|| ()).await.is_none(),
        "a reading admitted on a row that moved was published"
    );
}

async fn i7769_owner_only_fixture(id: u64) -> Relaunched {
    let fixture = Relaunched::new(id, Case::Own).await;
    fixture
        .rig
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.name, fixture.rig.channel);
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_none()
    );
    fixture
}

fn advance_same_turn(fixture: &Relaunched, tick: u64, row: bool, transcript: bool) {
    let id = fixture.rig.channel.get();
    if row {
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            id,
        );
        let mut state: InflightTurnState =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        state.save_generation += 1;
        state.last_offset += 100;
        let next = path.with_extension("progress-next");
        std::fs::write(&next, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::rename(next, path).unwrap();
    }
    if transcript {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(fixture.rig.root.path().join("resumed.jsonl"))
            .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type":"assistant", "uuid":format!("progress-{tick}"), "apiBlockIndex":0,
                "message":{"id":format!("message-{tick}"),"content":[{"type":"tool_use", "id":format!("tool-{tick}"),
                    "name":"Read", "input":{}}], "stop_reason":null}
            })
        )
        .unwrap();
    }
}

fn pause_first_collection(id: u64) -> (Arc<Pause>, PauseGuard) {
    let pause = Arc::new(Pause {
        at: 1,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(id, pause.clone());
    (pause, PauseGuard(id))
}

async fn progress_after_collection(
    fixture: &Relaunched,
    pause: &Pause,
    tick: u64,
    row: bool,
    transcript: bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(20), pause.entered.notified())
        .await
        .expect("first file collection was not reached");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    advance_same_turn(fixture, tick, row, transcript);
    pause.resume.notify_one();
}

// Isolate the rejection input while identity, owner, inventory and canonical rows stay fixed.
#[tokio::test(flavor = "current_thread")]
async fn i7769_progress_snapshot_rejection_diagnostic_pg() {
    let fixture = i7769_owner_only_fixture(77_690_001).await;
    let provider = ProviderKind::Claude;
    let ports = || {
        Arc::new(LivePorts {
            root: Some(fixture.rig.root.path().to_owned()),
        })
    };
    for (tick, row, transcript) in [(1, false, true), (2, true, false), (3, true, true)] {
        let (pause, _guard) = pause_first_collection(fixture.rig.channel.get());
        let (pair, ()) = tokio::join!(
            Snapshot::collect_pair(&fixture.rig.shared, &provider, fixture.rig.channel, ports()),
            progress_after_collection(&fixture, &pause, tick, row, transcript),
        );
        let (before, namespace, after) = pair.unwrap();
        assert_eq!(
            before.inflight_session(&fixture.rig.shared, &namespace.inventory),
            Some(fixture.name.clone())
        );
        assert_eq!(
            after.inflight_session(&fixture.rig.shared, &namespace.inventory),
            Some(fixture.name.clone())
        );
        let mut identity_only = after.clone();
        identity_only.files.row_identity = before.files.row_identity.clone();
        assert!(
            before.same_inputs(&identity_only),
            "something besides row_identity changed"
        );
        assert_eq!(before.same_inputs(&after), !row);
        assert!(
            before
                .same_inflight_inputs(&after, "admission_pair")
                .is_ok()
        );
        assert_eq!(before.files.row_identity == after.files.row_identity, !row);
        let candidates = after.files.candidates.as_ref().unwrap();
        assert_eq!(candidates.names.len(), 4);
        assert!(candidates.current.is_some() && candidates.inflight && !candidates.conflict);
        assert!(matches!(after.exact, HostedLookup::Found(_)) && after.exact == after.canonical);
        assert_eq!(
            after.effective_owners().get(&fixture.name),
            Some(&Some(fixture.rig.channel.get()))
        );
        let verdict = after.verdict(&fixture.rig.shared, &namespace.inventory);
        assert!(matches!(verdict, Verdict::Hold("candidate_inflight")));
        eprintln!(
            "I7769_CAUSE tick={tick} row_progress={row} transcript_append={transcript} same_inputs={} row_identity_equal={} all_other_inputs_equal=true inflight_session_before=admitted inflight_session_after=admitted candidates=4 current=true conflict=false inflight=true exact=canonical=found unresolved_owner_count=0 hold=candidate_inflight",
            before.same_inputs(&after),
            before.files.row_identity == after.files.row_identity
        );
    }
}

// A live turn's once-per-second row and transcript progress must not create an identity warning.
#[tokio::test(flavor = "current_thread")]
async fn i7769_same_live_session_progress_stays_busy_without_watcher_pg() {
    let fixture = i7769_owner_only_fixture(77_690_002).await;
    let provider = ProviderKind::Claude;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &provider, fixture.rig.channel.get()).await,
        "open"
    );
    let mut rejected = Vec::new();
    for tick in 1..=3 {
        let (pause, guard) = pause_first_collection(fixture.rig.channel.get());
        let (reading, ()) = tokio::join!(
            crate::services::discord::turn_presence::activity::reading_now(
                &fixture.rig.shared,
                &provider,
                fixture.rig.channel,
            ),
            progress_after_collection(&fixture, &pause, tick, true, true),
        );
        drop(guard);
        let current = reading.publish_if_current(|| ()).await.is_some();
        eprintln!(
            "I7769_PRESENCE tick={tick} observed={:?} publishable={current}",
            reading.observed
        );
        if reading.observed != observed(Activity::Busy, "open") || !current {
            rejected.push((tick, reading.observed, current));
        }
        assert!(
            fixture
                .rig
                .shared
                .tmux_watchers
                .channel_binding(&fixture.rig.channel)
                .is_none()
        );
    }
    assert!(
        rejected.is_empty(),
        "same identity made progress but lost busy presence: {rejected:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn i7769_admitted_progress_publication_diagnostic_pg() {
    let fixture = i7769_owner_only_fixture(77_690_003).await;
    let provider = ProviderKind::Claude;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &provider, fixture.rig.channel.get()).await,
        "open"
    );
    let reading = crate::services::discord::turn_presence::activity::reading_now(
        &fixture.rig.shared,
        &provider,
        fixture.rig.channel,
    )
    .await;
    assert_eq!(reading.observed, observed(Activity::Busy, "open"));
    assert!(reading.publish_if_current(|| ()).await.is_some());
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    advance_same_turn(&fixture, 1, true, false);
    assert!(reading.publish_if_current(|| ()).await.is_some());
    eprintln!(
        "I7769_PUBLICATION admitted_busy=true same_identity_progress=true publication_succeeded=true"
    );
}
fn spawn_nonce_path(fixture: &Relaunched) -> std::path::PathBuf {
    let path = marker_path(&fixture.name, "spawn_nonce");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

fn edit_row(fixture: &Relaunched, edit: impl FnOnce(&mut InflightTurnState)) {
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        fixture.rig.channel.get(),
    );
    let mut row: InflightTurnState =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut row);
    std::fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
}
fn pause_at(id: u64, at: usize) -> (Arc<Pause>, PauseGuard) {
    let guard = PauseGuard(id);
    let pause = Arc::new(Pause {
        at,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(id, pause.clone());
    (pause, guard)
}
fn generation(id: u64) -> u64 {
    let watch = lock(&crate::services::discord::turn_presence::activity::WATCHES)
        .get(&id)
        .unwrap()
        .clone();
    let value = lock(&watch).generation;
    value
}

#[tokio::test(flavor = "current_thread")]
async fn one_recollection_keeps_busy_and_reader_generation_on_epoch_and_session_learning_pg() {
    for (index, epoch) in [true, false].into_iter().enumerate() {
        let id = 77_690_100 + index as u64;
        let fixture = i7769_owner_only_fixture(id).await;
        assert_eq!(
            settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
            "open"
        );
        let before = generation(id);
        let (pause, _guard) = pause_first_collection(id);
        let (reading, ()) = tokio::join!(
            crate::services::discord::turn_presence::activity::reading_now(
                &fixture.rig.shared,
                &ProviderKind::Claude,
                fixture.rig.channel
            ),
            async {
                pause.entered.notified().await;
                if epoch {
                    fixture
                        .rig
                        .shared
                        .tmux_watchers
                        .restore_owner_channel_for_tmux_session(
                            "unrelated-fixture-session",
                            ChannelId::new(id + 100),
                        );
                } else {
                    edit_row(&fixture, |row| row.session_id = Some("resumed".into()));
                }
                pause.resume.notify_one();
            }
        );
        assert_eq!(reading.observed, observed(Activity::Busy, "open"));
        assert_eq!(generation(id), before);
        assert!(reading.publish_if_current(|| ()).await.is_some());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn progress_passes_publication_pair_pinned_and_final_files_checks_pg() {
    for (index, at) in [Some(3), Some(1), None].into_iter().enumerate() {
        let id = 77_690_110 + index as u64;
        let fixture = i7769_owner_only_fixture(id).await;
        assert_eq!(
            settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
            "open"
        );
        let reading = crate::services::discord::turn_presence::activity::reading_now(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel,
        )
        .await;
        let published = if let Some(at) = at {
            let (pause, _guard) = pause_at(id, at);
            let (published, ()) = tokio::join!(reading.publish_if_current(|| 1), async {
                pause.entered.notified().await;
                advance_same_turn(&fixture, 1, true, false);
                pause.resume.notify_one();
            });
            published
        } else {
            advance_same_turn(&fixture, 1, true, false);
            reading.publish_if_current(|| 1).await
        };
        assert_eq!(published, Some(1), "publication stage {index}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn same_second_same_session_new_turn_nonce_rejects_old_publication_pg() {
    let id = 77_690_120;
    let fixture = i7769_owner_only_fixture(id).await;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
        "open"
    );
    let old = crate::services::discord::turn_presence::activity::reading_now(
        &fixture.rig.shared,
        &ProviderKind::Claude,
        fixture.rig.channel,
    )
    .await;
    edit_row(&fixture, |row| row.turn_nonce = Some("successor".into()));
    assert!(matches!(
        old.admission.as_ref().unwrap().current().await,
        Err(inflight_diagnostics::Rejection::InputsChanged {
            phase: "publication_pinned",
            field: "row.turn_nonce"
        })
    ));
    assert!(old.publish_if_current(|| ()).await.is_none());
    let new = crate::services::discord::turn_presence::activity::reading_now(
        &fixture.rig.shared,
        &ProviderKind::Claude,
        fixture.rig.channel,
    )
    .await;
    assert_eq!(new.observed, observed(Activity::Busy, "open"));
    assert!(new.publish_if_current(|| ()).await.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn clear_terminal_missing_read_failure_and_spawn_nonce_invalidate_old_proofs_pg() {
    for boundary in 0..10 {
        let id = 77_690_130 + boundary;
        let fixture = i7769_owner_only_fixture(id).await;
        if boundary >= 8 {
            std::fs::write(spawn_nonce_path(&fixture), "original-incarnation").unwrap();
        }
        assert_eq!(
            settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
            "open"
        );
        let old = crate::services::discord::turn_presence::activity::reading_now(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel,
        )
        .await;
        match boundary {
            0 => {
                let mut core = fixture.rig.shared.core.lock().await;
                let session = core.sessions.get_mut(&fixture.rig.channel).unwrap();
                let name = session.channel_name.clone();
                session.clear_provider_session();
                session.cleared = true;
                assert_eq!(session.channel_name, name);
            }
            1 => edit_row(&fixture, |row| row.terminal_delivery_committed = true),
            2 => {
                let path = inflight::inflight_state_path(
                    &inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Claude,
                    id,
                );
                std::fs::remove_file(path).unwrap();
            }
            3 => {
                let path = inflight::inflight_state_path(
                    &inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Claude,
                    id,
                );
                std::fs::write(path, "broken").unwrap();
            }
            4 => {
                std::fs::write(spawn_nonce_path(&fixture), "new-incarnation").unwrap();
            }
            5 => {
                std::fs::write(spawn_nonce_path(&fixture), " ").unwrap();
            }
            6 => {
                fixture
                    .rig
                    .shared
                    .tmux_watchers
                    .restore_owner_channel_for_tmux_session(
                        &fixture.name,
                        ChannelId::new(id + 100),
                    );
            }
            7 => {
                let path = inflight::inflight_state_path(
                    &inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Claude,
                    id,
                );
                let mut wire: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                wire["runtime_kind"] = serde_json::json!("future_runtime");
                std::fs::write(path, serde_json::to_vec(&wire).unwrap()).unwrap();
            }
            8 => {
                std::fs::remove_file(spawn_nonce_path(&fixture)).unwrap();
            }
            9 => {
                let path = spawn_nonce_path(&fixture);
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            old.publish_if_current(|| ()).await.is_none(),
            "boundary {boundary}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_nameable_uses_projection_and_unnameable_keeps_strict_comparison_pg() {
    let id = 77_690_150;
    let fixture = i7769_owner_only_fixture(id).await;
    for unnameable in [false, true] {
        edit_row(&fixture, |row| {
            row.turn_nonce = None;
            row.user_msg_id = if unnameable { 0 } else { 200 };
            row.turn_start_offset = None;
        });
        let ports = Arc::new(LivePorts {
            root: Some(fixture.rig.root.path().to_owned()),
        });
        let before = Snapshot::collect(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel,
            ports.clone(),
        )
        .await
        .unwrap();
        advance_same_turn(&fixture, 1, true, false);
        let after = Snapshot::collect(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel,
            ports,
        )
        .await
        .unwrap();
        assert_eq!(
            inflight_identity::compare(&before.files, &after.files, "admission_pair").is_ok(),
            !unnameable
        );
        assert!(!before.files.same(&after.files));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_stream_tick_and_heartbeat_stay_busy_through_notice_grace_but_foreign_owner_notices_pg()
 {
    let id = 77_690_160;
    let fixture = i7769_owner_only_fixture(id).await;
    let provider = ProviderKind::Claude;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &provider, id).await,
        "open"
    );
    let probe = crate::services::discord::turn_presence::supervisor::tests::NoticeProbe::new(id);
    let start = tokio::time::Instant::now() - std::time::Duration::from_secs(200);
    for tick in 0..4 {
        let (pause, guard) = pause_first_collection(id);
        let (reading, ()) = tokio::join!(
            crate::services::discord::turn_presence::activity::reading_now(
                &fixture.rig.shared,
                &provider,
                fixture.rig.channel
            ),
            async {
                pause.entered.notified().await;
                let mut state = inflight::load_inflight_state_read_only_result(&provider, id)
                    .unwrap()
                    .unwrap();
                let identity = inflight::InflightTurnIdentity::from_state(&state);
                let mut baseline = state.clone();
                let (message, len) = (state.current_msg_id, state.current_msg_len);
                state.last_offset += 10;
                state.full_response.push_str("stream progress");
                assert_eq!(
                    inflight::save_stream_tick_state_preserving_current_message_races(
                        &mut baseline,
                        &mut state,
                        &identity,
                        message,
                        len,
                        "presence_progress_fixture"
                    ),
                    inflight::GuardedSaveOutcome::Saved
                );
                assert_eq!(
                    inflight::touch_inflight_state_if_matches_identity(
                        &provider,
                        id,
                        &identity,
                        "presence_progress_fixture"
                    ),
                    inflight::GuardedSaveOutcome::Saved
                );
                let durable = inflight::load_inflight_state_read_only_result(&provider, id)
                    .unwrap()
                    .unwrap();
                assert!(durable.save_generation > 0 && durable.last_offset > 0);
                pause.resume.notify_one();
            }
        );
        drop(guard);
        assert_eq!(reading.observed, observed(Activity::Busy, "open"));
        probe
            .tick(reading, start + std::time::Duration::from_secs(tick * 25))
            .await;
    }
    assert_eq!(probe.notice_count(), 0);
    fixture
        .rig
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.name, ChannelId::new(id + 1));
    for offset in [0, 61] {
        advance_same_turn(&fixture, 1, true, false);
        let reading = crate::services::discord::turn_presence::activity::reading_now(
            &fixture.rig.shared,
            &provider,
            fixture.rig.channel,
        )
        .await;
        assert_eq!(reading.observed.activity, Activity::Unknown);
        probe
            .tick(
                reading,
                start + std::time::Duration::from_secs(100 + offset),
            )
            .await;
    }
    assert_eq!(probe.notice_count(), 1);
}
#[tokio::test(flavor = "current_thread")]
async fn continuous_progress_in_both_collection_windows_keeps_busy_pg() {
    let id = 77_690_170;
    let fixture = i7769_owner_only_fixture(id).await;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
        "open"
    );
    let (first, _guard) = pause_first_collection(id);
    let second = Arc::new(Pause {
        at: 2,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    let shared = fixture.rig.shared.clone();
    let mut read = tokio::spawn(async move {
        crate::services::discord::turn_presence::activity::reading_now(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(id),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), first.entered.notified())
        .await
        .unwrap();
    advance_same_turn(&fixture, 1, true, true);
    lock(&PAUSES).insert(id, second.clone());
    first.resume.notify_one();
    let reading = tokio::select! {
        result = &mut read => result.unwrap(),
        () = second.entered.notified() => {
            advance_same_turn(&fixture, 2, true, true);
            second.resume.notify_one();
            read.await.unwrap()
        },
    };
    drop(_guard);
    assert_eq!(reading.observed, observed(Activity::Busy, "open"));
    assert!(reading.publish_if_current(|| ()).await.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn stop_pending_stays_busy_and_native_close_supersedes_the_old_reading_pg() {
    let id = 77_690_180;
    let fixture = i7769_owner_only_fixture(id).await;
    assert_eq!(
        settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
        "open"
    );
    let old = crate::services::discord::turn_presence::activity::reading_now(
        &fixture.rig.shared,
        &ProviderKind::Claude,
        fixture.rig.channel,
    )
    .await;
    assert!(old.publish_if_current(|| ()).await.is_some());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.rig.root.path().join("resumed.jsonl"))
        .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    assert_eq!(
        settled_reason(&fixture.rig.shared, &ProviderKind::Claude, id).await,
        "closed"
    );
    assert!(old.publish_if_current(|| ()).await.is_none());
}
