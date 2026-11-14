//! Routine thread creation against a Discord that answers creates as told: a create nobody
//! confirmed is repeated neither by the run nor by the log of its failure.
use super::*;
use crate::services::routines::RoutineDiscordLogger;
use poise::serenity_prelude as serenity;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How the stand-in Discord treats `POST /channels/{id}/threads`.
#[derive(Clone, Copy)]
enum Create {
    /// Takes the request, then drops the connection without an answer.
    Unanswered,
    /// Answers 403.
    Refused,
    /// Answers with `THREAD` as the new thread.
    Made,
    /// Nothing listens: the connection is refused.
    Offline,
}

struct ThreadApi {
    creates: Arc<AtomicUsize>,
    http: Arc<serenity::Http>,
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        let text = String::from_utf8_lossy(&seen).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok());
            if body.len() >= length.unwrap_or(0) {
                return text;
            }
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return text,
            Ok(read) => seen.extend_from_slice(&chunk[..read]),
        }
    }
}

/// A Discord REST stand-in for thread calls, and a client that talks to it.
async fn thread_api(create: Create) -> ThreadApi {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let creates = Arc::new(AtomicUsize::new(0));
    let counted = creates.clone();
    let serve = async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let request = read_request(&mut socket).await;
            let mut line = request.split_whitespace();
            let creating =
                line.next() == Some("POST") && line.next().is_some_and(|p| p.ends_with("/threads"));
            if creating {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            let (status, body) = match (creating, create) {
                (true, Create::Unanswered) => continue,
                (true, Create::Made) => (
                    "201 Created",
                    json!({"id": THREAD.get().to_string(), "type": 11, "name": "nightly",
                        "guild_id": "940487400000009", "parent_id": PRIMARY.get().to_string()}),
                ),
                (true, _) => (
                    "403 Forbidden",
                    json!({"message": "Missing Permissions", "code": 50013}),
                ),
                (false, _) => (
                    "404 Not Found",
                    json!({"message": "Unknown Channel", "code": 10003}),
                ),
            };
            let body = body.to_string();
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    };
    match create {
        Create::Offline => drop(serve),
        _ => drop(tokio::spawn(serve)),
    }
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .build();
    ThreadApi {
        creates,
        http: Arc::new(http),
    }
}

/// `(thread failure, next retry, the routine's saved thread)` of a run.
type ThreadOutcome = (
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
);

async fn thread_outcome(fixture: &Fixture, run: &str) -> ThreadOutcome {
    sqlx::query_as(
        "SELECT rr.result_json ->> 'thread_failure', rr.next_retry_at, r.discord_thread_id
         FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id WHERE rr.id = $1",
    )
    .bind(run)
    .fetch_one(&fixture.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_thread_create_nobody_confirmed_is_repeated_by_neither_the_run_nor_its_log_pg() {
    if !api_child(&format!(
        "{TEST}thread_tests::a_thread_create_nobody_confirmed_is_repeated_by_neither_the_run_nor_its_log_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    let before = channel_state(&fixture, PRIMARY).await;
    let prompt = || "nightly report".to_string();
    let unavailable = || Some("thread_unavailable".to_string());
    // The executor and the outcome logger of a runtime whose thread calls reach `api`.
    let pool = Arc::new(fixture.pool.clone());
    let serve = |api: &ThreadApi| {
        let (http, shared, pool) = (api.http.clone(), fixture.shared.clone(), pool.clone());
        async move {
            let registry = Arc::new(HealthRegistry::new());
            registry.register("claude".to_string(), shared).await;
            registry.register_http("claude".to_string(), http).await;
            let executor = RoutineAgentExecutor::new(pool.clone(), Some(registry.clone()), 60);
            let logger = RoutineDiscordLogger::new_with_health_registry(pool, Some(registry));
            (executor.with_headless_fresh_agents(true), logger)
        }
    };

    // The routine has no thread yet; Discord takes the create and never answers. With or without
    // a retry budget and a fallback agent, the run ends there.
    for policy in [(0, None), (2, Some("fallback8117"))] {
        let api = thread_api(Create::Unanswered).await;
        let (executor, logger) = serve(&api).await;
        let (store, _, claimed, routine_id) =
            routine_claim_with(&fixture, false, None, policy).await;
        let run = claimed.run_id.clone();
        let start =
            executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
        let failed = start.await.unwrap();
        assert_eq!(failed.status, "failed", "{policy:?} {failed:?}");
        // The runtime logs the outcome it got, and later polls find the run closed.
        let logged = logger.log_run_outcome(&store, &failed).await;
        let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
        assert!(polled.is_empty(), "{policy:?} {polled:?}");
        assert_eq!(api.creates.load(Ordering::SeqCst), 1, "{policy:?}");
        let ended = ("failed".to_string(), 0, unavailable());
        assert_eq!(run_state(&fixture, &run).await, ended, "{policy:?}");
        let unconfirmed = (Some("create_unconfirmed".to_string()), None, None);
        assert_eq!(thread_outcome(&fixture, &run).await, unconfirmed);
        // The failure notice goes to the agent's primary channel, not to a new thread.
        assert_eq!(logged.status, "ok", "{policy:?} {logged:?}");
        let notices: Vec<String> = sqlx::query_scalar(
            "SELECT target FROM message_outbox
             WHERE session_key = 'routine:' || $1 || ':run:' || $2 || ':attention'",
        )
        .bind(&routine_id)
        .bind(&run)
        .fetch_all(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(notices, [format!("channel:{}", PRIMARY.get())]);
        assert!(nothing_ran(&fixture, &[PRIMARY, THREAD]));
        assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    }

    // Control: a create Discord refuses also ends the run at once, as a refusal.
    let api = thread_api(Create::Refused).await;
    let (executor, _) = serve(&api).await;
    let spare = (2, Some("fallback8117"));
    let (store, _, claimed, _) = routine_claim_with(&fixture, false, None, spare).await;
    let run = claimed.run_id.clone();
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    assert_eq!(start.await.unwrap().status, "failed");
    let ended = ("failed".to_string(), 0, unavailable());
    assert_eq!(run_state(&fixture, &run).await, ended);
    let refused = (Some("refused".to_string()), None, None);
    assert_eq!(thread_outcome(&fixture, &run).await, refused);
    assert_eq!(api.creates.load(Ordering::SeqCst), 1);

    // Control: a create Discord answers starts the run on that thread and saves it.
    fixture.discord.unname_channel(THREAD.get());
    fixture.discord.add_thread(THREAD.get(), PRIMARY.get());
    let api = thread_api(Create::Made).await;
    let (executor, _) = serve(&api).await;
    let (store, _, claimed, _) = routine_claim(&fixture, false, None, 0).await;
    let run = claimed.run_id.clone();
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    let started = start.await.unwrap();
    assert_eq!(started.status, "running", "{started:?}");
    let saved = Some(THREAD.get().to_string());
    assert_eq!(thread_outcome(&fixture, &run).await, (None, None, saved));
    assert_eq!(api.creates.load(Ordering::SeqCst), 1);
    assert!(turn_active(&fixture, THREAD).await && !turn_active(&fixture, PRIMARY).await);
    std::fs::write(fixture.stubs.join("answer"), "thread-answer-8117").unwrap();
    std::fs::write(fixture.stubs.join("release"), "").unwrap();
    turn_ended(&fixture, THREAD).await;
    assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    sqlx::query("UPDATE routine_runs SET status = 'failed' WHERE id = $1")
        .bind(&run)
        .execute(&fixture.pool)
        .await
        .unwrap();

    // Control: a create that never reached Discord made nothing, so the run waits to try again.
    let api = thread_api(Create::Offline).await;
    let (executor, _) = serve(&api).await;
    let (store, _, claimed, _) = routine_claim(&fixture, false, None, 0).await;
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    let deferred = start.await.unwrap();
    assert_eq!(deferred.status, "deferred", "{deferred:?}");
    assert_eq!(
        deferred.result_json.unwrap()["deferred_reason"],
        "thread_unavailable"
    );
    fixture.db.drop().await;
}
