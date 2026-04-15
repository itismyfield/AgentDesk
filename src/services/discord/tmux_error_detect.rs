pub(super) fn is_prompt_too_long_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("conversation too long")
        || lower.contains("context window")
}

pub(super) fn is_auth_error_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("not logged in")
        || lower.contains("authentication error")
        || lower.contains("unauthorized")
        || lower.contains("please run /login")
        || lower.contains("oauth")
        || lower.contains("access token could not be refreshed")
        || (lower.contains("refresh token")
            && (lower.contains("expired")
                || lower.contains("invalid")
                || lower.contains("revoked")))
        || lower.contains("token expired")
        || lower.contains("invalid api key")
        || (lower.contains("api key")
            && (lower.contains("missing")
                || lower.contains("invalid")
                || lower.contains("expired")))
}

pub(super) fn detect_provider_overload_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    let looks_overloaded = lower.contains("selected model is at capacity")
        || lower.contains("model is at capacity")
        || (lower.contains("at capacity") && lower.contains("model"))
        || lower.contains("try a different model")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("provider overloaded")
        || lower.contains("server overloaded")
        || lower.contains("service overloaded")
        || lower.contains("overloaded")
        || lower.contains("please try again later");

    if looks_overloaded {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_provider_overload_message, is_auth_error_message, is_prompt_too_long_message,
    };
    use crate::services::discord::tmux::{WatcherToolState, process_watcher_lines};
    use crate::services::session_backend::StreamLineState;

    #[test]
    fn watcher_detects_provider_overload_from_structured_errors() {
        let mut buffer = concat!(
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"Selected model is at capacity. Please try a different model.\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert_eq!(
            outcome.provider_overload_message.as_deref(),
            Some("Selected model is at capacity. Please try a different model.")
        );
        assert!(full_response.is_empty());
    }

    #[test]
    fn watcher_detects_plain_text_provider_overload_line() {
        let mut buffer =
            "Selected model is at capacity. Please try a different model.\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert_eq!(
            outcome.provider_overload_message.as_deref(),
            Some("Selected model is at capacity. Please try a different model.")
        );
        assert!(full_response.is_empty());
    }

    #[test]
    fn overload_detects_rate_limit_text() {
        assert!(detect_provider_overload_message("Rate limit exceeded").is_some());
        assert!(detect_provider_overload_message("rate limit reached for model").is_some());
    }

    #[test]
    fn overload_detects_too_many_requests() {
        assert!(detect_provider_overload_message("Too many requests").is_some());
        assert!(
            detect_provider_overload_message("429 Too Many Requests — please slow down").is_some()
        );
    }

    #[test]
    fn overload_detects_server_overloaded_variants() {
        assert!(detect_provider_overload_message("provider overloaded").is_some());
        assert!(detect_provider_overload_message("Server overloaded").is_some());
        assert!(detect_provider_overload_message("Service overloaded").is_some());
        assert!(detect_provider_overload_message("The API is overloaded right now").is_some());
    }

    #[test]
    fn overload_detects_please_try_again_later() {
        assert!(detect_provider_overload_message("Please try again later.").is_some());
    }

    #[test]
    fn overload_detects_at_capacity_with_model() {
        assert!(
            detect_provider_overload_message(
                "The selected model is at capacity. Please try a different model."
            )
            .is_some()
        );
        assert!(detect_provider_overload_message("model is at capacity").is_some());
        assert!(detect_provider_overload_message("This model is currently at capacity").is_some());
    }

    #[test]
    fn overload_ignores_empty_and_normal_text() {
        assert!(detect_provider_overload_message("").is_none());
        assert!(detect_provider_overload_message("   ").is_none());
        assert!(detect_provider_overload_message("Hello world").is_none());
        assert!(detect_provider_overload_message("Build succeeded").is_none());
        assert!(
            detect_provider_overload_message(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}"
            )
            .is_none()
        );
    }

    #[test]
    fn overload_preserves_original_message_text() {
        let msg = "  Selected model is at capacity. Please try a different model.  ";
        let result = detect_provider_overload_message(msg).unwrap();
        assert_eq!(result, msg.trim());
    }

    #[test]
    fn prompt_too_long_detects_all_variants() {
        assert!(is_prompt_too_long_message("prompt is too long"));
        assert!(is_prompt_too_long_message("Error: prompt too long"));
        assert!(is_prompt_too_long_message("context_length_exceeded"));
        assert!(is_prompt_too_long_message("conversation too long"));
        assert!(is_prompt_too_long_message("exceeded context window"));
    }

    #[test]
    fn prompt_too_long_ignores_normal() {
        assert!(!is_prompt_too_long_message("everything is fine"));
        assert!(!is_prompt_too_long_message(""));
    }

    #[test]
    fn auth_error_detects_all_variants() {
        assert!(is_auth_error_message("not logged in"));
        assert!(is_auth_error_message("Authentication error"));
        assert!(is_auth_error_message("Unauthorized"));
        assert!(is_auth_error_message("Please run /login first"));
        assert!(is_auth_error_message("OAuth token refresh failed"));
        assert!(is_auth_error_message("access token could not be refreshed"));
        assert!(is_auth_error_message("refresh token expired"));
        assert!(is_auth_error_message("Token expired"));
        assert!(is_auth_error_message("Invalid API key"));
        assert!(is_auth_error_message("API key is missing"));
        assert!(is_auth_error_message("API key expired"));
    }

    #[test]
    fn auth_error_ignores_normal() {
        assert!(!is_auth_error_message("Build succeeded"));
        assert!(!is_auth_error_message(""));
    }

    #[test]
    fn overload_in_structured_result_does_not_populate_response() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working...\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"Too many requests\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert!(
            !full_response.contains("Too many requests"),
            "overload error should not leak into full_response, got: {full_response}"
        );
    }

    #[test]
    fn overload_in_plain_text_does_not_populate_response() {
        let mut buffer = "Server overloaded, please retry later\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert!(full_response.is_empty());
    }

    #[test]
    fn prompt_too_long_error_is_not_flagged_as_overload() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"prompt is too long\"}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_prompt_too_long);
        assert!(!outcome.is_provider_overloaded);
    }

    #[test]
    fn auth_error_is_not_flagged_as_overload() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"not logged in\"}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert_eq!(outcome.auth_error_message.as_deref(), Some("not logged in"));
        assert!(!outcome.is_provider_overloaded);
    }

    #[test]
    fn plain_text_auth_error_is_detected_and_preserved() {
        let mut buffer = "access token could not be refreshed\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert_eq!(
            outcome.auth_error_message.as_deref(),
            Some("access token could not be refreshed")
        );
        assert!(full_response.is_empty());
    }

    #[test]
    fn mixed_auth_and_overload_errors_sets_both_flags() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"not logged in\",\"server overloaded\"]}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert!(outcome.is_provider_overloaded);
    }
}
