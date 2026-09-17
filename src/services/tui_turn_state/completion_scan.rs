use super::*;

/// Widened re-scan ceiling for when the default 64KB tail lacks a terminator
/// (#3030); bounds the one-time widen so hot call sites never read a multi-MB file.
const TURN_STATE_MAX_TAIL_BYTES: u64 = 1024 * 1024;

/// Strict, relay-offset-independent "is the last turn fully over?" probe used
/// by `jsonl_ready_for_input`'s `offset_behind` path — without it, a written
/// terminator followed by trailing housekeeping reads as Busy forever
/// (#2790, #3030); see [`scan_strict_terminator`] for the walk-back.
pub(crate) fn jsonl_strict_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    scan_strict_terminator_idle_with_strictness(
        provider,
        path,
        TerminatorStrictness::DrainReadiness,
    )
}

/// Stricter turn-END-only sibling of [`jsonl_strict_terminator_idle`], used
/// only by the finalize `Done` decision (#3016 S3). Unlike the lenient probe,
/// this accepts only the authoritative TURN-END terminator (see
/// [`envelope_is_turn_end_terminator`]) — a completed `agent_message` (Codex
/// can write one mid-turn) is walked past, not treated as Busy.
pub(crate) fn jsonl_turn_end_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    jsonl_completion_scan_idle(provider, path)
}

/// Finalizer/gate authority entry point: only authoritative terminators prove completion.
pub(crate) fn jsonl_completion_scan_idle(provider: &ProviderKind, path: &Path) -> bool {
    scan_strict_terminator_idle_with_strictness(
        provider,
        path,
        TerminatorStrictness::FinalizeAuthority,
    )
}

/// Shared windowed reverse-scan driver for the lenient and turn-END-only
/// probes; only the `strictness` argument differs.
fn scan_strict_terminator_idle_with_strictness(
    provider: &ProviderKind,
    path: &Path,
    strictness: TerminatorStrictness,
) -> bool {
    let Ok(window) = read_recent_jsonl_window(path, TURN_STATE_TAIL_BYTES) else {
        // A read error cannot prove the turn has ended → conservative Busy.
        return false;
    };
    match scan_strict_terminator(provider, &window.lines, strictness) {
        StrictTerminatorScan::Idle => return true,
        StrictTerminatorScan::Busy => return false,
        // Inconclusive over the whole file stays Busy; otherwise widen once (#3030).
        StrictTerminatorScan::Inconclusive => {
            if window.window_covers_file {
                return false;
            }
        }
    }

    let Ok(wide) = read_recent_jsonl_window(path, TURN_STATE_MAX_TAIL_BYTES) else {
        return false;
    };
    match scan_strict_terminator(provider, &wide.lines, strictness) {
        StrictTerminatorScan::Idle => true,
        // Still no terminator at the 1MB ceiling: stay Busy, not idle-by-default.
        StrictTerminatorScan::Busy | StrictTerminatorScan::Inconclusive => false,
    }
}

/// How permissive the reverse scan is about what counts as an Idle verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminatorStrictness {
    /// Whole "Idle-class" family proves at-rest. Used by [`jsonl_strict_terminator_idle`].
    DrainReadiness,
    /// Only the authoritative TURN terminator counts. Used by [`jsonl_turn_end_terminator_idle`].
    FinalizeAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictTerminatorScan {
    /// A definitive terminator proves the turn is over → Ready.
    Idle,
    /// A definitive in-flight signal proves the turn is not over → Busy.
    Busy,
    /// Only housekeeping/unknown envelopes; caller decides whether to widen or stay Busy.
    Inconclusive,
}

/// Reverse-scan for the most recent definitive turn-state envelope. Never
/// reports `Idle` on plausible in-flight evidence — false-idle is worse (#3030).
fn scan_strict_terminator(
    provider: &ProviderKind,
    lines: &[String],
    strictness: TerminatorStrictness,
) -> StrictTerminatorScan {
    let mut allow_torn_trailing_skip = true;
    for (rev_index, line) in lines.iter().rev().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let json = match serde_json::from_str::<Value>(trimmed) {
            Ok(json) => json,
            Err(_) => {
                // A torn trailing write shouldn't pin Busy forever: skip at most
                // ONE such line, only when it's the last non-empty line, looks
                // truncated, and its `type` is a *known* housekeeping marker —
                // not positively proven housekeeping stays Busy (#3030).
                if allow_torn_trailing_skip
                    && rev_index == 0
                    && is_torn_trailing_fragment(trimmed)
                    && partial_is_skippable_housekeeping(provider, trimmed)
                {
                    allow_torn_trailing_skip = false;
                    continue;
                }
                // Any other partial or a second unparseable line can't prove the turn ended.
                return StrictTerminatorScan::Busy;
            }
        };
        // A complete line consumes the budget: a later unparseable line is genuine corruption.
        allow_torn_trailing_skip = false;
        let classified = provider_envelope_turn_state(provider, &json);
        match classified {
            Some(TuiTurnState::Idle) => match strictness {
                // Lenient: any Idle-class envelope proves at-rest.
                TerminatorStrictness::DrainReadiness => return StrictTerminatorScan::Idle,
                // Turn-END-only (#3016 S3): a non-terminator Idle marker is walked past.
                TerminatorStrictness::FinalizeAuthority => {
                    if envelope_is_turn_end_terminator(provider, &json) {
                        return StrictTerminatorScan::Idle;
                    }
                    continue;
                }
            },
            Some(TuiTurnState::Streaming | TuiTurnState::UserSubmitted) => {
                return StrictTerminatorScan::Busy;
            }
            // Housekeeping/unrecognized envelopes are walked past, never idle alone (#3030).
            Some(TuiTurnState::Unknown) | None => continue,
        }
    }
    StrictTerminatorScan::Inconclusive
}

fn provider_envelope_turn_state(provider: &ProviderKind, json: &Value) -> Option<TuiTurnState> {
    match provider {
        ProviderKind::Claude => claude_envelope_turn_state(json),
        ProviderKind::Codex => codex_envelope_turn_state(json),
        _ => None,
    }
}

/// Is this envelope the AUTHORITATIVE per-provider TURN-END terminator
/// (#3016 S3)? Narrower than the "Idle-class" the lenient scan trusts: Codex
/// ONLY `turn.completed` (excludes a mid-turn `agent_message`); Claude ONLY
/// `result` and `system{turn_duration | stop_hook_summary}`. `false` means
/// "Idle-class but not a boundary → keep scanning back".
pub(super) fn envelope_is_turn_end_terminator(provider: &ProviderKind, json: &Value) -> bool {
    let Some(type_str) = json.get("type").and_then(Value::as_str) else {
        return false;
    };
    match provider {
        ProviderKind::Codex => type_str == "turn.completed",
        ProviderKind::Claude => match type_str {
            "result" => true,
            // #3221: an interrupt marker is a genuine turn boundary too.
            "user" => claude_user_envelope_is_interrupt_marker(json),
            "system" => matches!(
                json.get("subtype").and_then(Value::as_str),
                Some("turn_duration" | "stop_hook_summary")
            ),
            _ => false,
        },
        _ => false,
    }
}

/// Cheap "was this a torn write?" check: starts as an object but doesn't end in `}`.
pub(super) fn is_torn_trailing_fragment(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    bytes.first() == Some(&b'{') && bytes.last() != Some(&b'}')
}

/// A torn trailing partial is safe to skip only when its top-level `type`
/// affirmatively classifies as a known mode/permission marker; reuses the
/// standard observer's field-fragment parser to recognize it pre-flush.
fn partial_is_skippable_housekeeping(provider: &ProviderKind, trimmed: &str) -> bool {
    if !trimmed.trim_start().starts_with('{') {
        return false;
    }
    let Some(type_value) = top_level_string_field_fragment(trimmed, "type") else {
        // Unrecoverable `type` could be the start of a new active envelope: do not skip.
        return false;
    };
    match provider {
        ProviderKind::Claude => is_interactive_mode_housekeeping_type(&type_value),
        // Codex has no equivalent skippable family; stay conservative.
        _ => false,
    }
}

/// Heuristic shape match for `/model` / `/compact` housekeeping, never a
/// turn-state signal. Used only by the torn-write skip above; deliberately
/// narrow so it never matches a real turn-state envelope.
pub(super) fn is_interactive_mode_housekeeping_type(type_str: &str) -> bool {
    type_str == "mode"
        || type_str.ends_with("-mode")
        || type_str.ends_with("_mode")
        || type_str.contains("permission")
}
