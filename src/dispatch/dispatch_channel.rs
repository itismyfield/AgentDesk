use crate::services::provider::ProviderKind;

pub(super) fn dispatch_uses_alt_channel(dispatch_type: &str) -> bool {
    matches!(dispatch_type, "review" | "e2e-test" | "consultation")
}

/// #3605 (T2): canonical set of "inert side-path" dispatch types — dispatches
/// that record information about a card without ever advancing, completing, or
/// failing it. They attach to the card (becoming `latest_dispatch_id`) but stay
/// pinned in `requested`: they must never kick off the implementation lifecycle
/// (see [`dispatch_type_skips_kickoff`]) and, crucially, their terminal
/// completion must NOT finalize the bound auto_queue entry as `done` (otherwise
/// a card would close with no implementation dispatch — see
/// `dispatch_status::should_skip_auto_queue_terminal_sync`).
///
/// `consultation` (#256) was the first such type; `scope-assessment` (#3605) is
/// the second. Centralizing the set here means every side-path guard mirrors the
/// two consistently and future side-path types are added in exactly one place.
pub(crate) const SIDE_PATH_DISPATCH_TYPES: &[&str] = &["consultation", "scope-assessment"];

/// Whether `dispatch_type` is an inert side-path (see
/// [`SIDE_PATH_DISPATCH_TYPES`]). Side-paths are owned by the policy layer
/// (kanban-rules onDispatchCompleted) and must be treated as non-implementation,
/// non-card-advancing work everywhere a guard distinguishes them.
pub(crate) fn dispatch_is_side_path(dispatch_type: Option<&str>) -> bool {
    dispatch_type.is_some_and(|t| SIDE_PATH_DISPATCH_TYPES.contains(&t))
}

/// Whether attaching `dispatch_type` to a card must NOT transition the card into
/// its kickoff (in_progress) state. This is the broader set: review-family
/// dispatches stay in their own review lifecycle, and inert side-paths
/// ([`dispatch_is_side_path`]) stay pinned in `requested`. Used by both
/// `transition::decide_dispatch_attached` (skip_kickoff) and
/// `dispatch_create` (kickoff_state gate) so the two layers cannot drift.
pub(crate) fn dispatch_type_skips_kickoff(dispatch_type: &str) -> bool {
    matches!(dispatch_type, "review" | "review-decision" | "rework")
        || dispatch_is_side_path(Some(dispatch_type))
}

pub(super) fn resolve_dispatch_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let _ = channel_id;
    false
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    let _ = channel_name;
    false
}

pub fn drain_unified_thread_kill_signals() -> Vec<String> {
    Vec::new()
}

/// Determine provider from a Discord channel name suffix.
pub(super) fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    ProviderKind::from_channel_suffix(channel).and_then(|provider| match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::OpenCode => Some("opencode"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Unsupported(_) => None,
    })
}

pub(crate) fn dispatch_destination_provider_override(
    dispatch_type: Option<&str>,
    context_json: Option<&str>,
) -> Option<String> {
    let key = match dispatch_type {
        Some("review") => "target_provider",
        Some("review-decision") => "from_provider",
        _ => return None,
    };
    let context =
        context_json.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
    context
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod side_path_predicate_tests {
    use super::*;

    #[test]
    fn side_path_set_is_exactly_consultation_and_scope_assessment() {
        // #3605 (T2): lock the canonical side-path set. consultation (#256) and
        // scope-assessment (#3605) are the two inert side-paths. If a new
        // side-path type is added it should be added HERE (one place) and this
        // test updated — every guard reads through dispatch_is_side_path.
        assert_eq!(
            SIDE_PATH_DISPATCH_TYPES,
            ["consultation", "scope-assessment"]
        );
        assert!(dispatch_is_side_path(Some("consultation")));
        assert!(dispatch_is_side_path(Some("scope-assessment")));
        assert!(!dispatch_is_side_path(Some("implementation")));
        assert!(!dispatch_is_side_path(Some("review")));
        assert!(!dispatch_is_side_path(Some("rework")));
        assert!(!dispatch_is_side_path(None));
    }

    #[test]
    fn skip_kickoff_covers_review_family_and_side_paths() {
        // Review-family dispatches and inert side-paths must not kick a card to
        // its in_progress state. This is the union used by transition.rs,
        // dispatch_create, and phase_gate.
        for t in [
            "review",
            "review-decision",
            "rework",
            "consultation",
            "scope-assessment",
        ] {
            assert!(dispatch_type_skips_kickoff(t), "{t} must skip kickoff");
        }
        for t in ["implementation", "e2e-test", "pm-decision"] {
            assert!(!dispatch_type_skips_kickoff(t), "{t} must NOT skip kickoff");
        }
    }

    #[test]
    fn side_path_is_a_strict_subset_of_skip_kickoff() {
        // Every side-path must also skip kickoff (it stays in `requested`), but
        // not every skip-kickoff type is a side-path (review-family is not).
        for t in SIDE_PATH_DISPATCH_TYPES {
            assert!(
                dispatch_type_skips_kickoff(t),
                "side-path {t} must be within the skip-kickoff set"
            );
        }
        assert!(dispatch_type_skips_kickoff("review"));
        assert!(!dispatch_is_side_path(Some("review")));
    }
}
