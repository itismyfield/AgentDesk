use super::super::{EndpointDoc, body_param, ep, header_param, path_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/decisions",
            "agents",
            "Store a Decision request an agent asks its operator: a question with two to five options. The request is stored once per idempotency key within the agent and its logical channel; the same key, body and capability answers the same request (created=false), another body or capability gets 409 idempotency_conflict. Only the SHA-256 of the requester capability is stored. Nothing is posted to Discord yet: the answer is 202 with publication=pending, and no button or notice exists. Option labels and keys must be distinct and may not read as a bare number; recommendation_percent is given for every option (summing to 100) or for none. The body cannot name a category, forum, operator or result. Only a non-browser process on this host or a holder of the server auth_token as Bearer may call it; any other caller gets 403 caller_not_allowed.",
        )
        .with_params([
            (
                "X-AgentDesk-Decision-Owner",
                header_param(
                    "string",
                    true,
                    "Requester capability: 64 lowercase hex digits (256 random bits) the caller keeps privately",
                ),
            ),
            ("agent_id", body_param("string", true, "Requesting agent id")),
            (
                "target",
                body_param("object", true, "{channel_id}: the agent's channel or a thread under it"),
            ),
            ("provider_hint", body_param("string", false, "Provider of the target channel")),
            ("ref", body_param("string", true, "Short reference the requester uses for this request")),
            (
                "idempotency_key",
                body_param("string", false, "Stable key of this ask; defaults to ref"),
            ),
            ("title", body_param("string", true, "The question")),
            ("context", body_param("string", false, "Background and why a decision is needed")),
            (
                "options",
                body_param("array", true, "Two to five of {label, key?, detail?, recommendation_percent?}"),
            ),
        ])
        .with_example(
            json!({"header": {"X-AgentDesk-Decision-Owner": "<64 hex>"}, "body": {"agent_id": "family-counsel", "target": {"channel_id": "1473922824350601297"}, "ref": "deploy-window", "title": "오늘 배포할까요?", "context": "검증은 끝났고 트래픽이 낮습니다.", "options": [{"label": "진행", "detail": "지금 배포", "recommendation_percent": 70}, {"label": "보류", "detail": "내일 오전", "recommendation_percent": 30}]}}),
            json!({"ok": true, "request_id": "7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e", "created": true, "publication": "pending"}),
        )
        .with_error_example(
            409,
            json!({"body": {"agent_id": "family-counsel", "ref": "deploy-window"}}),
            json!({"ok": false, "error": "idempotency_conflict"}),
        ),
        ep(
            "GET",
            "/api/decisions/{id}",
            "agents",
            "Read a stored Decision request as its requester: its agent, channels, ref, title, context and options, with publication=pending while nothing is posted. The caller must present the capability the request was asked under; without it, with another requester's, or for an id nobody asked, the answer is 403 not_request_owner and carries nothing of the request. The capability and its hash are never returned. 400 invalid_request_id, 503 decision_store_unavailable. Only a non-browser process on this host or a holder of the server auth_token as Bearer may call it.",
        )
        .with_params([
            ("id", path_param("Request id (UUID) the ask answered")),
            (
                "X-AgentDesk-Decision-Owner",
                header_param("string", true, "The capability this request was asked under"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e"}, "header": {"X-AgentDesk-Decision-Owner": "<64 hex>"}}),
            json!({"ok": true, "request_id": "7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e", "agent_id": "family-counsel", "logical_channel_id": "1473922824350601297", "target_channel_id": "1473922824350601297", "ref": "deploy-window", "title": "오늘 배포할까요?", "context": "검증은 끝났고 트래픽이 낮습니다.", "created_at": "2026-11-25T04:00:00Z", "options": [{"label": "진행", "key": null, "detail": "지금 배포", "recommendation_percent": 70}, {"label": "보류", "key": null, "detail": "내일 오전", "recommendation_percent": 30}], "publication": "pending"}),
        )
        .with_error_example(
            403,
            json!({"path": {"id": "7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e"}}),
            json!({"ok": false, "error": "not_request_owner"}),
        ),
    ]
}
