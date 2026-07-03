//! Shared parser for the six turn-output controller cutover env flags.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ControllerRolloutFlagParse {
    enabled: bool,
    warn_enabled: bool,
}

pub(in crate::services::discord) fn enabled_from_env(var: &str) -> bool {
    let raw = std::env::var(var).ok();
    let parsed = parse_raw(raw.as_deref());
    if parsed.warn_enabled {
        if let Some(raw) = raw.as_deref() {
            warn_invalid_enabled_once(var, raw);
        }
    }
    parsed.enabled
}

#[cfg(test)]
pub(in crate::services::discord) fn enabled_from_raw(raw: Option<&str>) -> bool {
    parse_raw(raw).enabled
}

fn parse_raw(raw: Option<&str>) -> ControllerRolloutFlagParse {
    match raw.map(str::trim) {
        None => ControllerRolloutFlagParse {
            enabled: true,
            warn_enabled: false,
        },
        Some(value) if value == "0" || value.eq_ignore_ascii_case("false") => {
            ControllerRolloutFlagParse {
                enabled: false,
                warn_enabled: false,
            }
        }
        Some(value) if value == "1" || value.eq_ignore_ascii_case("true") => {
            ControllerRolloutFlagParse {
                enabled: true,
                warn_enabled: false,
            }
        }
        Some(_) => ControllerRolloutFlagParse {
            enabled: true,
            warn_enabled: true,
        },
    }
}

fn warn_invalid_enabled_once(var: &str, raw: &str) {
    static WARNED_FLAGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

    let mut warned_flags = WARNED_FLAGS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if warned_flags.insert(var.to_string()) {
        tracing::warn!(
            flag = %var,
            raw_value = %raw,
            "{}={:?} treating as enabled; use 0 or false to opt out",
            var,
            raw
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_rollout_flags_default_on_when_unset() {
        assert!(enabled_from_raw(None));
        assert_eq!(
            parse_raw(None),
            ControllerRolloutFlagParse {
                enabled: true,
                warn_enabled: false,
            }
        );
    }

    #[test]
    fn controller_rollout_flags_reject_documented_optout_values() {
        for raw in ["0", "false", "FALSE", " false "] {
            assert!(!enabled_from_raw(Some(raw)), "{raw:?} should opt out");
            assert_eq!(
                parse_raw(Some(raw)),
                ControllerRolloutFlagParse {
                    enabled: false,
                    warn_enabled: false,
                },
                "{raw:?} should opt out without warning"
            );
        }
    }

    #[test]
    fn controller_rollout_flags_keep_recognized_enable_values_enabled() {
        for raw in ["1", "true", "TRUE", " true "] {
            assert!(enabled_from_raw(Some(raw)), "{raw:?} should enable");
            assert_eq!(
                parse_raw(Some(raw)),
                ControllerRolloutFlagParse {
                    enabled: true,
                    warn_enabled: false,
                },
                "{raw:?} should enable without warning"
            );
        }
    }

    #[test]
    fn controller_rollout_flags_warn_set_still_enables() {
        for raw in ["", " ", "off", "no", "yes", "on", "garbage"] {
            assert!(enabled_from_raw(Some(raw)), "{raw:?} should enable");
            assert_eq!(
                parse_raw(Some(raw)),
                ControllerRolloutFlagParse {
                    enabled: true,
                    warn_enabled: true,
                },
                "{raw:?} should enable with an operator warning"
            );
        }
    }
}
