use super::*;
use axum::http::StatusCode;
use std::sync::atomic::{AtomicUsize, Ordering};

struct DispatchCase {
    statuses: [StatusCode; 2],
    text: &'static str,
    detail_missing: bool,
    prefix_detail_known: bool,
    lookups: usize,
    task: bool,
    row: Option<&'static str>,
}

async fn check_intake(case: DispatchCase, channel: ChannelId) {
    let count = Arc::new(AtomicUsize::new(0));
    let lookups = count.clone();
    let linked_thread = case.text.starts_with("DISPATCH:");
    let parent = ChannelId::new(channel.get() + 100);
    let thread_path = format!("/channels/{channel}");
    let parent_path = format!("/channels/{parent}");
    let answer: StatusAnswer = Arc::new(move |_, uri| match uri.path() {
        "/api/internal/pending-dispatch-for-thread" => {
            let nth = lookups.fetch_add(1, Ordering::SeqCst);
            let status = case.statuses[nth.min(1)];
            Some((status, serde_json::json!({"dispatch_id":"dispatch-A"})))
        }
        "/api/internal/card-thread" => {
            let prefix = uri
                .query()
                .is_some_and(|query| query == "dispatch_id=dispatch-B");
            Some(
                if case.detail_missing && !(prefix && case.prefix_detail_known) {
                    (StatusCode::NOT_FOUND, serde_json::json!({}))
                } else {
                    (
                        StatusCode::OK,
                        serde_json::json!({
                            "dispatch_type":"implementation",
                            "dispatch_title":"Fixture work"
                        }),
                    )
                },
            )
        }
        path if linked_thread && path.ends_with(&thread_path) => Some((
            StatusCode::OK,
            serde_json::json!({"id":channel.to_string(), "type":11, "name":"fixture-work",
                "guild_id":"940487400000009", "parent_id":parent.to_string()}),
        )),
        path if linked_thread && path.ends_with(&parent_path) => Some((
            StatusCode::OK,
            serde_json::json!({"id":parent.to_string(), "type":0, "name":"fixture-parent",
                "guild_id":"940487400000009", "position":0, "permission_overwrites":[]} ),
        )),
        _ => None,
    });
    let host = host_with_status(&[channel, parent], answer).await;
    if linked_thread {
        sqlx::query("INSERT INTO task_dispatches (id,status,dispatch_type) VALUES ('dispatch-A','pending','review'), ('dispatch-B','pending','implementation')")
            .execute(&host.pool).await.unwrap();
    }
    let turn = paused_intake(&host, request(channel, channel.get() + 1_000, case.text)).await;
    let prompt = turn
        .prompt
        .system_prompt
        .as_deref()
        .expect("system prompt captured");
    assert_eq!(prompt.contains("[Current Task]"), case.task);
    assert_eq!(prompt.contains("Dispatch ID: dispatch-A"), case.task);
    if case.detail_missing {
        assert!(!prompt.contains("[Dispatch Contract]"), "{prompt}");
        assert!(
            !prompt.contains("PATCH /api/dispatches/dispatch-A"),
            "{prompt}"
        );
    }
    let row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        channel.get(),
    )
    .expect("inflight persisted before terminal release");
    assert_eq!(row.dispatch_id.as_deref(), case.row, "inflight dispatch");
    let channel_text = channel.get().to_string();
    let posted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for call in host.api.take() {
                let Some(body) = call.strip_prefix("POST /api/dispatched-sessions/webhook ") else {
                    continue;
                };
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                if body["status"] == "turn_active" && body["channel_id"] == channel_text {
                    return body;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn_active webhook before terminal release");
    assert_eq!(posted["dispatch_id"].as_str(), case.row, "webhook dispatch");
    assert_eq!(
        count.load(Ordering::SeqCst),
        case.lookups,
        "pending lookups"
    );
    turn.finish().await;
    host.pool.close().await;
    host.db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_8148_intake_first_hit_pins_and_first_miss_keeps_row_lookup_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_intake_first_hit_pins_and_first_miss_keeps_row_lookup_pg"
    )) {
        return;
    }
    use StatusCode as S;
    const A: Option<&str> = Some("dispatch-A");
    const B: Option<&str> = Some("dispatch-B");
    for (index, (statuses, prefix_detail, (lookups, task, row))) in [
        ([S::OK, S::SERVICE_UNAVAILABLE], None, (1, true, A)),
        ([S::SERVICE_UNAVAILABLE, S::OK], None, (2, false, A)),
        ([S::NOT_FOUND; 2], None, (2, false, None)),
        ([S::SERVICE_UNAVAILABLE; 2], None, (2, false, None)),
        ([S::OK, S::SERVICE_UNAVAILABLE], Some(true), (1, true, A)),
        (
            [S::SERVICE_UNAVAILABLE, S::NOT_FOUND],
            Some(false),
            (2, false, B),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let text = if prefix_detail.is_some() {
            "DISPATCH: dispatch-B\nfixture input"
        } else {
            "fixture input"
        };
        let case = DispatchCase {
            statuses,
            text,
            detail_missing: prefix_detail.is_some(),
            prefix_detail_known: prefix_detail == Some(true),
            lookups,
            task,
            row,
        };
        check_intake(case, ChannelId::new(81_480_600 + index as u64)).await;
    }
}
