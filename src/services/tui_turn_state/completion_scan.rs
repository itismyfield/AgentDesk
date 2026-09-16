use super::*;

/// Upper bound for the strict-terminator re-scan's widened window, used when
/// the default 64KB tail lacks a turn-state envelope. A post-terminator
/// housekeeping burst (`/model`, `/compact`, attachments) can push the real
/// terminator past 64KB and stick the idle-queue on `Busy` forever (#3030);
/// this bounds the one-time widen so hot call sites never read a multi-MB file.
const TURN_STATE_MAX_TAIL_BYTES: u64 = 1024 * 1024;

/// Strict, relay-offset-independent "is the last turn fully over?" probe,
/// used by `jsonl_ready_for_input` on the `offset_behind` path (relay has not
/// consumed the whole transcript). Without this, a fully written terminator
/// followed by trailing post-turn housekeeping (`pr-link`, `ai-title`, mode
/// envelopes, …) reads as Busy forever (#2790, #3030) — see
/// [`scan_strict_terminator`] for the walk-back algorithm that finds the real
/// terminator beneath such trailing lines. On this path the caller has no
/// active turn, so a skipped trailing `permission-mode` is `/model` metadata,
/// never a turn spin-up; the watcher's completion gate has its own
/// `full_response`-non-empty guard against tearing down a spinning-up turn
/// (#2712).
pub(crate) fn jsonl_strict_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    scan_strict_terminator_idle_with_strictness(
        provider,
        path,
        TerminatorStrictness::DrainReadiness,
    )
}

/// The STRICTER turn-END-only sibling of [`jsonl_strict_terminator_idle`],
/// used ONLY by the finalize `Done` decision
/// (`TurnFinalizer::completion_signal_state`, #3016 S3 Concern 1).
///
/// Where the lenient probe treats the whole per-provider "Idle-class" family
/// as at-rest — including a *completed* `agent_message`, which for Codex can
/// be written mid-turn right before a tool call — this one accepts only the
/// authoritative TURN-END terminator; see
/// [`TerminatorStrictness::FinalizeAuthority`] and
/// [`envelope_is_turn_end_terminator`] for exactly which envelopes qualify.
/// Everything else is walked past rather than treated as Busy, same as the
/// lenient scan; only which envelopes may *produce* an Idle verdict differs.
pub(crate) fn jsonl_turn_end_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    jsonl_completion_scan_idle(provider, path)
}

/// Shared completion-signal scan entry point for the finalizer/gate authority:
/// only authoritative per-provider turn terminators prove completion.
pub(crate) fn jsonl_completion_scan_idle(provider: &ProviderKind, path: &Path) -> bool {
    scan_strict_terminator_idle_with_strictness(
        provider,
        path,
        TerminatorStrictness::FinalizeAuthority,
    )
}

/// Shared windowed reverse-scan driver for both the lenient
/// ([`jsonl_strict_terminator_idle`]) and the turn-END-only
/// ([`jsonl_turn_end_terminator_idle`]) probes. Only the `strictness` argument
/// differs — the windowing, widen-once, and torn-write handling are identical.
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
        // No definitive envelope in this window. If it already covers the whole
        // file, stay Busy; otherwise the terminator may have scrolled out behind
        // a housekeeping burst (#3030) — widen once, bounded.
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
        // Still no terminator within the 1MB ceiling: stay Busy rather than
        // assume idle on an ambiguous, unbounded transcript.
        StrictTerminatorScan::Busy | StrictTerminatorScan::Inconclusive => false,
    }
}

/// How permissive the reverse scan is about what counts as an Idle verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminatorStrictness {
    /// The whole provider "Idle-class" family proves at-rest (the idle-queue
    /// drain's readiness question). Used by [`jsonl_strict_terminator_idle`].
    DrainReadiness,
    /// ONLY the authoritative per-provider TURN terminator proves the turn
    /// ENDED. Every other envelope (including the lenient Idle-class markers) is
    /// walked past. Used by [`jsonl_turn_end_terminator_idle`] for the finalize
    /// `Done` decision (#3016 S3, Concern 1).
    FinalizeAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictTerminatorScan {
    /// A definitive terminator proves the turn is over → Ready.
    Idle,
    /// A definitive in-flight signal (streaming/user, an active-looking partial,
    /// or a non-torn unparseable line) proves the turn is not over → Busy.
    Busy,
    /// The window held only housekeeping/unknown envelopes and ran out without a
    /// verdict. The caller decides whether to widen the window or stay Busy.
    Inconclusive,
}

/// Reverse-scan the tail window for the most recent *definitive* turn-state
/// envelope. Conservatism rule (#3030): never report `Idle` while there is any
/// plausible evidence of an in-flight turn — false-idle (input injected
/// mid-turn) is strictly worse than false-busy.
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
                // A single torn *trailing* write (writer mid-flush) should not
                // pin the session Busy forever. Skip at most ONE such line,
                // and only when it is: the very last non-empty line, looks
                // truncated (no trailing `}`), and its recoverable top-level
                // `type` is a *known* housekeeping marker (not active, not a
                // trusted partial terminator, not an unrecoverable fragment
                // like `{"ty`). Anything not positively proven housekeeping
                // stays Busy — false-busy is recoverable, false-idle injects
                // input mid-turn (#3030).
                if allow_torn_trailing_skip
                    && rev_index == 0
                    && is_torn_trailing_fragment(trimmed)
                    && partial_is_skippable_housekeeping(provider, trimmed)
                {
                    allow_torn_trailing_skip = false;
                    continue;
                }
                // An active-looking partial, an unidentifiable partial, an
                // interior partial, or a second unparseable line — none of these
                // can prove the turn has ended.
                return StrictTerminatorScan::Busy;
            }
        };
        // Any complete line consumes the one-shot torn-trailing budget: a torn
        // write can only ever be the trailing line, so once we have seen a
        // complete line, a later (older) unparseable line is genuine corruption.
        allow_torn_trailing_skip = false;
        let classified = provider_envelope_turn_state(provider, &json);
        match classified {
            Some(TuiTurnState::Idle) => match strictness {
                // Lenient: any Idle-class envelope proves at-rest.
                TerminatorStrictness::DrainReadiness => return StrictTerminatorScan::Idle,
                // Turn-END-only (#3016 S3): an Idle-class envelope ends the
                // turn only when it is the authoritative terminator — a
                // non-terminator Idle-class marker (e.g. a completed
                // `agent_message` right before a tool call) is walked PAST to
                // the real terminator beneath, same as trailing housekeeping.
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
            // Skip housekeeping (`permission-mode` → Unknown) and unrecognized
            // envelopes (None); never idle on their own (#3030) — only walked
            // past to reveal the real terminator beneath.
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

/// Is this fully-parsed envelope the AUTHORITATIVE per-provider TURN-END
/// terminator (#3016 S3), as opposed to a merely "Idle-class" at-rest marker
/// the lenient scan also trusts? Intentionally the NARROW subset:
///   - Codex: ONLY `turn.completed` — excludes `session_meta`/`thread.started`,
///     `task_complete`, and a completed `agent_message` (can be mid-turn);
///   - Claude: ONLY `result` and `system{turn_duration | stop_hook_summary}` —
///     excludes `system{init}` (session-start, never a turn end).
///
/// Callers guarantee `json` already classified `TuiTurnState::Idle`, so
/// `false` means "Idle-class but not a boundary → keep scanning back".
pub(super) fn envelope_is_turn_end_terminator(provider: &ProviderKind, json: &Value) -> bool {
    let Some(type_str) = json.get("type").and_then(Value::as_str) else {
        return false;
    };
    match provider {
        ProviderKind::Codex => type_str == "turn.completed",
        ProviderKind::Claude => match type_str {
            "result" => true,
            // #3221: the `[Request interrupted by user]` marker is a genuine
            // turn boundary (the turn was aborted), so the turn-END-only scan
            // used by the finalize `Done` decision must treat it as a
            // terminator too — keeping the strict scan consistent with the
            // standard observer's Idle classification of the same envelope.
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

/// A truncated trailing JSON fragment looks like the writer was interrupted
/// mid-flush: it starts as an object but the final non-whitespace byte is not a
/// closing `}`. A complete JSON object always ends in `}`, so this is a cheap,
/// conservative "was this a torn write?" check.
pub(super) fn is_torn_trailing_fragment(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    bytes.first() == Some(&b'{') && bytes.last() != Some(&b'}')
}

/// Positive identification that a torn trailing partial is recognized
/// post-turn housekeeping and therefore safe to skip. The conservative
/// inverse of an "is it active?" check: skip only when the partial's
/// top-level `type` affirmatively classifies as a known mode/permission
/// marker; an unrecoverable, active, or partial-terminator type all return
/// `false`. Reuses the same top-level field-fragment parser the standard
/// observer uses, so a partial `{"type":"permission-mode"...` is recognized
/// before the line is fully flushed.
fn partial_is_skippable_housekeeping(provider: &ProviderKind, trimmed: &str) -> bool {
    if !trimmed.trim_start().starts_with('{') {
        return false;
    }
    let Some(type_value) = top_level_string_field_fragment(trimmed, "type") else {
        // Could not even recover the top-level `type` — could be the start of a
        // new active envelope. Do not skip.
        return false;
    };
    match provider {
        // Claude: only the structurally-recognized mode/permission housekeeping
        // family is skippable. `user`/`assistant`/`result`/`system` are not.
        ProviderKind::Claude => is_interactive_mode_housekeeping_type(&type_value),
        // Codex has no equivalent post-turn housekeeping envelope family that is
        // safe to skip on a torn trailing line; stay conservative and never skip.
        _ => false,
    }
}

/// Heuristic shape match for `/model` / `/compact` mode-change housekeeping
/// envelopes (`permission-mode`, `mode`, future `model-mode`/`permission_mode`
/// renames) — never turn-state signals. Used ONLY by
/// `partial_is_skippable_housekeeping`'s torn-write skip; deliberately NOT
/// wired into `claude_envelope_turn_state`, where mapping the whole family to
/// `Unknown` would break the standard observer's walk-back across a completed
/// turn's trailing `mode` housekeeping (see the note there). Deliberately
/// narrow — must not match any envelope that could be a real turn-state signal.
pub(super) fn is_interactive_mode_housekeeping_type(type_str: &str) -> bool {
    type_str == "mode"
        || type_str.ends_with("-mode")
        || type_str.ends_with("_mode")
        || type_str.contains("permission")
}
