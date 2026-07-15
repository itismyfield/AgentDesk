use super::truncate_str;

pub(super) enum ProviderErrorPresentation {
    PromptTooLong(String),
    Failure(String),
}

pub(super) fn provider_error_detail(message: &str, stderr: &str) -> String {
    if stderr.trim().is_empty() {
        format!("Error: {message}")
    } else {
        format!("Error: {message}\nstderr: {}", truncate_str(stderr, 500))
    }
}

pub(super) fn provider_error_presentation(
    message: &str,
    stderr: &str,
) -> ProviderErrorPresentation {
    let combined = format!("{message} {stderr}").to_lowercase();
    let prompt_too_long = combined.contains("prompt is too long")
        || combined.contains("prompt too long")
        || combined.contains("context_length_exceeded")
        || combined.contains("max_tokens")
        || combined.contains("context window")
        || combined.contains("token limit");
    let detail = provider_error_detail(message, stderr);

    if prompt_too_long {
        ProviderErrorPresentation::PromptTooLong(
            super::super::super::response_delivery::prompt_too_long_guidance(&detail),
        )
    } else {
        ProviderErrorPresentation::Failure(
            crate::services::discord::commands::owner_error_response(
                "provider가 응답을 완료하지 못했어요.\n같은 요청을 다시 시도해 주세요. 문제가 반복되면 `!clear`로 세션을 초기화한 뒤 다시 보내 주세요.",
                &detail,
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderErrorPresentation, provider_error_presentation};

    #[test]
    fn prompt_too_long_guard_selects_actionable_folded_presentation() {
        let ProviderErrorPresentation::PromptTooLong(response) = provider_error_presentation(
            "request failed",
            "context_length_exceeded: private provider detail",
        ) else {
            panic!("context-length errors must use prompt-too-long guidance");
        };

        assert!(response.starts_with("⚠️ 현재 대화가 provider의 컨텍스트 한도를 넘었어요."));
        assert!(response.contains("`/compact`"));
        assert!(response.contains("요청을 짧게"));
        assert!(response.contains("||**상세**\n```text\n__prompt too long__"));
        assert!(response.contains("Error: request failed"));
        assert!(response.contains("context_length_exceeded: private provider detail"));
        assert!(response.ends_with("```||"));
    }

    #[test]
    fn provider_error_guidance_folds_detail_and_suggests_recovery() {
        let ProviderErrorPresentation::Failure(response) = provider_error_presentation(
            "request failed",
            "private stderr with ``` fence and || spoiler",
        ) else {
            panic!("ordinary provider errors must not use prompt-too-long guidance");
        };

        assert!(response.starts_with("⚠️ provider가 응답을 완료하지 못했어요."));
        assert!(response.contains("다시 시도"));
        assert!(response.contains("`!clear`"));
        assert!(response.contains("||**상세**\n```text\nError: request failed"));
        assert!(response.ends_with("```||"));
        assert!(!response.contains("``` fence"));
        assert!(!response.contains("|| spoiler"));
    }

    #[test]
    fn provider_error_without_stderr_keeps_message_in_folded_detail() {
        let ProviderErrorPresentation::Failure(response) =
            provider_error_presentation("bare provider failure", "")
        else {
            panic!("ordinary provider errors must not use prompt-too-long guidance");
        };

        assert!(response.contains("같은 요청을 다시 시도"));
        assert!(response.contains("||**상세**\n```text\nError: bare provider failure\n```||"));
        assert!(!response.contains("stderr:"));
    }
}
