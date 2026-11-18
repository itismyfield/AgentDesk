use super::*;
use crate::services::discord::commands::routine_model::{Input, prepare};
use crate::services::routines::provider_choice::ProviderChoice;
use crate::services::routines::{NewRoutine, RoutinePatch, RoutineStore};

fn dispatch_refusal(fx: &Fixture, model: &str) -> String {
    let runtime = tokio::runtime::Handle::current();
    let (shared, channel, name, cwd, model) = (
        fx.shared.clone(),
        fx.channel,
        fx.name.clone(),
        fx.cwd.path().to_str().unwrap().to_string(),
        model.to_string(),
    );
    let mut context = fx.context(channel);
    context.session_key = Some(channel_key(&shared, &name));
    let thread = std::thread::spawn(move || {
        let _entered = runtime.enter();
        crate::services::platform::with_provider_execution_context(context, || {
            crate::services::discord::router::message_handler::routine_model_launch_for_tests(
                Input {
                    shared: &shared,
                    provider: &ProviderKind::Claude,
                    channel,
                    tmux: &name,
                    cwd: &cwd,
                    session: Some(SESSION),
                    model: Some(&model),
                },
                Arc::new(CancelToken::new()),
                &cleared(&name),
            )
            .unwrap_err()
        })
    });
    crate::test_support::wait::join_within(thread, crate::test_support::wait::WAIT_CAP)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persistent_model_patch_resumes_the_same_native_session_before_input_pg() {
    let test = concat!(
        module_path!(),
        "::a_persistent_model_patch_resumes_the_same_native_session_before_input_pg"
    );
    if !api_child(test.strip_prefix("agentdesk::").unwrap_or(test)) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(821901, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let provider = ProviderKind::Claude;
    let prep = |model| Input {
        shared: &fx.shared,
        provider: &provider,
        channel: fx.channel,
        tmux: &fx.name,
        cwd: fx.cwd.path().to_str().unwrap(),
        session: Some(SESSION),
        model,
    };
    fx.tmux.mark_missing();
    let a = prepare(prep(Some("sonnet"))).await.unwrap();
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        SESSION,
        Some("sonnet"),
        None,
        Some(a.requirement),
    );
    assert!(argv.windows(2).any(|p| p == ["--model", "sonnet"]));
    let observed: String = sqlx::query_scalar("SELECT model FROM sessions WHERE session_key=$1")
        .bind(channel_key(&fx.shared, &fx.name))
        .fetch_one(fx.shared.pg_pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(observed, "sonnet");

    fx.tmux.mark_present();
    fx.tmux.take_calls();
    let pool = fx.shared.pg_pool.as_ref().unwrap();
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('routine-model','Routine model','claude',$1)")
        .bind(fx.channel.get().to_string()).execute(pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let routine = store
        .attach_routine_with_choice(
            NewRoutine {
                agent_id: Some("routine-model".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "provider.js".into(),
                name: "model".into(),
                status: None,
                execution_strategy: "persistent".into(),
                schedule: None,
                next_due_at: None,
                checkpoint: None,
                discord_thread_id: None,
                timeout_secs: None,
            },
            None,
            3,
            &ProviderChoice {
                provider: Some("claude".into()),
                model: Some("sonnet".into()),
            },
        )
        .await
        .unwrap();
    let old = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    store
        .patch_routine(
            &routine.id,
            RoutinePatch {
                provider_choice: Some(ProviderChoice {
                    provider: Some("claude".into()),
                    model: Some("opus".into()),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pinned = store.run_provider_choice(&old.run_id).await.unwrap();
    let old_prep = prepare(prep(pinned.model.as_deref())).await.unwrap();
    assert!(
        old_prep.reused,
        "the claim before PATCH keeps its model and pane"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    sqlx::query("UPDATE routine_runs SET status='succeeded' WHERE id=$1")
        .bind(&old.run_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE routines SET in_flight_run_id=NULL WHERE id=$1")
        .bind(&routine.id)
        .execute(pool)
        .await
        .unwrap();
    let next = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let choice = store.run_provider_choice(&next.run_id).await.unwrap();
    let b = prepare(prep(choice.model.as_deref())).await.unwrap();
    let before: String = sqlx::query_scalar("SELECT model FROM sessions WHERE session_key=$1")
        .bind(channel_key(&fx.shared, &fx.name))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        before, "sonnet",
        "preparing B cannot publish it before launch"
    );

    assert!(
        called(&fx.tmux.take_calls(), "kill-session"),
        "model B must replace the old native execution before input"
    );
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        SESSION,
        choice.model.as_deref(),
        None,
        Some(b.requirement),
    );
    assert!(argv.windows(2).any(|p| p == ["--resume", SESSION]));
    assert!(argv.windows(2).any(|p| p == ["--model", "opus"]));
    let applied: String = sqlx::query_scalar("SELECT model FROM sessions WHERE session_key=$1")
        .bind(channel_key(&fx.shared, &fx.name))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(applied, "opus");

    fx.tmux.mark_present();
    fx.tmux.take_calls();
    let _busy = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(true);
    let blocked = dispatch_refusal(&fx, "blocked-model");
    assert!(blocked.contains("NotSent"), "{blocked}");
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    drop(_busy);
    sqlx::query("UPDATE routine_runs SET status='succeeded' WHERE id=$1")
        .bind(&next.run_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE routines SET in_flight_run_id=NULL WHERE id=$1")
        .bind(&routine.id)
        .execute(pool)
        .await
        .unwrap();
    store
        .patch_routine(
            &routine.id,
            RoutinePatch {
                provider_choice: Some(ProviderChoice {
                    provider: Some("claude".into()),
                    model: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let null_run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    assert!(
        store
            .run_provider_choice(&null_run.run_id)
            .await
            .unwrap()
            .model
            .is_none()
    );
    // The parent override exists before the routine; the routine must never replace it.
    fx.shared
        .overrides
        .model_overrides
        .insert(fx.channel, "channel-model".into());
    let parent_model = crate::services::discord::commands::resolve_model_for_turn(
        &fx.shared, fx.channel, &provider,
    )
    .await;
    let inherited = prepare(prep(parent_model.as_deref())).await.unwrap();
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        SESSION,
        Some("channel-model"),
        None,
        Some(inherited.requirement),
    );
    assert!(argv.windows(2).any(|p| p == ["--resume", SESSION]));
    assert!(argv.windows(2).any(|p| p == ["--model", "channel-model"]));
    assert_eq!(
        fx.shared
            .overrides
            .model_overrides
            .get(&fx.channel)
            .unwrap()
            .as_str(),
        "channel-model"
    );
    assert_eq!(fx.shared.overrides.model_overrides.len(), 1);
    let applied: String = sqlx::query_scalar("SELECT model FROM sessions WHERE session_key=$1")
        .bind(channel_key(&fx.shared, &fx.name))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(applied, "channel-model");

    fx.tmux.mark_present();
    fx.tmux.take_calls();
    fx.tmux.fail_kills();
    let failed = dispatch_refusal(&fx, "another-model");
    assert!(failed.contains("NotSent"), "{failed}");
    let calls = fx.tmux.take_calls();
    assert!(called(&calls, "kill-session"));
    assert!(
        !called(&calls, "load-buffer")
            && !called(&calls, "paste-buffer")
            && !called(&calls, "send-keys")
    );
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
    Case::Stored(Stored::Future)
        .seed(
            pool,
            &channel_key(&fx.shared, &fx.name),
            &fx.name,
            fx.channel.get(),
        )
        .await;
    let refused = dispatch_refusal(&fx, "host-refused");
    assert!(refused.contains("NotSent"), "{refused}");
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    Case::Stored(Stored::Legacy)
        .seed(
            pool,
            &channel_key(&fx.shared, &fx.name),
            &fx.name,
            fx.channel.get(),
        )
        .await;
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        SESSION,
        Some(&fx._account.2),
    )
    .unwrap();
    let bytes = std::fs::read(&transcript).unwrap();
    std::fs::remove_file(&transcript).unwrap();
    let unavailable = dispatch_refusal(&fx, "resume-unavailable");
    assert!(
        unavailable.contains("same-session resume unavailable"),
        "{unavailable}"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    std::fs::write(&transcript, bytes).unwrap();
    let proof_path = crate::services::tui_input::routine_model::path(&fx.name);
    let _ = std::fs::remove_file(&proof_path);
    let nonce_path = tc::session_temp_path(&fx.name, "spawn_nonce");
    std::fs::remove_file(nonce_path).unwrap();
    let missing = dispatch_refusal(&fx, "unproved");
    assert!(missing.contains("selector unproved"), "{missing}");
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    tc::cleanup_session_temp_files(&fx.name);
}
