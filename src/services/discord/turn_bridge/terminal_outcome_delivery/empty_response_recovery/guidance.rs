pub(super) fn empty_response_guidance(rx_disconnected: bool) -> &'static str {
    if rx_disconnected {
        "⚠️ provider 프로세스가 응답을 만들기 전에 종료됐어요.\n자동 복구에서도 전달할 응답을 찾지 못했습니다. 같은 요청을 다시 시도해 주세요. 문제가 반복되면 `!clear`로 세션을 초기화한 뒤 다시 보내 주세요."
    } else {
        "⚠️ provider가 응답 내용 없이 턴을 종료했어요.\n자동 복구에서도 전달할 응답을 찾지 못했습니다. 같은 요청을 다시 시도해 주세요. 문제가 반복되면 `!clear`로 세션을 초기화한 뒤 다시 보내 주세요."
    }
}

#[cfg(test)]
mod tests {
    use super::empty_response_guidance;

    #[test]
    fn disconnected_empty_response_explains_recovery_and_next_actions() {
        let response = empty_response_guidance(true);

        assert!(response.contains("프로세스가 응답을 만들기 전에 종료"));
        assert!(response.contains("자동 복구"));
        assert!(response.contains("다시 시도"));
        assert!(response.contains("`!clear`"));
        assert!(!response.contains("(No response"));
    }

    #[test]
    fn completed_empty_response_explains_recovery_and_next_actions() {
        let response = empty_response_guidance(false);

        assert!(response.contains("응답 내용 없이 턴을 종료"));
        assert!(response.contains("자동 복구"));
        assert!(response.contains("다시 시도"));
        assert!(response.contains("`!clear`"));
        assert!(!response.contains("(No response"));
    }
}
