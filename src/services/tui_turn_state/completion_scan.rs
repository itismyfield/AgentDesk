use super::*;

/// Bounded upper limit for the strict-terminator re-scan when the default
/// 64KB tail window does not contain a turn-state envelope but the transcript
/// is larger than the window. Post-terminator housekeeping bursts (`/model`,
/// `/compact`, attachment metadata, …) can exceed 64KB and push the real
/// terminator out of the default window, which left the idle-queue stuck on
/// `Busy` forever (#3030). We widen the window once, up to this ceiling, so a
/// terminator that merely scrolled out of the small tail is still found —
/// while keeping the read bounded so the 9+ hot call sites never read a whole
/// multi-megabyte transcript on every probe.
const TURN_STATE_MAX_TAIL_BYTES: u64 = 1024 * 1024;

/// Completion-signal scan acceptance mode.
///
/// `FinalizeAuthority` is the strict finalizer authority: only real
/// per-provider turn terminators prove `Done`. `GateQuiescence` is the S2-a
/// compatibility mode for the watcher completion gate: it intentionally
/// preserves the pre-S2-a observer acceptance set (64KB tail, Claude malformed
/// fallback, provider Idle-class envelopes, source-agnostic provider JSONL).
/// S2-b should delete `GateQuiescence` together with the gate's `TimedOut`
/// completion suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonlCompletionScanMode {
    FinalizeAuthority,
    GateQuiescence,
}

/// Strict, relay-offset-independent "is the last turn fully over?" probe.
///
/// `jsonl_ready_for_input` calls this on the `offset_behind` path (the relay
/// has not consumed the whole transcript). #2790 introduced it so a fully
/// written terminator envelope reports Ready even though trailing bytes are
/// still unconsumed — otherwise the idle-queue drain loops forever
/// (`hosted TUI structured turn state is busy` every 2s).
///
/// #2790 only inspected the single latest line. Claude writes post-turn
/// housekeeping envelopes *after* the terminator — `pr-link`, `ai-title`,
/// `last-prompt`, `mode`, `attachment`, plus a `permission-mode` envelope
/// emitted whenever the user opens an interactive `/model` / `/compact` view
/// and returns to the prompt. With those trailing lines the latest line is
/// no longer the terminator, so the probe wrongly reported Busy and the
/// queued message was never drained — the recurring "no active turn yet the
/// queue is stuck" bug (observed 9×; see the watcher test note in
/// `tmux_watcher.rs`).
///
/// We now walk backward across those non-turn-state housekeeping lines to
/// find the most recent *definitive* turn-state envelope:
///   - a terminator (`result` / `system{turn_duration,stop_hook_summary,init}`
///     / Codex `turn.completed`) proves the turn is over → idle.
///   - a `user`/`assistant` envelope proves a turn is in flight → not idle.
///   - a partial/unparseable trailing fragment cannot prove anything (a new
///     turn may be mid-write), so we stop and report not-idle — preserving
///     #2790's race guard against dispatching onto a just-restarted turn.
///   - `permission-mode` (Unknown) and unrecognized housekeeping envelopes
///     (None) are skipped; on this offset-behind readiness path the caller
///     has no active turn, so a trailing `permission-mode` is `/model`
///     metadata, not a turn spin-up. (The watcher's completion gate has its
///     own `full_response`-non-empty guard, so this skip cannot tear down a
///     spinning-up turn — see #2712.)
pub(crate) fn jsonl_strict_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    scan_strict_terminator_idle_with_strictness(
        provider,
        path,
        TerminatorStrictness::DrainReadiness,
    )
}

/// #3016 S3 (Concern 1): the STRICTER turn-END-only sibling of
/// [`jsonl_strict_terminator_idle`], used ONLY by the finalize `Done` decision
/// (`TurnFinalizer::completion_signal_state`).
///
/// The lenient probe above accepts the whole "Idle-class" envelope family as
/// proof the session is at rest — for Codex that includes `session_meta`,
/// `thread.started`, `event_msg{task_complete}`, AND a *completed*
/// `agent_message` (`item.completed`). That leniency is correct for the
/// idle-queue *drain* (it asks "is the session ready to accept input?"), but it
/// is WRONG as a turn-END terminator: a completed `agent_message` written
/// immediately BEFORE a tool call is mid-turn — the turn is still LIVE — yet the
/// lenient scan reads it as Idle. Finalizing on that signal over-finalizes a
/// live turn.
///
/// This probe accepts as Idle ONLY the authoritative per-provider TURN
/// terminator:
///   - Codex: ONLY `type == "turn.completed"` (NOT `task_complete`, NOT a
///     completed `agent_message`, NOT `session_meta`/`thread.started`).
///   - Claude: ONLY the real turn terminator — `type == "result"` or the
///     `system{turn_duration | stop_hook_summary}` turn-end envelope. NOT
///     `system{init}` (a SESSION-start marker, never a turn-end), NOT any
///     housekeeping/mode envelope.
///
/// Everything else (the lenient Idle-class markers, housekeeping, unknown
/// metadata) is treated as "keep looking" so the reverse scan walks back to the
/// real terminator beneath trailing housekeeping — while streaming/user
/// envelopes and torn non-housekeeping fragments still report Busy. The
/// torn-trailing skip and the housekeeping-walk-back are preserved verbatim; the
/// ONLY behavioural change versus the lenient scan is which envelopes are
/// allowed to *produce* an Idle verdict.
pub(crate) fn jsonl_turn_end_terminator_idle(provider: &ProviderKind, path: &Path) -> bool {
    jsonl_completion_scan_idle(provider, path, JsonlCompletionScanMode::FinalizeAuthority)
}

/// Shared completion-signal scan entry point for the S2-a finalizer/gate split.
/// The scanner is common; only the explicit acceptance mode differs.
pub(crate) fn jsonl_completion_scan_idle(
    provider: &ProviderKind,
    path: &Path,
    mode: JsonlCompletionScanMode,
) -> bool {
    let strictness = match mode {
        JsonlCompletionScanMode::FinalizeAuthority => TerminatorStrictness::FinalizeAuthority,
        JsonlCompletionScanMode::GateQuiescence => TerminatorStrictness::GateQuiescence,
    };
    scan_strict_terminator_idle_with_strictness(provider, path, strictness)
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
    // First pass over the default 64KB tail window.
    let Ok(window) = read_recent_jsonl_window(path, TURN_STATE_TAIL_BYTES) else {
        // A read error cannot prove the turn has ended → conservative Busy.
        return false;
    };
    match scan_strict_terminator(provider, &window.lines, strictness) {
        StrictTerminatorScan::Idle => return true,
        StrictTerminatorScan::Busy => return false,
        // GateQuiescence deliberately reproduces the pre-S2-a gate: one 64KB
        // observer-style tail only. Do not widen past the old acceptance
        // window here.
        StrictTerminatorScan::Inconclusive
            if strictness == TerminatorStrictness::GateQuiescence =>
        {
            return false;
        }
        // The window contained no definitive turn-state envelope. If it already
        // covered the whole file there is nothing more to read → conservative
        // Busy. Otherwise the real terminator may have scrolled out of the
        // small tail window behind a post-terminator housekeeping burst
        // (`/model`, `/compact`, attachments — #3030); widen once, bounded.
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
        // Even in the widened window we found no terminator (or the terminator
        // is still older than the 1MB ceiling): stay conservatively Busy rather
        // than assume idle on an ambiguous, unbounded transcript.
        StrictTerminatorScan::Busy | StrictTerminatorScan::Inconclusive => false,
    }
}

/// How permissive the reverse scan is about what counts as an Idle verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminatorStrictness {
    /// The whole provider "Idle-class" family proves at-rest (the idle-queue
    /// drain's readiness question). Used by [`jsonl_strict_terminator_idle`].
    DrainReadiness,
    /// The pre-S2-a completion-gate acceptance set: provider Idle-class
    /// envelopes prove quiescence, Claude malformed/partial tails can fall back
    /// to a prior result, `Unknown` stops the scan, and only the default 64KB
    /// window is inspected.
    GateQuiescence,
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
                if strictness == TerminatorStrictness::GateQuiescence {
                    if let Some(state) = provider_partial_turn_state(provider, trimmed) {
                        return match state {
                            TuiTurnState::Idle => StrictTerminatorScan::Idle,
                            TuiTurnState::Streaming
                            | TuiTurnState::UserSubmitted
                            | TuiTurnState::Unknown => StrictTerminatorScan::Busy,
                        };
                    }
                    if provider_malformed_jsonl_line_policy(provider)
                        == MalformedJsonlLinePolicy::FallbackToPrevious
                    {
                        continue;
                    }
                    return StrictTerminatorScan::Busy;
                }
                // A single torn *trailing* write (the writer was mid-flush when
                // we read) should not pin the session Busy forever. We skip at
                // most ONE such line, and ONLY when we can *positively* identify
                // it as recognized post-turn housekeeping (e.g. a partial
                // `permission-mode` / `mode` envelope). Requirements:
                //   - it is the very last non-empty line (the only place a torn
                //     write can legitimately appear), and
                //   - it looks truncated (does not end in `}`), and
                //   - its recoverable top-level `type` is a *known* housekeeping
                //     marker — NOT active (`user`/`assistant`/streaming), NOT a
                //     terminator we would trust from a partial, and NOT an
                //     unrecoverable/too-short fragment (e.g. `{"ty`, which could
                //     be the start of a new `user` envelope).
                // Anything we cannot positively prove is housekeeping keeps the
                // session Busy — false-busy here is recoverable; false-idle
                // injects input mid-turn (#3030).
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
                TerminatorStrictness::DrainReadiness | TerminatorStrictness::GateQuiescence => {
                    return StrictTerminatorScan::Idle;
                }
                // Turn-END-only (#3016 S3, Concern 1): an Idle-class envelope
                // proves the TURN ended ONLY when it is the authoritative
                // per-provider turn terminator. A non-terminator Idle-class
                // marker (Codex `session_meta`/`thread.started`/`task_complete`/
                // completed `agent_message`; Claude `system{init}`) is NOT a
                // turn boundary — a completed `agent_message` right before a tool
                // call is mid-turn — so walk PAST it to the real terminator
                // beneath, exactly like trailing housekeeping. It can never
                // *create* a Done verdict on its own.
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
            // Skip post-turn housekeeping (`permission-mode` → Unknown) and
            // unrecognized metadata envelopes (None); keep looking for the
            // real terminator. Unknown/None NEVER count as idle (#3030): a
            // renamed housekeeping envelope must not be able to *create* an
            // idle verdict — it can only be skipped over to reveal the real,
            // structurally-recognized terminator beneath it.
            Some(TuiTurnState::Unknown) if strictness == TerminatorStrictness::GateQuiescence => {
                return StrictTerminatorScan::Busy;
            }
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

fn provider_partial_turn_state(provider: &ProviderKind, line: &str) -> Option<TuiTurnState> {
    match provider {
        ProviderKind::Claude => claude_partial_turn_state(line),
        ProviderKind::Codex => None,
        _ => None,
    }
}

fn provider_malformed_jsonl_line_policy(provider: &ProviderKind) -> MalformedJsonlLinePolicy {
    match provider {
        ProviderKind::Claude => MalformedJsonlLinePolicy::FallbackToPrevious,
        ProviderKind::Codex => MalformedJsonlLinePolicy::ReturnUnknown,
        _ => MalformedJsonlLinePolicy::ReturnUnknown,
    }
}

/// #3016 S3 (Concern 1): is this fully-parsed envelope the AUTHORITATIVE
/// per-provider TURN-END terminator (the genuine turn boundary), as opposed to a
/// merely "Idle-class" / at-rest marker that the lenient scan also trusts?
///
/// This is intentionally the NARROW subset of the Idle-class family:
///   - Codex: ONLY `turn.completed`. `session_meta`/`thread.started` (session
///     bring-up), `event_msg{task_complete}` (a task signal, not the turn
///     record), and a completed `agent_message` (`item.completed`, which can be
///     written mid-turn right before a tool call) are EXCLUDED.
///   - Claude: ONLY `result` and the `system{turn_duration | stop_hook_summary}`
///     turn-end envelopes. `system{init}` is a SESSION-start marker — never a
///     turn end — and is EXCLUDED.
///
/// Callers guarantee `json` already classified as `TuiTurnState::Idle` via the
/// per-provider classifier, so a `false` here means "Idle-class but not a turn
/// boundary → keep scanning back".
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

/// Positive identification that a torn trailing partial is *recognized post-turn
/// housekeeping* and therefore safe to skip over. This is the conservative
/// inverse of an "is it active?" check: we skip ONLY when we can affirmatively
/// classify the partial's top-level `type` as a known mode/permission marker.
/// An unrecoverable type (too short, e.g. `{"ty`), an active envelope, or a
/// partial terminator all return `false` → the caller stays Busy.
///
/// We reuse the same top-level field-fragment parser the standard observer uses
/// so a partial `{"type":"permission-mode"...` is recognized before its line is
/// fully flushed, without ever mistaking a partial `{"type":"user"...` for
/// housekeeping.
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

/// Heuristic shape match for the family of `/model` / `/compact` interactive-
/// view and mode-change housekeeping envelopes (`permission-mode`, `mode`, and
/// future renames like `model-mode` / `permission_mode`). These are never
/// turn-state signals.
///
/// Used ONLY by the strict offset-behind scan's torn-write skip
/// (`partial_is_skippable_housekeeping`) to positively identify a truncated
/// trailing line as safe-to-skip housekeeping. It is deliberately NOT wired into
/// `claude_envelope_turn_state`: there, mapping the whole family to `Unknown`
/// would stop the standard observer's walk-back across a completed turn's
/// trailing `mode` housekeeping and wrongly report not-ready (see the note in
/// `claude_envelope_turn_state`).
///
/// Deliberately narrow: matches only types that *are* a mode marker (`mode`),
/// end in a `-mode`/`_mode` suffix, or carry a `permission` token. It must not
/// match any envelope that could be a real turn-state signal.
pub(super) fn is_interactive_mode_housekeeping_type(type_str: &str) -> bool {
    type_str == "mode"
        || type_str.ends_with("-mode")
        || type_str.ends_with("_mode")
        || type_str.contains("permission")
}
