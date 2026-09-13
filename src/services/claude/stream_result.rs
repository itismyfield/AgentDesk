//! Capture the result shared by native and typed terminal stream events.

use super::debug_log;

pub(super) fn capture_terminal_result(
    result: &str,
    session_id: &Option<String>,
    final_result: &mut Option<String>,
    last_session_id: &mut Option<String>,
) {
    let result_preview: String = result.chars().take(100).collect();
    debug_log(&format!(
        "  >>> Done: result_len={}, session_id={:?}, preview={:?}",
        result.len(),
        session_id,
        result_preview
    ));
    *final_result = Some(result.to_owned());
    if session_id.is_some() {
        *last_session_id = session_id.clone();
    }
}
