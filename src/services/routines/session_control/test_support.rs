use chrono::Utc;

use super::RoutineRecord;

pub fn routine_with_thread(
    execution_strategy: &str,
    discord_thread_id: Option<&str>,
) -> RoutineRecord {
    RoutineRecord {
        id: "routine-1".to_string(),
        agent_id: Some("agent-1".to_string()),
        fallback_agent_id: None,
        max_retries: 0,
        script_ref: "script".to_string(),
        name: "Routine".to_string(),
        status: "enabled".to_string(),
        execution_strategy: execution_strategy.to_string(),
        schedule: None,
        next_due_at: None,
        last_run_at: None,
        last_result: None,
        checkpoint: None,
        discord_thread_id: discord_thread_id.map(ToOwned::to_owned),
        timeout_secs: None,
        in_flight_run_id: None,
        pause_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{
        RoutineBotHttpPort, resolve_available_log_bot, resolve_routine_thread_http,
        routine_log_bot_candidates,
    };

    #[tokio::test]
    async fn bot_http_port_preserves_thread_and_log_fallbacks() {
        struct NotifyOnly(std::sync::Mutex<Vec<String>>);

        impl RoutineBotHttpPort for NotifyOnly {
            fn resolve<'a>(
                &'a self,
                bot: &'a str,
            ) -> futures::future::BoxFuture<
                'a,
                crate::services::routines::discord_port::RoutineBotHttpResult,
            > {
                Box::pin(async move {
                    self.0.lock().unwrap().push(bot.to_string());
                    if bot == "notify" {
                        Ok(Arc::new(poise::serenity_prelude::Http::new("test-token")))
                    } else {
                        Err(("400 Bad Request", "unknown bot".to_string()))
                    }
                })
            }
        }

        let bot_http = NotifyOnly(Default::default());
        let candidates = routine_log_bot_candidates(Some("codex"), Some("maker"));
        assert_eq!(
            resolve_available_log_bot(None, &candidates).await.unwrap(),
            "codex"
        );
        assert_eq!(
            resolve_available_log_bot(None, &[]).await.unwrap_err(),
            "no routine discord log bot candidates"
        );
        assert!(bot_http.0.lock().unwrap().is_empty());

        assert_eq!(
            resolve_available_log_bot(Some(&bot_http), &candidates)
                .await
                .unwrap(),
            "notify"
        );
        assert_eq!(*bot_http.0.lock().unwrap(), ["codex", "maker", "notify"]);

        bot_http.0.lock().unwrap().clear();
        let thread_http = resolve_routine_thread_http(&bot_http, "codex", "codex")
            .await
            .unwrap();
        assert_eq!(thread_http.bot, "notify");
        assert_eq!(*bot_http.0.lock().unwrap(), ["codex", "notify"]);

        let error = resolve_available_log_bot(Some(&bot_http), &candidates[..2])
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "no routine discord log bot available (codex: 400 Bad Request unknown bot; maker: 400 Bad Request unknown bot)"
        );
    }
}
