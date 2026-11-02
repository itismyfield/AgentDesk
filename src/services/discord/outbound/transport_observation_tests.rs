use crate::services::cluster::intake_router_hook::owner_record::output;
use crate::services::discord::outbound::raw_output::{self, Operation};
use crate::services::legacy_body_census as census;
use crate::services::tui_o::repost::io::tests::{
    CHANNEL, Mock, Reply, always, dispatch, reply, route_429,
};
use crate::services::tui_o::repost::send::WireOutcome;
use crate::services::tui_o::writer::{DiscordPort, PostOutcome};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::{sync::Arc, time::Duration};

fn isolated(test: &str) -> bool {
    if std::env::var("ADK_TRANSPORT_OBSERVATION_TEST").is_ok() {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("ADK_TRANSPORT_OBSERVATION_TEST", test)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env_remove("AGENTDESK_CONFIG")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&result.stdout).contains("running 1 test"));
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    true
}

async fn journal() -> PathBuf {
    let root = PathBuf::from(std::env::var("AGENTDESK_ROOT_DIR").unwrap());
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    std::fs::write(
        root.join("runtime/dcserver.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    census::ensure_ready().await;
    assert_eq!(census::health_json()["journal_ready"], true);
    root.join("legacy_body_census").join(format!(
        "{}.jsonl",
        crate::services::process_boot::initialize().id
    ))
}

fn events(path: &Path, case: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "target" && row["census"]["case"] == case)
        .collect()
}

fn phases(path: &Path, case: &str) -> Vec<(String, Option<String>)> {
    events(path, case)
        .iter()
        .map(|row| {
            (
                row["phase"].as_str().unwrap().to_owned(),
                row["outcome"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn message(id: u64) -> Reply {
    reply(
        "200 OK",
        &[],
        &json!({
            "id":id.to_string(), "channel_id":CHANNEL.to_string(), "content":"body",
            "author":{"id":"42","username":"bot","discriminator":"0001","avatar":null},
            "timestamp":"2026-11-02T00:00:00+00:00","edited_timestamp":null,
            "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
            "attachments":[],"embeds":[],"pinned":false,"type":0
        })
        .to_string(),
    )
}

fn client(mock: &Mock) -> serenity::Http {
    serenity::HttpBuilder::new("test-token")
        .proxy(mock.base.clone())
        .ratelimiter_disabled(true)
        .build()
}

#[tokio::test]
async fn raw_requests_observe_body_operations_and_retry_once() {
    if isolated(
        concat!(
            module_path!(),
            "::raw_requests_observe_body_operations_and_retry_once"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = journal().await;
    let channel = ChannelId::new(CHANNEL);
    for (case, replies, outcome, wire) in [
        ("post", vec![message(9001)], Some("confirmed"), 1),
        ("edit", vec![message(9001)], Some("confirmed"), 1),
        ("delete", vec![reply("204 No Content", &[], "")], None, 1),
        ("untagged", vec![message(9001)], None, 1),
        (
            "rejected",
            vec![reply(
                "403 Forbidden",
                &[],
                r#"{"code":50013,"message":"denied"}"#,
            )],
            Some("rejected"),
            1,
        ),
        (
            "uncertain",
            vec![reply("500 Internal Server Error", &[], "{}")],
            Some("uncertain"),
            1,
        ),
        (
            "unreadable",
            vec![reply("200 OK", &[], "{")],
            Some("uncertain"),
            1,
        ),
        (
            "retry",
            vec![route_429("0"), message(9001)],
            Some("confirmed"),
            2,
        ),
    ] {
        let mock = Mock::start(replies).await;
        let http = client(&mock);
        let tagged = (case != "untagged").then(|| json!({"case":case}));
        let result = raw_output::census_scope(tagged, async {
            match case {
                "edit" => crate::services::discord::http::edit_channel_message(&http, channel, MessageId::new(9001), "body").await.map(|_| ()),
                "delete" => crate::services::discord::http::delete_channel_message(&http, channel, MessageId::new(9001)).await,
                "retry" => raw_output::request(channel, Operation::Post, async {
                    let first = channel.send_message(&http, serenity::CreateMessage::new().content("body")).await;
                    assert!(matches!(&first, Err(serenity::Error::Http(e)) if e.status_code().is_some_and(|s| s.as_u16()==429)));
                    channel.send_message(&http, serenity::CreateMessage::new().content("body")).await.map(|_| ())
                }).await,
                _ => crate::services::discord::http::send_channel_message(&http, channel, "body").await.map(|_| ()),
            }
        }).await;
        assert_eq!(
            result.is_ok(),
            !matches!(case, "rejected" | "uncertain" | "unreadable"),
            "{case}"
        );
        assert_eq!(mock.requests().len(), wire, "{case}");
        let expected = outcome.map_or_else(Vec::new, |result| {
            vec![
                ("begin".into(), None),
                ("result".into(), Some(result.into())),
            ]
        });
        assert_eq!(
            phases(&log, case),
            expected,
            "{case}: evidence must match the real transport result"
        );
        for event in events(&log, case) {
            assert_eq!(event["channel"], CHANNEL);
            assert_eq!(event["op"], if case == "edit" { "EDIT" } else { "POST" });
            assert!(event["at_ms"].as_i64().is_some());
        }
    }
}

#[tokio::test]
async fn nested_observation_preserves_owner_fences_and_uncertain_results_pg() {
    if isolated(
        concat!(
            module_path!(),
            "::nested_observation_preserves_owner_fences_and_uncertain_results_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = journal().await;
    let (_db, pool, origin) = output::tests::fixture(&CHANNEL.to_string()).await;
    let scope = output::Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let artifact = origin.seal("body");
    let channel = ChannelId::new(CHANNEL);
    for case in [
        "owner_inside",
        "census_inside",
        "changed_payload",
        "census_changed_payload",
        "timeout",
        "cancelled",
        "revoked",
        "census_revoked",
    ] {
        if case == "revoked" {
            output::tests::revoke(&pool, &origin).await;
        }
        let blocked = case.ends_with("changed_payload") || case.ends_with("revoked");
        let stalled = matches!(case, "timeout" | "cancelled");
        let mock = Mock::start(vec![if stalled { Reply::Stall } else { message(9001) }]).await;
        let http = client(&mock);
        let tag = Some(json!({"case":case}));
        let body = if case.ends_with("changed_payload") {
            "changed"
        } else {
            "body"
        };
        let send = crate::services::discord::http::send_channel_message(&http, channel, body);
        let mut call: std::pin::Pin<
            Box<dyn std::future::Future<Output = serenity::Result<serenity::Message>>>,
        > = if case.starts_with("census_") {
            Box::pin(raw_output::scope(
                Some(scope.clone()),
                Some(&artifact),
                body,
                raw_output::census_scope(tag, send),
            ))
        } else {
            Box::pin(raw_output::census_scope(
                tag,
                raw_output::scope(Some(scope.clone()), Some(&artifact), body, send),
            ))
        };
        if stalled {
            tokio::select! {
                value = &mut call => panic!("request ended before the recording server: {value:?}"),
                _ = tokio::time::timeout(Duration::from_secs(5), async {
                    while mock.requests().is_empty() { tokio::task::yield_now().await; }
                }) => {}
            }
            assert_eq!(mock.requests().len(), 1);
            assert_eq!(
                phases(&log, case),
                [("begin".into(), None)],
                "begin must be stable before the raw request"
            );
            if case == "timeout" {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(61)).await;
                let error = tokio::time::timeout(Duration::from_secs(3), call)
                    .await
                    .expect("the original raw deadline must remain bounded")
                    .unwrap_err();
                assert!(error.to_string().contains("deadline"));
                tokio::time::resume();
            } else {
                drop(call);
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if phases(&log, case).len() == 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                phases(&log, case),
                [
                    ("begin".into(), None),
                    ("result".into(), Some("uncertain".into()))
                ]
            );
        } else {
            assert_eq!(call.await.is_ok(), !blocked, "{case}");
            assert_eq!(mock.requests().len(), usize::from(!blocked), "{case}");
            let expected = if blocked {
                vec![("not_sent".into(), None)]
            } else {
                vec![
                    ("begin".into(), None),
                    ("result".into(), Some("confirmed".into())),
                ]
            };
            assert_eq!(phases(&log, case), expected, "{case}");
        }
    }
    pool.close().await;
}

fn o_count() -> u64 {
    census::health_json()["o_created"]
        .as_array()
        .map_or(0, |rows| {
            rows.iter()
                .filter(|row| row["channel"] == CHANNEL)
                .map(|row| row["confirmed"].as_u64().unwrap())
                .sum()
        })
}

#[tokio::test]
async fn o_ack_counts_only_fresh_created_responses() {
    if isolated(
        concat!(
            module_path!(),
            "::o_ack_counts_only_fresh_created_responses"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = journal().await;
    for (index, case) in [
        "nonce_echo",
        "post",
        "post_origin",
        "repost",
        "refused",
        "unreadable",
        "history",
    ]
    .into_iter()
    .enumerate()
    {
        let id = (((chrono::Utc::now().timestamp_millis() + 1000 - 1_420_070_400_000) as u64)
            << 22)
            | index as u64;
        let response = match case {
            "nonce_echo" => message(9001),
            "refused" => reply("403 Forbidden", &[], r#"{"code":50013,"message":"denied"}"#),
            "unreadable" => reply("200 OK", &[], "{"),
            _ => message(id),
        };
        let mock = Mock::start(vec![response]).await;
        let before = o_count();
        raw_output::census_scope(Some(json!({"case":case})), async {
            if matches!(case, "repost" | "nonce_echo") {
                let result = dispatch(&mock.port(), always()).await;
                assert!(matches!(result.outcome, WireOutcome::Created(_)));
            } else {
                let port = crate::services::discord::outbound::o_writer_io::GatewayPort::new(
                    Arc::new(client(&mock)),
                    42,
                );
                if case == "history" {
                    assert!(port.message(CHANNEL, id).await.unwrap().is_some());
                } else {
                    let result = if case == "post_origin" {
                        port.post_origin(CHANNEL, "body".into(), None).await
                    } else {
                        port.post(CHANNEL, "body".into()).await
                    };
                    assert_eq!(
                        matches!(result, PostOutcome::Created(_)),
                        !matches!(case, "refused" | "unreadable")
                    );
                }
            }
        })
        .await;
        assert_eq!(mock.requests().len(), 1, "{case}");
        assert_eq!(
            o_count() - before,
            u64::from(matches!(case, "post" | "post_origin" | "repost")),
            "{case}"
        );
        assert!(events(&log, case).is_empty(), "O is never a Legacy request");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn claim_api_scopes_no_claim_transport_without_creating_a_tag() {
    if isolated(
        concat!(
            module_path!(),
            "::claim_api_scopes_no_claim_transport_without_creating_a_tag"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = journal().await;
    let mock = Mock::start(vec![message(9010)]).await;
    let http = client(&mock);
    let result = raw_output::census_scope(Some(json!({"case":"claim-none"})), async {
        crate::services::tui_o::cutover::claim_then_send_at(
            crate::services::tui_o::cutover::BodySite::StandbyRelay,
            None,
            || async {
                crate::services::discord::http::send_channel_message(
                    &http,
                    ChannelId::new(CHANNEL),
                    "body",
                )
                .await
            },
        )
        .await
    })
    .await;
    assert!(matches!(
        result,
        Ok(crate::services::tui_o::cutover::BodySend::Sent(Ok(_)))
    ));
    assert_eq!(mock.requests().len(), 1);
    let recorded = events(&log, "claim-none");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0]["census"]["site"], "W06");
    assert_eq!(recorded[0]["census"]["reason"], "NO_CLAIM");
    let mock = Mock::start(vec![message(9013)]).await;
    let http = client(&mock);
    crate::services::tui_o::cutover::claim_then_send_at(
        crate::services::tui_o::cutover::BodySite::StandbyRelay,
        None,
        || async {
            assert!(raw_output::CONTEXT.try_with(|_| ()).is_err());
            crate::services::discord::http::send_channel_message(
                &http,
                ChannelId::new(CHANNEL),
                "body",
            )
            .await
            .unwrap();
        },
    )
    .await
    .unwrap();
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(events(&log, "claim-none").len(), 2);
    assert_eq!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| serde_json::from_str::<Value>(line).unwrap()["kind"] == "target")
            .count(),
        2,
        "a site argument cannot turn None into an operational body tag"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn claim_held_context_preserves_lifetime_none_guard_and_outer_site() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::{self, BodyClaim, BodySite};
    if isolated(
        concat!(
            module_path!(),
            "::claim_held_context_preserves_lifetime_none_guard_and_outer_site"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = journal().await;
    for (case, claimed, reason) in [
        ("held-pending", true, "PENDING_RELEASED"),
        ("held-none", false, "NO_CLAIM"),
    ] {
        let _boot = cutover::test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
        let candidate = cutover::test_override::with_channels(|boot| {
            boot.unwrap().candidate(CHANNEL).unwrap().clone()
        });
        let mock = Mock::start(vec![message(9011), message(9011)]).await;
        let http = client(&mock);
        let claim = claimed.then(|| BodyClaim::new(CHANNEL, Some(ClaudeTui)));
        let (sent, context) = raw_output::census_scope(
            Some(json!({"case":case})),
            cutover::claim_then_send_held_at(BodySite::StandbyRelay, claim, || async {
                crate::services::discord::http::send_channel_message(
                    &http,
                    ChannelId::new(CHANNEL),
                    "body",
                )
                .await
            }),
        )
        .await
        .unwrap();
        assert!(matches!(sent, cutover::BodySend::Sent(Ok(_))));
        assert_eq!(candidate.sends(), (u64::from(claimed), 0));
        context
            .run(async {
                tokio::task::yield_now().await;
                crate::services::discord::http::edit_channel_message(
                    &http,
                    ChannelId::new(CHANNEL),
                    MessageId::new(9011),
                    "body",
                )
                .await
                .unwrap();
            })
            .await;
        assert_eq!(candidate.sends(), (u64::from(claimed), 0));
        drop(context);
        assert_eq!(candidate.sends(), (u64::from(claimed), u64::from(claimed)));
        assert_eq!(mock.requests().len(), 2);
        let recorded = events(&log, case);
        assert_eq!(
            recorded.len(),
            4,
            "each real transport has one begin/result"
        );
        for event in recorded {
            assert_eq!(event["census"]["site"], "W06");
            assert_eq!(event["census"]["reason"], reason);
        }
    }
    let mock = Mock::start(vec![message(9012)]).await;
    let http = client(&mock);
    raw_output::census_scope(
        Some(json!({"case":"outer-claim", "site":"W30", "reason":"RELEASED"})),
        cutover::claim_then_send_at(BodySite::StandbyRelay, None, || async {
            cutover::claim_then_send_at(BodySite::StandbyRelay, None, || async {
                crate::services::discord::http::send_channel_message(
                    &http,
                    ChannelId::new(CHANNEL),
                    "body",
                )
                .await
                .unwrap();
            })
            .await
            .unwrap();
        }),
    )
    .await
    .unwrap();
    assert_eq!(mock.requests().len(), 1);
    let recorded = events(&log, "outer-claim");
    assert_eq!(recorded.len(), 2);
    for event in recorded {
        assert_eq!(event["census"]["site"], "W30");
        assert_eq!(event["census"]["reason"], "RELEASED");
    }
}
