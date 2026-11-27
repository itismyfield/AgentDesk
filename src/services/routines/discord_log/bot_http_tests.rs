use super::*;

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
