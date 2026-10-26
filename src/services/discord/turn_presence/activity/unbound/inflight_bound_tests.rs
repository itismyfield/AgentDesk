use super::super::review_tests::Rig;
use super::*;
use crate::services::discord::turn_presence::activity::tests::{log_turn_source, settled_reason};
use crate::services::discord::turn_presence::supervisor::tests::LiveSupervisor;
use crate::services::platform::tmux::session_inventory::tests::ChannelCommand;
use std::io::Write;

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
}

/// What presence settles on, what a supervisor tick publishes, and the held Unknown health shows.
type Observed3 = (&'static str, String, Option<(String, Option<String>)>);

/// A relaunched tmux channel mid-turn with no watcher yet, its four candidates naming one live
/// session, as `case` leaves its row, owner and sessions rows.
async fn relaunched(id: u64, case: Case) -> Observed3 {
    let rig = Rig::new(id, false, false).await;
    let provider = ProviderKind::Claude;
    let name = provider.build_tmux_session_name(&format!("s1e-{id}"));
    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let key = |name: &str| {
        adk_session::build_namespaced_session_key(&rig.shared.token_hash, &provider, name)
    };
    match case {
        Case::CanonicalElsewhere => {
            let earlier = provider.build_tmux_session_name(&format!("before-clear-{id}"));
            inflight::seed_session_row_hashed(
                pool,
                &key(&earlier),
                id,
                &rig.shared.token_hash,
                None,
            )
            .await;
        }
        Case::NoSessionRow => {}
        _ => {
            inflight::seed_session_row_hashed(pool, &key(&name), id, &rig.shared.token_hash, None)
                .await
        }
    }
    let row_name = match case {
        Case::RowConflict => provider.build_tmux_session_name(&format!("other-{id}")),
        _ => name.clone(),
    };
    let row = InflightTurnState::new(
        provider.clone(),
        id,
        None,
        200,
        id + 1,
        id + 2,
        "resumed turn".into(),
        None,
        Some(row_name),
        None,
        None,
        0,
    );
    let path =
        inflight::inflight_state_path(&inflight::inflight_runtime_root().unwrap(), &provider, id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
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
    let _inventory = ChannelCommand::new(id, executable);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_none()
    );

    let settled = settled_reason(&rig.shared, &provider, id).await;
    let supervisor = LiveSupervisor::new(rig.shared.clone(), provider, id);
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

// A turn of a session relaunched by /resume or a restart reads busy before any watcher claims it,
// so presence holds no Unknown and no notice can start.
#[tokio::test(flavor = "current_thread")]
async fn relaunched_tmux_turn_reads_its_own_session_busy_without_a_watcher_pg() {
    let observed = relaunched(74_240_100, Case::Own).await;
    assert_eq!(observed, ("open", "busy".to_owned(), None));
}

// Inputs that do not all name one session of this channel keep the Unbound identity Hold, so its
// Unknown and notice stay.
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
    ]
    .into_iter()
    .enumerate()
    {
        let observed = relaunched(74_240_110 + index as u64, case).await;
        if observed != unresolved {
            wrong.push((case, observed));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}
