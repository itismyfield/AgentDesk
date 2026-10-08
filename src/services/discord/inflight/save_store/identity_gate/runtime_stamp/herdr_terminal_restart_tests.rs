//! Restart recovery of a held Herdr turn, unadmitted or with its terminal kind committed.
use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on,
};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use serenity::all::ChannelId;

/// A restart keeps a held Herdr turn's row byte for byte and its kind as admitted: `None` never
/// turns into a terminal, though the wrapper or transcript it names holds one; nothing reattaches.
#[tokio::test]
async fn a_restart_keeps_a_held_herdr_turn_and_its_admitted_kind_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let transcripts = tempfile::tempdir().unwrap();
    let (mut cases, mut guards) = (Vec::new(), Vec::new());
    let kinds = [
        (None, false),
        (None, true),
        (Some(NativeTerminalKind::Aborted), false),
        (Some(NativeTerminalKind::Completed), false),
    ];
    for (n, (kind, own_output)) in kinds.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_160_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p10-held-{n}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            Stored::Hosted,
        )
        .await;
        let session = HostSessionRef::tmux(&name);
        guards.push((
            InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(session, HostPresence::Present),
        ));
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, "herdr").unwrap();
        busy_turn(&shared, channel, &name).await;
        // The row names the tmux wrapper seed or the provider transcript, which holds a result.
        let wrapper = transcripts.path().join(format!("{n}.wrapper.jsonl"));
        std::fs::write(&wrapper, "").unwrap();
        let transcript = transcripts.path().join(format!("{n}.jsonl"));
        let prompt =
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "q"}});
        let result = serde_json::json!({"type": "result", "subtype": "success", "result": "a"});
        std::fs::write(&transcript, format!("{prompt}\n{result}\n")).unwrap();
        let mut row =
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .unwrap();
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        let output = if own_output { &transcript } else { &wrapper };
        row.output_path = Some(output.display().to_string());
        row.tui_terminal_kind = kind;
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let path = inflight_state_path(&inflight_runtime_root().unwrap(), &provider, channel.get());
        cases.push((channel, kind, path.clone(), std::fs::read(&path).unwrap()));
    }
    let discord =
        crate::services::discord::recovery_engine::o_cut_recorder::start(cases[0].0.get()).await;

    crate::services::discord::recovery_engine::restore_inflight_turns(
        &discord.http,
        &shared,
        &provider,
    )
    .await;

    for (channel, kind, path, before) in &cases {
        let row = crate::services::discord::inflight::load_inflight_state(&provider, channel.get());
        assert_eq!(
            row.expect("the row stays").tui_terminal_kind,
            *kind,
            "{kind:?}"
        );
        assert_eq!(&std::fs::read(path).unwrap(), before, "{kind:?}");
        let registered = shared.core.lock().await.sessions.contains_key(channel);
        assert!(!registered, "{kind:?}");
    }
    drop(guards);
    pool.close().await;
    db.drop().await;
}
