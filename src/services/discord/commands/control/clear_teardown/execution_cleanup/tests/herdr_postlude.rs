use super::*;
use crate::services::session_host::herdr_termination_rig as herdr;

#[derive(Clone, Copy, PartialEq)]
enum Selector {
    Absent,
    Same,
    Successor,
    Unproven,
}

fn case(name: &str, channel: u64, interrupted: bool, selector: Selector) {
    if !herdr::in_child(name, channel) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(channel);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    runtime.block_on(async {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let hash = "discord_6772d0000000a0d3";
        let logical = "AgentDesk-claude-e07";
        let key = format!("claude/{hash}/{}:{logical}", herdr::NODE);
        let binding = json!({"host":"herdr","endpoint":herdr::NODE,"owner":logical,
            "root":crate::services::tmux_common::current_tmux_owner_marker(),"execution_nonce":herdr::EXECUTION});
        let fx = Fixture::on_executor(channel, "claude", Some(binding), herdr::NODE).await;
        let mut shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
        Arc::get_mut(&mut shared).unwrap().token_hash = hash.into();
        let _switches = rig.bind(&fx.pool, hash, &key, herdr::HostedState::Bound).await;
        sqlx::query("INSERT INTO session_key_aliases(session_key,session_id) SELECT $1,id FROM sessions WHERE session_key=$2")
            .bind(logical).bind(&key).execute(&fx.pool).await.unwrap();
        let receipt:i64 = sqlx::query_scalar("SELECT routing_receipt_id FROM intake_outbox WHERE provider='claude' AND channel_id=$1 AND owner_generation=0")
            .bind(channel.to_string()).fetch_one(&fx.pool).await.unwrap();
        let mut state = discord::inflight::InflightTurnState::new(crate::services::provider::ProviderKind::Claude,
            channel, None, 100, 90100, 9001, "old frame".into(), None, Some(logical.into()), None, None, 0);
        if matches!(selector, Selector::Same | Selector::Successor) {
            state.session_id=Some("provider-original".into());
        }
        state.replay_receipt_id=Some(receipt);
        state.terminal_delivery_committed=true;
        let root=discord::inflight::inflight_runtime_root().unwrap();
        let path=discord::inflight::inflight_state_path(&root,&shared.provider,channel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes=serde_json::to_vec_pretty(&state).unwrap();std::fs::write(&path,&bytes).unwrap();
        let mut row=cleanup::claim(&fx.pool,&fx.row().await).await.unwrap().unwrap();
        if interrupted {
            let (sent, stopped)=tokio::sync::oneshot::channel();
            discord::herdr_terminate::probe_after_settlement(Box::new(move || Box::pin(async move {
                sent.send(()).unwrap();std::future::pending::<()>().await;
            })));
            {
                let end=run(&shared,&row);tokio::pin!(end);
                tokio::select! {
                    response=&mut end=>panic!("must stop after retire, before cleanup postlude: {response:?}"),
                    _=stopped=>{},
                }
            }
            assert!(path.exists());
            assert_eq!(herdr::row(&fx.pool,&key).await.unwrap().0,herdr::HostedState::Retired);
            assert_ne!(fx.row().await.routing_cleanup_state,"ended");
            fx.wake().await;
            row=cleanup::claim(&fx.pool,&fx.row().await).await.unwrap().unwrap();
            rig.fail_next_process_read();
            assert_eq!(run(&shared,&row).await,Response::Retry,"a failed retired read must remain retryable");
            assert!(cleanup::respond(&fx.pool,&row,Response::Retry).await.unwrap());
            fx.wake().await;
            assert!(cleanup::due(&fx.pool,"claude",herdr::NODE).await.unwrap().iter().any(|next|next.id==row.id));
            row=cleanup::claim(&fx.pool,&fx.row().await).await.unwrap().unwrap();
        }
        if selector != Selector::Absent {
            let shared = shared.clone();
            discord::herdr_terminate::probe_after_settlement(Box::new(move || Box::pin(async move {
                let id = if selector == Selector::Same { "provider-original" } else { "provider-successor" };
                discord::rebind_channel_session(&shared,&shared.provider,ChannelId::new(channel),"/tmp",id).await;
                let mut core=shared.core.lock().await;
                let session=core.sessions.get_mut(&ChannelId::new(channel)).unwrap();
                session.memento_context_loaded=true;session.memento_reflected=true;
            })));
        }
        assert_eq!(run(&shared,&row).await,Response::Ended);
        if selector != Selector::Absent {
            let core=shared.core.lock().await;
            let session=&core.sessions[&ChannelId::new(channel)];
            if selector == Selector::Same {
                assert!(session.session_id.is_none());
                assert!(!session.memento_context_loaded && !session.memento_reflected);
            } else {
                assert_eq!(session.session_id.as_deref(),Some("provider-successor"),"cleanup must preserve a successor or unproven selector");
                assert!(session.memento_context_loaded && session.memento_reflected);
            }
        }
        let writes=rig.writes();
        let pane=rig.record(hash,&key).location.unwrap().pane_id;
        assert!(writes.iter().all(|(method,actual)|method.ends_with(".close") && actual==&pane),"unexpected write: {writes:?}");
        assert_eq!(writes.len(),1);
        assert!(!path.exists(),"retired execution must archive its completed sidecar");
        let archives:Vec<_>=std::fs::read_dir(root.join("archive")).unwrap().map(|e|e.unwrap().path()).collect();
        assert_eq!(archives.len(),1);assert_eq!(std::fs::read(&archives[0]).unwrap(),bytes);
        assert!(cleanup::respond(&fx.pool,&row,Response::Ended).await.unwrap());
        assert!(cleanup::advance(&fx.pool,row.id).await.unwrap());
        use crate::services::tui_o::ownership::execution::{self,SessionRef};
        let fresh=execution::read(&fx.pool,"claude",channel,"mac-b",||Ok(path.exists())).await.unwrap().unwrap();
        assert_eq!(fresh.session_ref,SessionRef::Fresh);
        let delivery=crate::db::intake_outbox::claim_pending_for_target_except(&fx.pool,"mac-b","claude","fresh-proof",&[]).await.unwrap().unwrap();
        assert!(crate::db::intake_outbox::mark_accepted(&fx.pool,delivery.id,"fresh-proof").await.unwrap());
        assert!(crate::db::intake_outbox::mark_spawned(&fx.pool,delivery.id,"fresh-proof").await.unwrap());
        use crate::services::cluster::intake_router_hook::owner_record::input_start;
        let next=json!({"native":"new target fixture","generation":1});
        let permit=input_start::prepare(&fx.pool,input_start::StartRequest{delivery_id:delivery.id,executor:"mac-b",claim_owner:"fresh-proof",incarnation:"new-incarnation",frame:"next",native_binding:&next}).await.unwrap();
        let calls=Arc::new(AtomicUsize::new(0));let observed=calls.clone();
        permit.submit("next",&next,move |_|async move{observed.fetch_add(1,Ordering::SeqCst);}).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst),1);
        fx.pool.close().await;
    });
}

#[test]
fn owner_cleanup_herdr_postlude_archives_before_fresh_pg() {
    case(
        concat!(
            module_path!(),
            "::owner_cleanup_herdr_postlude_archives_before_fresh_pg"
        ),
        73369312,
        false,
        Selector::Absent,
    );
}
#[test]
fn owner_cleanup_herdr_retired_restart_finishes_without_second_close_pg() {
    case(
        concat!(
            module_path!(),
            "::owner_cleanup_herdr_retired_restart_finishes_without_second_close_pg"
        ),
        73369313,
        true,
        Selector::Absent,
    );
}

#[test]
fn owner_cleanup_herdr_matching_selector_cleared_pg() {
    case(
        concat!(
            module_path!(),
            "::owner_cleanup_herdr_matching_selector_cleared_pg"
        ),
        73369314,
        false,
        Selector::Same,
    );
}

#[test]
fn owner_cleanup_herdr_successor_selector_preserved_pg() {
    case(
        concat!(
            module_path!(),
            "::owner_cleanup_herdr_successor_selector_preserved_pg"
        ),
        73369315,
        false,
        Selector::Successor,
    );
}

#[test]
fn owner_cleanup_herdr_unproven_selector_preserved_pg() {
    case(
        concat!(
            module_path!(),
            "::owner_cleanup_herdr_unproven_selector_preserved_pg"
        ),
        73369316,
        false,
        Selector::Unproven,
    );
}
