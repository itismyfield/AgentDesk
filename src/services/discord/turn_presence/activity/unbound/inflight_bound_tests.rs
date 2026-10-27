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
