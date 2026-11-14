use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::channel_policy::first_prompt_digest;
use crate::services::tui_o::cutover::BodySend;
use crate::services::tui_prompt_dedupe::{
    self as dedupe, TuiRuntimeBinding, binding_context::PreparedIncarnation,
};
use crate::test_support::wait::{WAIT_CAP, await_within};

async fn scenario(
    boundary: Option<Step>,
    invalidator: &str,
    output: Option<&str>,
    pool: Option<&sqlx::PgPool>,
) {
    dedupe::reset_state_for_tests();
    tokio::time::pause();
    let Takeover {
        harness,
        io,
        tasks,
        _reserved: reserved,
    } = hosted_over_old(8_165_201).await;
    let channel = harness.channel_id;
    let blocker = if let Some(pool) = pool {
        tokio::time::resume();
        *io.intake_pool.lock().unwrap() = Some(pool.clone());
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE intake_outbox IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        Some((tx, pid))
    } else {
        None
    };
    let candidate =
        test_override::with_channels(|boot| boot.unwrap().candidate(channel).unwrap().clone());
    let token = Arc::new(CancelToken::new());
    let admission = candidate
        .admit_first(
            channel,
            token.turn_nonce().unwrap(),
            "hi",
            "tmux-codex",
            || {
                ChannelBindingLog::new(channel, ShadowProvider::Codex)
                    .binding_events_since(channel, 0)
                    .map(|events| events.last().map_or(0, |e| e.seq))
            },
        )
        .unwrap();
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        "tmux-codex",
        Some(channel),
        None,
        false,
        Some(harness._runtime.path().canonicalize().unwrap()),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    candidate.attach_first_execution(&prepared.context, "hi", token.turn_nonce());
    let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
        "tmux-codex",
        "spawn_nonce",
    ));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, &prepared.context.execution_nonce).unwrap();
    crate::services::tmux_common::write_tmux_runtime_kind_marker("tmux-codex", CodexTui).unwrap();
    let mut bytes = prompted("s-codex");
    if invalidator == "wrong-prompt" {
        bytes = String::from_utf8(bytes)
            .unwrap()
            .replace("hi", "different")
            .into_bytes();
    }
    let (path, source) = rollout(&harness, "proved-first.jsonl", &bytes);
    let output = output.map(str::to_owned);
    let first = answer("first-output", "short answer");
    let ended = codex_line(
        serde_json::json!({"type":"event_msg", "timestamp":Utc::now().to_rfc3339(),
        "payload":{"type":"task_complete", "turn_id":"t1"}}),
    );
    if output.as_deref().is_some_and(|point| {
        matches!(
            point,
            "before-bind" | "closed" | "growth" | "unsafe" | "rewrite"
        )
    }) {
        append(&path, &first);
        if output.as_deref() == Some("closed") {
            append(&path, &ended);
        }
    }
    if output.as_deref() == Some("announced") {
        append(
            &path,
            &codex_line(serde_json::json!({"type":"event_msg", "payload":{
            "type":"item_completed", "turn_id":"t1", "item":{"type":"AgentMessage", "id":"first-output"}}})),
        );
    }
    let mutate = {
        let candidate = candidate.clone();
        let token = token.clone();
        let marker = marker.clone();
        let path = path.clone();
        let output = output.clone();
        let invalidator = invalidator.to_owned();
        move || {
            match invalidator.as_str() {
                "send" => {
                    drop(candidate.claim_body(channel));
                }
                "cancel" | "stop" => token.publish_cancel("stop at takeover boundary"),
                "clear" => cutover::invalidate_codex_first(Some(channel), None, None),
                "terminate" => candidate.invalidate_first(None, Some("tmux-codex")),
                "nonce" => std::fs::write(marker, "successor").unwrap(),
                "evict" => {
                    assert!(dedupe::evict_dead_tmux_mirror("tmux-codex"));
                }
                "valid" | "wrong-prompt" | "output" | "unsafe-output" => {}

                _ => unreachable!(),
            }
            match output.as_deref() {
                Some("after-judge" | "fence" | "final") => {
                    append(&path, &answer("first-output", "short answer"))
                }
                Some("growth") => append(&path, &answer("second-output", "later answer")),
                Some("unsafe") => {
                    let mut old: serde_json::Value =
                        serde_json::from_slice(&answer("foreign", "old answer")).unwrap();
                    old["payload"]["turn_id"] = "old-turn".into();
                    append(&path, &codex_line(old));
                }
                Some("rewrite") => {
                    let bytes = std::fs::read_to_string(&path)
                        .unwrap()
                        .replace("short answer", "rewritten answer");
                    std::fs::write(path, bytes).unwrap();
                }
                _ => {}
            }
            Ok(())
        }
    };
    if invalidator == "output" {
        let mut unkeyed: serde_json::Value = serde_json::from_slice(&first).unwrap();
        unkeyed["payload"].as_object_mut().unwrap().remove("id");
        append(&path, &codex_line(unkeyed));
    }
    let shared = crate::services::discord::make_shared_data_for_tests();
    *io.relay_shared.lock().unwrap() = Some(shared.clone());
    let transports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut body = Box::pin(crate::services::discord::first_terminal_plan(
        shared.clone(),
        channel,
        "tmux-codex",
        token.turn_nonce(),
        &path,
        transports.clone(),
    ));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(body.as_mut(), &mut context).is_pending(),
        "the body is retained before any Legacy claim"
    );
    assert_eq!(transports.load(Ordering::SeqCst), 0);
    assert!(!shared.relay_emission_in_flight(poise::serenity_prelude::ChannelId::new(channel)));
    if let Some(boundary) = boundary {
        test_hook::set(channel, boundary, mutate);
    }
    dedupe::register_tmux_channel("tmux-codex", channel);
    if invalidator == "missing-bind-nonce" {
        std::fs::remove_file(&marker).unwrap();
    } else if invalidator == "old-bind-nonce" {
        std::fs::write(&marker, "previous-execution").unwrap();
    }
    dedupe::register_tmux_runtime_binding(
        "tmux-codex",
        TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("s-codex".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    if matches!(invalidator, "missing-bind-nonce" | "old-bind-nonce") {
        std::fs::write(&marker, &prepared.context.execution_nonce).unwrap();
    }
    if invalidator == "evict" && boundary.is_none() {
        dedupe::evict_dead_tmux_mirror("tmux-codex");
    }
    if matches!(output.as_deref(), Some("after-bind" | "short")) {
        if output.as_deref() == Some("short") {
            let events = ChannelBindingLog::new(channel, ShadowProvider::Codex)
                .binding_events_since(channel, 0)
                .unwrap();
            assert_eq!(
                crate::services::tui_o::writer::adoption::current(&events)
                    .map(|(source, _)| source),
                Some(source.clone()),
                "the binding is durable before the first output"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        append(&path, &first);
        if output.as_deref() == Some("short") {
            append(
                &path,
                &codex_line(
                    serde_json::json!({"type":"event_msg", "timestamp":Utc::now().to_rfc3339(),
                "payload":{"type":"task_complete", "turn_id":"t1"}}),
                ),
            );
        }
    }
    let sent = if let Some((tx, pid)) = blocker {
        let publish = async {
            let pool = pool.unwrap();
            await_within(async {
                loop {
                    let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))")
                        .bind(pid).fetch_one(pool).await.unwrap();
                    if waiting { break; }
                }
            }, WAIT_CAP).await;
            append(&path, &first);
            tx.rollback().await.unwrap();
        };
        tokio::join!(await_within(body, WAIT_CAP), publish).0
    } else {
        await_within(body, WAIT_CAP).await
    };
    if invalidator == "valid" {
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        assert_eq!(init_sources(&harness, channel), [(source.clone(), 0)]);
        assert!(matches!(sent, BodySend::OwnedByO));
        assert_eq!(transports.load(Ordering::SeqCst), 0);
        if pool.is_none() {
            tokio::time::resume();
        }
        if output.as_deref() == Some("announced") {
            assert!(
                harness.port.posts().is_empty(),
                "announced is not sealed or posted"
            );
        }
        if output.is_none() || output.as_deref() == Some("announced") {
            append(&path, &first);
        }
        await_within(
            async {
                let count = if matches!(output.as_deref(), Some("closed" | "short" | "growth")) {
                    2
                } else {
                    1
                };
                while harness.port.posts().len() < count {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        let posts = harness.port.posts();
        assert_eq!(
            posts
                .iter()
                .filter(|body| body.as_str() == "short answer")
                .count(),
            1
        );
        if output.as_deref() == Some("growth") {
            assert_eq!(posts, ["short answer", "later answer"]);
        } else if matches!(output.as_deref(), Some("closed" | "short")) {
            assert_eq!(posts.len(), 2, "one body and one existing turn summary");
            assert_ne!(posts[0], posts[1]);
            let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let stamp = |kind: &str| {
                let record = records
                    .iter()
                    .find(|v| v["payload"]["type"] == kind)
                    .unwrap();
                chrono::DateTime::parse_from_rfc3339(record["timestamp"].as_str().unwrap()).unwrap()
            };
            let native_elapsed =
                stamp("task_complete").signed_duration_since(stamp("task_started"));
            assert!(
                (0..2000).contains(&native_elapsed.num_milliseconds()),
                "fixture native turn must complete under two seconds"
            );
        } else {
            assert_eq!(posts, ["short answer"]);
        }
    } else {
        assert_ne!(adoption(channel), Adoption::Committed, "{invalidator}");
        assert!(
            harness.store.read_init(channel).unwrap().is_none(),
            "{invalidator}"
        );
        assert!(
            matches!(sent, BodySend::Sent(1)),
            "{invalidator}: Legacy must resume exactly once"
        );
    }
    drop(admission);
    if invalidator == "valid" {
        tokio::time::pause();
        restart_posts_nothing(&harness, tasks, &source).await;
    } else {
        assert_eq!(transports.load(Ordering::SeqCst), 1);
        drop(tasks);
    }
    drop(reserved);
    dedupe::reset_state_for_tests();
}

#[test]
fn proved_first_prefix_commits_and_releases_legacy_without_a_body_send() {
    serial_scenario(None, "valid");
}

#[test]
fn output_free_policy_refuses_output_and_wrong_admitted_prompt_without_losing_legacy() {
    for invalid in ["output", "wrong-prompt"] {
        serial_scenario(None, invalid);
    }
}

#[test]
fn invalidation_at_each_fence_boundary_prevents_a_stale_first_init() {
    for step in [Step::BeforeFence, Step::AfterFence, Step::BeforeLock] {
        for invalid in [
            "send",
            "cancel",
            "stop",
            "clear",
            "terminate",
            "nonce",
            "evict",
        ] {
            serial_scenario(Some(step), invalid);
        }
    }
}

#[test]
fn a_binding_must_name_the_admitted_execution_not_a_legacy_or_old_one() {
    for invalid in ["missing-bind-nonce", "old-bind-nonce"] {
        serial_scenario(None, invalid);
    }
}

fn serial_scenario(boundary: Option<Step>, invalidator: &str) {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    dedupe::run_serial_test(async {
        scenario(boundary, invalidator, None, None).await;
    });
    drop(root_guard);
}

#[test]
fn buffered_first_output_at_every_adoption_boundary_is_replayed_once() {
    for (point, boundary) in [
        ("before-bind", None),
        ("after-bind", None),
        ("after-judge", Some(Step::BeforeFence)),
        ("fence", Some(Step::AfterFence)),
        ("final", Some(Step::BeforeLock)),
        ("closed", None),
        ("announced", None),
        ("growth", Some(Step::BeforeLock)),
    ] {
        let root = crate::config::TestRuntimeRootGuard::new();
        dedupe::run_serial_test(async {
            scenario(boundary, "valid", Some(point), None).await;
        });
        drop(root);
    }
}

#[test]
fn a_checked_buffered_prefix_rejects_foreign_growth_and_rewritten_known_bytes() {
    for point in ["unsafe", "rewrite"] {
        let root = crate::config::TestRuntimeRootGuard::new();
        dedupe::run_serial_test(async {
            scenario(Some(Step::BeforeLock), "unsafe-output", Some(point), None).await;
        });
        drop(root);
    }
}

#[test]
fn a_short_native_turn_binds_before_output_commits_and_posts_once() {
    let root = crate::config::TestRuntimeRootGuard::new();
    dedupe::run_serial_test(async {
        scenario(None, "valid", Some("short"), None).await;
    });
    drop(root);
}

#[test]
fn buffered_output_arriving_while_the_real_intake_fence_waits_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    dedupe::run_serial_test(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "first_buffered_fence",
            "buffered first turn while intake fence waits",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        scenario(None, "valid", Some("fence-wait"), Some(&pool)).await;
        tokio::time::resume();
        pool.close().await;
        db.drop().await;
    });
    drop(root);
}

#[test]
fn buffered_first_proof_is_invalidated_at_every_lifecycle_boundary() {
    for step in [Step::BeforeFence, Step::AfterFence, Step::BeforeLock] {
        for invalid in [
            "send",
            "cancel",
            "stop",
            "clear",
            "terminate",
            "nonce",
            "evict",
        ] {
            let root = crate::config::TestRuntimeRootGuard::new();
            dedupe::run_serial_test(async {
                scenario(Some(step), invalid, Some("before-bind"), None).await;
            });
            drop(root);
        }
    }
}
