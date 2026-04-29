pub const TURN_ACTIVE: &str = "turn_active";
pub const AWAITING_BG: &str = "awaiting_bg";
pub const AWAITING_USER: &str = "awaiting_user";
pub const IDLE: &str = "idle";
pub const DISCONNECTED: &str = "disconnected";
pub const ABORTED: &str = "aborted";

pub const LEGACY_WORKING: &str = "working";

pub fn normalize_session_status(raw_status: Option<&str>, active_children: i32) -> &'static str {
    match raw_status
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(IDLE)
        .to_ascii_lowercase()
        .as_str()
    {
        TURN_ACTIVE => TURN_ACTIVE,
        AWAITING_BG => AWAITING_BG,
        AWAITING_USER => AWAITING_USER,
        IDLE => {
            if active_children > 0 {
                AWAITING_BG
            } else {
                IDLE
            }
        }
        DISCONNECTED => DISCONNECTED,
        ABORTED => ABORTED,
        LEGACY_WORKING => {
            if active_children > 0 {
                AWAITING_BG
            } else {
                TURN_ACTIVE
            }
        }
        _ => {
            if active_children > 0 {
                AWAITING_BG
            } else {
                IDLE
            }
        }
    }
}

pub fn normalize_incoming_session_status(raw_status: Option<&str>) -> &'static str {
    normalize_session_status(raw_status, 0)
}

pub fn is_active_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        TURN_ACTIVE | LEGACY_WORKING
    )
}

pub fn is_bg_wait_status(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case(AWAITING_BG)
}

pub fn is_user_wait_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        AWAITING_USER | IDLE
    )
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        DISCONNECTED | ABORTED
    )
}

pub fn is_live_status(status: &str) -> bool {
    !is_terminal_status(status)
}
