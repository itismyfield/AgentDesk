use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{ChannelId, HttpBuilder};
use std::sync::{Arc, Mutex};

use crate::services::discord::gateway::{DiscordGateway, TurnGateway};
use crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway;
use crate::services::discord::turn_end_summary::tests::{Isolation, row};
use crate::services::discord::turn_end_summary::{LiveSummary, post_live};
use crate::services::provider::ProviderKind;
use crate::services::turn_summary_line::Outcome;

#[derive(Clone, Copy, Debug)]
enum Reply {
    RateLimited,
    MalformedLimit,
    GlobalLimit,
    ServerError,
    InvalidJson,
    Redirect,
}

fn message(channel: u64) -> serde_json::Value {
    serde_json::json!({
        "id":"900001", "channel_id":channel.to_string(), "content":"-# ✅ 완료",
        "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
        "timestamp":"2026-10-20T00:00:00+00:00","edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false,"type":0
    })
}

struct Recorder {
    http: Arc<poise::serenity_prelude::Http>,
    calls: Calls,
    server: tokio::task::AbortHandle,
}

type Calls = Arc<Mutex<Vec<(Method, String, serde_json::Value)>>>;

impl Recorder {
    async fn start(channel: u64, reply: Reply) -> Self {
        let calls = Calls::default();
        let recorded = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    let first = {
                        let mut calls = recorded.lock().unwrap();
                        calls.push((
                            method,
                            uri.path().into(),
                            serde_json::from_slice(&body).unwrap(),
                        ));
                        calls.len() == 1
                    };
                    // A retry receives a valid message so its extra POST reaches the oracle.
                    if !first {
                        return axum::Json(message(channel)).into_response();
                    }
                    let mut response = match reply {
                        Reply::RateLimited => (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(serde_json::json!({"code":0,"message":"rate limited"})),
                        )
                            .into_response(),
                        Reply::ServerError => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"code":0,"message":"uncertain result"})),
                        )
                            .into_response(),
                        Reply::InvalidJson => (StatusCode::OK, "invalid JSON").into_response(),
                        Reply::Redirect => StatusCode::TEMPORARY_REDIRECT.into_response(),
                        Reply::MalformedLimit | Reply::GlobalLimit => {
                            axum::Json(message(channel)).into_response()
                        }
                    };
                    let headers = response.headers_mut();
                    match reply {
                        Reply::RateLimited => {
                            headers.insert("retry-after", "0".parse().unwrap());
                        }
                        Reply::MalformedLimit => {
                            headers.insert("x-ratelimit-limit", "invalid".parse().unwrap());
                        }
                        Reply::GlobalLimit => {
                            headers.insert("x-ratelimit-global", "true".parse().unwrap());
                            headers.insert("retry-after", "0".parse().unwrap());
                        }
                        Reply::Redirect => {
                            headers.insert("location", uri.path().parse().unwrap());
                        }
                        Reply::ServerError | Reply::InvalidJson => {}
                    }
                    response
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            HttpBuilder::new("terminal-notice-test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http,
            calls,
            server: server.abort_handle(),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn strict_gateway_override(source: &str, gateway: &str) {
    let implementation = source
        .split(&format!("impl TurnGateway for {gateway} {{"))
        .nth(1)
        .unwrap();
    let method = implementation
        .split("fn send_notice_once")
        .nth(1)
        .unwrap_or_else(|| panic!("{gateway} needs its own terminal notice override"))
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(
        method
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| line.contains("confirmation::send_terminal_notice_http_once(")),
        "{gateway} terminal notice must use strict HTTP helper"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn turn_end_summary_spends_one_physical_post_and_consumes_every_response() {
    // Enabling Serenity's limiter ignores Http.proxy; pin this before any local request.
    let source = include_str!("../confirmation.rs");
    let function = source
        .split("async fn send_terminal_notice_http_once(")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    for guard in [
        ".ratelimiter_disabled(true)",
        ".retry(reqwest::retry::never())",
        ".redirect(reqwest::redirect::Policy::none())",
    ] {
        assert!(
            function
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .any(|line| line.trim().starts_with(guard)),
            "terminal notice transport guard missing: {guard}"
        );
    }
    strict_gateway_override(include_str!("../../gateway.rs"), "DiscordGateway");
    strict_gateway_override(
        include_str!("../../tui_prompt_relay/bridge_gateway.rs"),
        "TuiDirectBridgeGateway",
    );
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (index, (direct, reply)) in [false, true]
        .into_iter()
        .flat_map(|direct| {
            [
                Reply::RateLimited,
                Reply::MalformedLimit,
                Reply::GlobalLimit,
                Reply::ServerError,
                Reply::InvalidJson,
                Reply::Redirect,
            ]
            .into_iter()
            .map(move |reply| (direct, reply))
        })
        .enumerate()
    {
        let channel = 586_700 + index as u64;
        let recorder = Recorder::start(channel, reply).await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let gateway: Arc<dyn TurnGateway> = if direct {
            terminal_notice_test_gateway(recorder.http.clone(), shared)
        } else {
            Arc::new(DiscordGateway::new(
                recorder.http.clone(),
                shared,
                ProviderKind::Claude,
                None,
            ))
        };
        let summary = LiveSummary::bridge(
            ChannelId::new(channel),
            &row(channel, 1, "strict-terminal-notice"),
            0,
            Outcome::Completed,
            None,
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            post_live(gateway.as_ref(), &ProviderKind::Claude, summary.clone()),
        )
        .await
        .unwrap();
        let calls = recorder.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            1,
            "one physical terminal POST for {reply:?} direct={direct}: {calls:?}"
        );
        assert_eq!(calls[0].0, Method::POST);
        assert_eq!(calls[0].1, format!("/api/v10/channels/{channel}/messages"));
        assert_eq!(calls[0].2["content"], "-# ✅ 완료");
        assert!(calls[0].2.get("nonce").is_none());
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            post_live(gateway.as_ref(), &ProviderKind::Claude, summary),
        )
        .await
        .unwrap();
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "duplicate producer cannot re-post terminal summary after {reply:?} direct={direct}"
        );
    }
}
