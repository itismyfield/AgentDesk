use super::super::super::provider_dispatch;
use super::*;
use crate::services::provider::session_prompt;

fn driver(channel: ChannelId, resume: bool) {
    *provider_dispatch::INPUT_EFFECT_OBSERVER.lock().unwrap() = Some((
        channel.get(),
        Box::new(move |_| {
            let mut args = vec![
                "--append-system-prompt".into(),
                "captured original audit text".into(),
            ];
            if resume {
                args.extend(["--resume".into(), STALE.into()]);
            }
            session_prompt::claude_args(&args);
        }),
    ));
}

#[test]
fn system_prompt_intake_saves_without_automatic_posts_and_storage_failure_keeps_turn_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::system_prompt_tests::",
        "system_prompt_intake_saves_without_automatic_posts_and_storage_failure_keeps_turn_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let (fresh, resumed, failing) = (ChannelId::new(7_378_101), ChannelId::new(7_378_102), ChannelId::new(7_378_103));
        let host = host(&[fresh, resumed, failing]).await;
        {
            let mut core = host.shared.core.lock().await;
            for channel in [fresh, failing] { core.sessions.get_mut(&channel).unwrap().session_id = None; }
        }
        driver(fresh, false);
        assert!(run(&host, request(fresh, 7_378_111, "fresh input")).await.is_some());
        let saved = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(saved) = crate::db::provider_session_prompts::fetch(&host.pool, "claude", &fresh.to_string(), "input-effect-provider").await.unwrap() { break saved; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("successful provider and bridge identity must join into one snapshot");
        assert_eq!(saved.system_prompt, "captured original audit text");
        driver(resumed, true);
        assert!(run(&host, request(resumed, 7_378_112, "resume input")).await.is_some());
        assert!(crate::db::provider_session_prompts::fetch(&host.pool, "claude", &resumed.to_string(), "input-effect-provider").await.unwrap().is_none());
        for index in 0..5 {
            let mut seed = saved.clone();
            seed.channel_id = failing.to_string();
            seed.provider_session_id = format!("seed-{index}");
            crate::db::provider_session_prompts::save(&host.pool, &seed).await.unwrap();
        }
        sqlx::query("CREATE SEQUENCE prompt_prune_attempts").execute(&host.pool).await.unwrap();
        sqlx::query("CREATE FUNCTION refuse_prompt_prune() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('prompt_prune_attempts'); RAISE EXCEPTION 'injected prompt prune failure'; END $$").execute(&host.pool).await.unwrap();
        sqlx::query("CREATE TRIGGER refuse_prompt_prune BEFORE DELETE ON provider_session_system_prompts FOR EACH ROW EXECUTE FUNCTION refuse_prompt_prune()").execute(&host.pool).await.unwrap();
        driver(failing, false);
        let _result = provider_dispatch::tests::set_result(failing.get(), "provider turn completed".into());
        let mut input = request(failing, 7_378_113, "storage failure input");
        input.wait_for_completion = true;
        assert!(run(&host, input).await.is_some(), "snapshot persistence cannot fail the intake");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let attempted: bool = sqlx::query_scalar("SELECT is_called FROM prompt_prune_attempts").fetch_one(&host.pool).await.unwrap();
                if attempted { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("actual snapshot prune failure was injected");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_session_system_prompts WHERE channel_id=$1").bind(failing.to_string()).fetch_one(&host.pool).await.unwrap();
        assert_eq!(count, 5, "failed prune rolls back its insert");
        let calls = host.api.take();
        assert!(!calls.iter().any(|call| call.contains("-# 🧾 system prompt") || call.contains("captured original audit text")), "no command means no automatic prompt posts");
        assert!(host.shared.core.lock().await.sessions[&failing].history.iter().any(|item| item.content == "provider turn completed"));
    });
}
