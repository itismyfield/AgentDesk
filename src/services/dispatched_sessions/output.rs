//! Backend-neutral output API with authoritative owner fencing.
use super::*;
pub(super) struct Capture {
    pub backend: &'static str,
    pub format: &'static str,
    pub available: bool,
    pub alive: bool,
    pub text: String,
    pub reason: Option<&'static str>,
    pub truncated: bool,
}

pub(super) fn capture(session_name: &str, lines: i32) -> Capture {
    if let Some(output) = crate::services::session_backend::capture_process_output(
        session_name,
        lines.clamp(1, 2000) as usize,
    ) {
        return match output {
            Ok(output) => Capture {
                backend: "process",
                format: "jsonl",
                available: true,
                alive: output.alive,
                text: output.text,
                reason: None,
                truncated: output.truncated,
            },
            Err(reason) => unavailable("process", "jsonl", reason),
        };
    }
    #[cfg(unix)]
    {
        if let Some(text) =
            crate::services::platform::tmux::capture_pane(session_name, -lines.clamp(1, 2000))
        {
            let mut start = text.len().saturating_sub(256 * 1024);
            while !text.is_char_boundary(start) {
                start += 1;
            }
            return Capture {
                backend: "tmux",
                format: "terminal",
                available: true,
                alive: true,
                text: text[start..].into(),
                reason: None,
                truncated: start > 0,
            };
        }
    }
    unavailable("unattached", "unknown", "session_output_not_attached")
}

fn unavailable(backend: &'static str, format: &'static str, reason: &'static str) -> Capture {
    Capture {
        backend,
        format,
        available: false,
        alive: false,
        text: String::new(),
        reason: Some(reason),
        truncated: false,
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_missing_process_is_unavailable_instead_of_empty_tmux_success() {
        let capture = super::capture("nonexistent-output-fixture", 80);
        assert!(!capture.available);
        assert!(!capture.alive);
        assert_eq!(capture.backend, "unattached");
        assert_eq!(capture.reason, Some("session_output_not_attached"));
    }
}

/// GET /api/sessions/{id}/output?lines=N (legacy alias: tmux-output).
/// Resolves the authoritative owner before reading its bound process output
/// or tmux pane. Native process handles cannot be reattached after a restart.
pub async fn tmux_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(params): Query<TmuxOutputQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let requested_lines = params.lines.unwrap_or(TMUX_OUTPUT_DEFAULT_LINES);
    let effective_lines = requested_lines.max(1).min(TMUX_OUTPUT_MAX_LINES);

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    // Lookup session row. Prefer Postgres (authoritative) when available.
    let session_row = match dispatched_sessions_db::load_session_by_id_pg(pool, id).await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    let Some((session_key, agent_id, provider, status, owner_instance_id)) = session_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("session #{id} not found"),
                "session_id": id,
            })),
        );
    };

    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(&state);
    if let Err(response) = crate::services::session_forwarding::enforce_receiver_fence(
        &headers,
        owner_instance_id.as_deref(),
        forward_context.cluster_instance_id.as_deref(),
    ) {
        return response;
    }
    if !crate::services::session_forwarding::is_forwarded_request(&headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            &forward_context,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_tmux_output(
                    &forward_context,
                    &target,
                    id,
                    effective_lines,
                )
                .await;
            }
            crate::services::session_forwarding::ForwardResolution::Unavailable {
                status,
                body,
            } => {
                return (status, Json(body));
            }
        }
    }

    let tmux_name = match tmux_name_from_session_key(&session_key) {
        Some(name) => name,
        _ => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "session #{id} session_key does not follow legacy host:tmux or namespaced provider/token/host:tmux format"
                    ),
                    "session_id": id,
                    "session_key": session_key,
                })),
            );
        }
    };

    let captured_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0);

    let capture_name = tmux_name.clone();
    let capture =
        match tokio::task::spawn_blocking(move || capture(&capture_name, effective_lines)).await {
            Ok(capture) => capture,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":"session output capture failed"})),
                );
            }
        };

    (
        StatusCode::OK,
        Json(json!({
            "session_id": id,
            "session_key": session_key,
            "tmux_name": tmux_name,
            "tmux_alive": capture.backend == "tmux" && capture.alive,
            "backend": capture.backend,
            "alive": capture.alive,
            "available": capture.available,
            "output_format": capture.format,
            "unavailable_reason": capture.reason,
            "truncated": capture.truncated,
            "agent_id": agent_id,
            "provider": provider,
            "status": status,
            "lines_requested": requested_lines,
            "lines_effective": effective_lines,
            "recent_output": capture.text,
            "captured_at_ms": captured_at_ms,
        })),
    )
}
