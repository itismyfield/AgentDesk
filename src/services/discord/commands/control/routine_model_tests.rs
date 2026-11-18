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
    assert_eq!(parent_model.as_deref(), Some("channel-model"));
    let claim = serde_json::json!({
        "routine_id": routine.id,
        "execution_strategy": "persistent",
        "model": null,
    });
    assert!(
        crate::services::discord::router::message_handler::routine_model_boundary_for_tests(
            Some(&claim),
            &provider,
            fx.channel.get(),
            Some(&fx.name),
            Some(SESSION),
        ),
        "null claim must check the live execution that applied the pinned model"
    );
    let inherited = prepare(prep(parent_model.as_deref())).await.unwrap();
    assert!(!inherited.reused);
    assert!(called(&fx.tmux.take_calls(), "kill-session"));
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        SESSION,
        parent_model.as_deref(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routine_fix4_claude_default_replaces_the_live_pinned_model_pg() {
    default_restore(
        true,
        concat!(
            module_path!(),
            "::routine_fix4_claude_default_replaces_the_live_pinned_model_pg"
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routine_fix4_claude_default_replaces_the_saved_pinned_model_pg() {
    default_restore(
        false,
        concat!(
            module_path!(),
            "::routine_fix4_claude_default_replaces_the_saved_pinned_model_pg"
        ),
    )
    .await;
}

async fn default_restore(live: bool, test: &str) {
    let child = {
        // The child needs the normal PATH before either tmux fixture is installed.
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        api_child(test.strip_prefix("agentdesk::").unwrap_or(test))
    };
    if !child {
        return;
    }
    use crate::services::discord::router::message_handler::routine_model_boundary_for_tests;
    let _root = crate::config::TestRuntimeRootGuard::new();
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture as Transport;
    let native_tmux = Transport::tmux_program();
    let fx = Fixture::new(821941 + u64::from(live), RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let pool = fx.shared.pg_pool.as_ref().unwrap();
    let key = channel_key(&fx.shared, &fx.name);
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('default-model','Default model','claude',$1)")
        .bind(fx.channel.get().to_string())
        .execute(pool)
        .await
        .unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let routine = store
        .attach_routine_with_choice(
            NewRoutine {
                agent_id: Some("default-model".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "provider.js".into(),
                name: "default model".into(),
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
                model: Some("opus".into()),
            },
        )
        .await
        .unwrap();
    let prep = |session, model| Input {
        shared: &fx.shared,
        provider: &ProviderKind::Claude,
        channel: fx.channel,
        tmux: &fx.name,
        cwd: fx.cwd.path().to_str().unwrap(),
        session,
        model,
    };
    let old = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let old_choice = store.run_provider_choice(&old.run_id).await.unwrap();
    fx.tmux.mark_missing();
    let a = prepare(prep(Some(SESSION), old_choice.model.as_deref()))
        .await
        .unwrap();
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        SESSION,
        old_choice.model.as_deref(),
        None,
        Some(a.requirement),
    );
    assert!(argv.windows(2).any(|p| p == ["--model", "opus"]));
    let applied = || {
        sqlx::query_scalar::<_, Option<String>>("SELECT model FROM sessions WHERE session_key=$1")
            .bind(&key)
            .fetch_one(pool)
    };
    assert_eq!(applied().await.unwrap().as_deref(), Some("opus"));
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

    let session = if live {
        fx.tmux.mark_present();
        fx.session_id().await.unwrap()
    } else {
        fx.tmux.mark_missing();
        std::fs::remove_file(tc::session_temp_path(&fx.name, "spawn_nonce")).unwrap();
        // Restore the durable native selector with no execution nonce available.
        sqlx::query("UPDATE sessions SET claude_session_id=$1 WHERE session_key=$2")
            .bind(SESSION)
            .bind(&key)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query_scalar::<_, String>(
            "SELECT claude_session_id FROM sessions WHERE session_key=$1",
        )
        .bind(&key)
        .fetch_one(pool)
        .await
        .unwrap()
    };
    use crate::services::tui_input::routine_model as proof;
    let proof_path = proof::path(&fx.name);
    let old_proof = std::fs::read(&proof_path).unwrap();
    let aged = std::time::SystemTime::now() - Duration::from_secs(3600);
    let owner = PathBuf::from(tc::session_temp_path(&fx.name, "owner"));
    let control = PathBuf::from(tc::session_temp_path(
        "routine-model-orphan-control",
        "owner",
    ));
    std::fs::write(&control, b"orphan sweep control").unwrap();
    for path in [&proof_path, &owner, &control] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(aged)
            .unwrap();
    }
    crate::services::discord::tmux::sweep_orphan_session_files_for_tests().await;
    assert!(
        !control.exists(),
        "production sweep must remove an aged orphan"
    );
    assert_eq!(
        owner.exists(),
        live,
        "ordinary files track the pane lifetime"
    );
    assert_eq!(
        std::fs::read(&proof_path).ok().as_deref(),
        Some(old_proof.as_slice()),
        "production sweep discarded the aged routine model proof (live={live})"
    );
    assert!(proof::tracks_native_session("claude", &fx.name, &session));

    fx.tmux.take_calls();
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
    let next = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let choice = store.run_provider_choice(&next.run_id).await.unwrap();
    assert!(choice.model.is_none());
    let model = crate::services::discord::commands::resolve_model_for_turn(
        &fx.shared,
        fx.channel,
        &ProviderKind::Claude,
    )
    .await;
    assert!(
        model.is_none(),
        "the parent leaves Claude's model to the account"
    );
    let claim = serde_json::json!({"routine_id":routine.id, "execution_strategy":"persistent", "model":choice.model});
    assert!(
        routine_model_boundary_for_tests(
            Some(&claim),
            &ProviderKind::Claude,
            fx.channel.get(),
            Some(&fx.name),
            Some(&session),
        ),
        "the saved Claude conversation still needs model reset"
    );

    let restored = prepare(prep(Some(&session), model.as_deref()))
        .await
        .unwrap();
    assert!(!restored.reused);
    assert_eq!(called(&fx.tmux.take_calls(), "kill-session"), live);
    assert_eq!(applied().await.unwrap().as_deref(), Some("opus"));
    let (argv, _, _) = fx.launch_required(
        fx.channel,
        &fx.name,
        &session,
        model.as_deref(),
        None,
        Some(restored.requirement),
    );
    assert!(argv.windows(2).any(|p| p == ["--resume", SESSION]));
    assert!(
        argv.windows(2).any(|p| p == ["--model", "default"]),
        "Claude would restore the pinned model without an explicit account-default selector: {argv:?}"
    );
    assert_eq!(applied().await.unwrap(), None);
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
    fx.tmux.mark_present();
    fx.tmux.take_calls();
    assert!(
        prepare(prep(Some(&session), model.as_deref()))
            .await
            .unwrap()
            .reused
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    // Replay the actual launch argv through the owner on an isolated renderer.
    tokio::task::block_in_place(|| {
        crate::services::discord::router::message_handler::assert_routine_owner_delivery(
            native_tmux,
            &session,
            &argv,
            "default",
        )
    });
}
