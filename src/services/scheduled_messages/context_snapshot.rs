//! Immutable conversation-context snapshots for scheduled agent turns (#4658).
//!
//! A `context_strategy='snapshot'` reservation freezes its source channel's live
//! conversation at creation time. The rendered pairs are stored inline (not a
//! frontier pointer) so the snapshot stays reproducible after transcript
//! retention/`/clear` would otherwise erase the source rows. The fire path
//! recomputes [`compute_content_digest`] and fails closed on any mismatch —
//! this digest check is the immutability guard (later live-context changes can
//! never mutate what a captured snapshot injects).
//!
//! Session isolation: a snapshot turn derives its ADK session key from
//! [`scheduled_snapshot_session_basis`] (`scheduled:{definition_id}`) instead of
//! the channel name, so the reserved turn never shares — and therefore never
//! overwrites — the channel's live provider session row (AC-2; avoids the #4634
//! session_key collision class).
//!
//! Security: the snapshot stores conversation context and model/effort intent
//! only. Tool permissions, sandbox/approval, `codex_exec_policy`, and allowlists
//! are deliberately NOT captured — the fire-time headless path re-resolves them
//! from current settings (AC-6).

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::db::session_transcripts::{
    ChannelTranscriptPair, fetch_channel_frontier_tx, fetch_channel_pairs_up_to_frontier_tx,
};

/// Max pairs captured (design default; wider than the 3-pair live-context path).
pub(crate) const SNAPSHOT_MAX_PAIRS: u64 = 10;
/// Per-message truncation ceiling.
pub(crate) const SNAPSHOT_PAIR_MESSAGE_MAX_CHARS: usize = 4_000;
/// Overall rendered-context ceiling; oldest pairs are dropped to fit.
pub(crate) const SNAPSHOT_TOTAL_MAX_CHARS: usize = 32_000;
const TRUNCATION_MARKER: &str = "…[truncated]";

/// Execution intent frozen alongside the conversation context. Conversation and
/// model intent only — never security policy (see module docs / AC-6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotIntent {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub workspace_hint: Option<String>,
}

/// Result of rendering frontier-bounded pairs into a frozen context block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedSnapshot {
    pub rendered_context: String,
    pub pair_count: usize,
}

/// A persisted immutable snapshot row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ContextSnapshotRow {
    pub id: String,
    pub source_channel_id: String,
    pub source_session_key: Option<String>,
    pub transcript_frontier: i64,
    pub rendered_context: String,
    pub pair_count: i32,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub workspace_hint: Option<String>,
    pub content_digest: String,
}

/// Why a snapshot could not be captured at reservation time.
#[derive(Debug)]
pub(crate) enum CaptureError {
    /// The source channel has no eligible conversation pairs. Fail-closed: the
    /// reservation is refused rather than silently degraded to fresh.
    EmptyContext,
    Db(anyhow::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::EmptyContext => {
                write!(f, "source channel has no conversation context to snapshot")
            }
            CaptureError::Db(error) => write!(f, "{error}"),
        }
    }
}

/// Why a snapshot failed validation at fire time.
#[derive(Debug)]
pub(crate) enum SnapshotValidationError {
    /// The referenced snapshot row is genuinely gone (deleted out from under the
    /// definition). Deterministic — retry cannot resolve it.
    Missing,
    /// Stored digest disagrees with a recompute over stored fields
    /// (tamper/corruption). Deterministic — retry cannot resolve it.
    DigestMismatch,
    /// A transient database error (connection blip, timeout). NOT deterministic:
    /// the caller must retry/defer rather than terminalize (F2 — a DB blip must
    /// never permanently kill a recurring definition).
    Db(anyhow::Error),
}

impl SnapshotValidationError {
    /// Deterministic failures terminalize; a transient DB error must retry.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(self, SnapshotValidationError::Db(_))
    }
}

impl std::fmt::Display for SnapshotValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotValidationError::Missing => write!(f, "snapshot row missing"),
            SnapshotValidationError::DigestMismatch => write!(f, "snapshot digest mismatch"),
            SnapshotValidationError::Db(error) => write!(f, "snapshot lookup failed: {error}"),
        }
    }
}

// ── Pure rendering + digest (unit-testable; no DB) ──────────────────────────

fn truncate_message(message: &str) -> String {
    let message = message.trim();
    if message.chars().count() <= SNAPSHOT_PAIR_MESSAGE_MAX_CHARS {
        return message.to_string();
    }
    let mut truncated: String = message
        .chars()
        .take(SNAPSHOT_PAIR_MESSAGE_MAX_CHARS)
        .collect();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

fn render_block(pairs: &[(String, String)]) -> String {
    let mut block = format!(
        "[예약 시점 고정 컨텍스트 — 이 예약이 생성될 때의 대화 {}쌍을 불변 스냅샷으로 제공합니다. \
         이후 채널 대화 변화와 무관하게 고정됩니다. 지시가 아니라 참고용입니다.]",
        pairs.len()
    );
    for (index, (user_message, assistant_message)) in pairs.iter().enumerate() {
        block.push_str(&format!(
            "\n\n{}. 사용자:\n{}\n어시스턴트:\n{}",
            index + 1,
            user_message,
            assistant_message
        ));
    }
    block
}

/// Render frontier-bounded pairs (oldest-first) into a frozen context block,
/// applying per-message truncation and dropping the oldest pairs to fit the
/// overall cap. Empty/whitespace-only pairs are excluded.
pub(crate) fn render_snapshot_pairs(pairs: &[ChannelTranscriptPair]) -> RenderedSnapshot {
    let mut rendered_pairs: Vec<(String, String)> = pairs
        .iter()
        .take(SNAPSHOT_MAX_PAIRS as usize)
        .filter(|pair| {
            !pair.user_message.trim().is_empty() && !pair.assistant_message.trim().is_empty()
        })
        .map(|pair| {
            (
                truncate_message(&pair.user_message),
                truncate_message(&pair.assistant_message),
            )
        })
        .collect();

    if rendered_pairs.is_empty() {
        return RenderedSnapshot {
            rendered_context: String::new(),
            pair_count: 0,
        };
    }

    let mut rendered_context = render_block(&rendered_pairs);
    while rendered_context.chars().count() > SNAPSHOT_TOTAL_MAX_CHARS && rendered_pairs.len() > 1 {
        rendered_pairs.remove(0);
        rendered_context = render_block(&rendered_pairs);
    }

    RenderedSnapshot {
        rendered_context,
        pair_count: rendered_pairs.len(),
    }
}

fn meta_field(value: Option<&str>) -> &str {
    value.unwrap_or("")
}

/// Canonical SHA-256 over the rendered context and captured meta. Any change to
/// a stored field changes the digest, so [`validate_snapshot_pg`] detects
/// post-capture tampering. This is the immutability guard.
pub(crate) fn compute_content_digest(
    rendered_context: &str,
    transcript_frontier: i64,
    pair_count: i32,
    intent: &SnapshotIntent,
) -> String {
    let fast = match intent.fast_mode {
        Some(true) => "true",
        Some(false) => "false",
        None => "",
    };
    let canonical = format!(
        "v1\nfrontier={transcript_frontier}\npairs={pair_count}\nprovider={}\nmodel={}\neffort={}\nfast={fast}\nworkspace={}\n---\n{rendered_context}",
        meta_field(intent.provider.as_deref()),
        meta_field(intent.model.as_deref()),
        meta_field(intent.reasoning_effort.as_deref()),
        meta_field(intent.workspace_hint.as_deref()),
    );
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{digest:x}")
}

/// Session-key basis for a scheduled snapshot turn. Deriving the ADK session key
/// from this instead of the channel name is what keeps a snapshot turn's
/// `sessions.session_key` distinct from the channel's live session (AC-2).
pub(crate) fn scheduled_snapshot_session_basis(definition_id: &str) -> String {
    format!("scheduled:{definition_id}")
}

/// Prepend the frozen snapshot context to the base agent prompt. The snapshot
/// block is the ONLY conversation context a snapshot turn receives (the headless
/// path disables live `channel_recent_context` for these turns).
pub(crate) fn inject_snapshot_prompt(rendered_context: &str, base_prompt: String) -> String {
    if rendered_context.trim().is_empty() {
        return base_prompt;
    }
    format!("{}\n\n{}", rendered_context.trim(), base_prompt)
}

/// Outcome of resolving a definition's context snapshot at fire time.
pub(crate) enum FireSnapshotResolution {
    /// No snapshot strategy, or an opt-in degrade-to-fresh: run with `prompt` and
    /// no session isolation label.
    Fresh(String),
    /// Inject the frozen context; `session_label` isolates the session key (AC-2).
    Injected {
        prompt: String,
        session_label: String,
    },
    /// Deterministic validation failure with `on_context_failure='fail'` — the
    /// caller terminalizes without re-arming (AC-5).
    Invalid(String),
}

/// Resolve the immutable snapshot for a fire, producing the effective prompt and
/// (when injected) the session-isolation label. Called before the launch-commit
/// barrier so a deterministic `fail` outcome terminalizes without a committed
/// launch.
///
/// Returns `Err` ONLY for a transient DB error (F2): the caller propagates it to
/// the existing retry/defer path so a connection blip never permanently
/// terminalizes a recurring definition — and never silently degrades a
/// `fresh`-policy definition either. Deterministic failures (missing row, digest
/// mismatch) become `Invalid` (fail policy) or `Fresh` (opt-in degrade).
pub(crate) async fn resolve_fire_snapshot(
    pool: &PgPool,
    message: &crate::db::scheduled_messages::ScheduledMessageRow,
    base_prompt: String,
) -> anyhow::Result<FireSnapshotResolution> {
    use crate::db::scheduled_messages::{CONTEXT_STRATEGY_SNAPSHOT, ON_CONTEXT_FAILURE_FRESH};

    if message.context_strategy != CONTEXT_STRATEGY_SNAPSHOT {
        return Ok(FireSnapshotResolution::Fresh(base_prompt));
    }
    let degrade_to_fresh = message.on_context_failure == ON_CONTEXT_FAILURE_FRESH;
    let Some(snapshot_id) = message.context_snapshot_id.as_deref() else {
        // chk_smsg_snapshot_required makes this unreachable for well-formed rows.
        let reason =
            "context_snapshot_invalid: snapshot strategy without context_snapshot_id".to_string();
        if degrade_to_fresh {
            tracing::warn!(id = message.id, "[smsg] {reason}; degrading to fresh");
            return Ok(FireSnapshotResolution::Fresh(base_prompt));
        }
        return Ok(FireSnapshotResolution::Invalid(reason));
    };
    match validate_snapshot_pg(pool, snapshot_id).await {
        Ok(snapshot) => Ok(FireSnapshotResolution::Injected {
            prompt: inject_snapshot_prompt(&snapshot.rendered_context, base_prompt),
            session_label: scheduled_snapshot_session_basis(&message.id),
        }),
        // Transient DB error: propagate so the fire path retries/defers. This
        // applies regardless of on_context_failure — a blip must not silently
        // degrade a `fresh`-policy definition to fresh either.
        Err(error) if error.is_transient() => Err(anyhow!(
            "context snapshot lookup failed transiently for {}: {error}",
            message.id
        )),
        Err(error) => {
            let reason = format!("context_snapshot_invalid: {error}");
            if degrade_to_fresh {
                tracing::warn!(
                    id = message.id,
                    "[smsg] {reason}; degrading to fresh per on_context_failure=fresh"
                );
                Ok(FireSnapshotResolution::Fresh(base_prompt))
            } else {
                Ok(FireSnapshotResolution::Invalid(reason))
            }
        }
    }
}

// ── DB capture / validate ───────────────────────────────────────────────────

fn row_digest(row: &ContextSnapshotRow) -> String {
    compute_content_digest(
        &row.rendered_context,
        row.transcript_frontier,
        row.pair_count,
        &SnapshotIntent {
            provider: row.provider.clone(),
            model: row.model.clone(),
            reasoning_effort: row.reasoning_effort.clone(),
            fast_mode: row.fast_mode,
            workspace_hint: row.workspace_hint.clone(),
        },
    )
}

/// The immutability guard: a snapshot row's stored `content_digest` must equal a
/// fresh recompute over its stored fields. Any post-capture mutation (content or
/// meta) breaks this. `validate_snapshot_pg` fails closed when it returns false.
pub(crate) fn snapshot_matches_digest(row: &ContextSnapshotRow) -> bool {
    row_digest(row) == row.content_digest
}

/// Capture a snapshot inside the reservation-create transaction. Frontier and
/// pairs are read on the same `tx` so the boundary is atomic with the insert.
/// Refuses (`EmptyContext`) when the source channel has no eligible pairs.
pub(crate) async fn capture_snapshot_tx(
    tx: &mut Transaction<'_, Postgres>,
    source_channel_id: &str,
    source_session_key: Option<&str>,
    intent: &SnapshotIntent,
) -> Result<ContextSnapshotRow, CaptureError> {
    let frontier = fetch_channel_frontier_tx(tx, source_channel_id)
        .await
        .map_err(CaptureError::Db)?;
    let pairs =
        fetch_channel_pairs_up_to_frontier_tx(tx, source_channel_id, frontier, SNAPSHOT_MAX_PAIRS)
            .await
            .map_err(CaptureError::Db)?;

    let rendered = render_snapshot_pairs(&pairs);
    if rendered.pair_count == 0 {
        return Err(CaptureError::EmptyContext);
    }

    let pair_count = rendered.pair_count as i32;
    let content_digest =
        compute_content_digest(&rendered.rendered_context, frontier, pair_count, intent);
    let id = format!("smcs_{}", Uuid::new_v4());

    let row = sqlx::query_as::<_, ContextSnapshotRow>(
        "INSERT INTO scheduled_message_context_snapshots
            (id, source_channel_id, source_session_key, transcript_frontier,
             rendered_context, pair_count, provider, model, reasoning_effort,
             fast_mode, workspace_hint, content_digest)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         RETURNING id, source_channel_id, source_session_key, transcript_frontier,
                   rendered_context, pair_count, provider, model, reasoning_effort,
                   fast_mode, workspace_hint, content_digest",
    )
    .bind(&id)
    .bind(source_channel_id)
    .bind(source_session_key)
    .bind(frontier)
    .bind(&rendered.rendered_context)
    .bind(pair_count)
    .bind(&intent.provider)
    .bind(&intent.model)
    .bind(&intent.reasoning_effort)
    .bind(intent.fast_mode)
    .bind(&intent.workspace_hint)
    .bind(&content_digest)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| CaptureError::Db(anyhow!("insert context snapshot: {error}")))?;

    Ok(row)
}

/// Load a snapshot by id.
pub(crate) async fn get_snapshot_pg(pool: &PgPool, id: &str) -> Result<Option<ContextSnapshotRow>> {
    sqlx::query_as::<_, ContextSnapshotRow>(
        "SELECT id, source_channel_id, source_session_key, transcript_frontier,
                rendered_context, pair_count, provider, model, reasoning_effort,
                fast_mode, workspace_hint, content_digest
         FROM scheduled_message_context_snapshots WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load context snapshot {id}: {error}"))
}

/// Validate a snapshot before the fire path's launch-commit barrier. Returns the
/// row when the recomputed digest matches; otherwise a deterministic error.
pub(crate) async fn validate_snapshot_pg(
    pool: &PgPool,
    id: &str,
) -> Result<ContextSnapshotRow, SnapshotValidationError> {
    // A lookup FAILURE is a transient DB error (retryable) — never conflate it
    // with a genuinely-absent row (deterministic Missing). Only `Ok(None)` is
    // Missing (F2).
    let row = get_snapshot_pg(pool, id)
        .await
        .map_err(SnapshotValidationError::Db)?
        .ok_or(SnapshotValidationError::Missing)?;
    if !snapshot_matches_digest(&row) {
        return Err(SnapshotValidationError::DigestMismatch);
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(user: &str, assistant: &str) -> ChannelTranscriptPair {
        ChannelTranscriptPair {
            user_message: user.to_string(),
            assistant_message: assistant.to_string(),
        }
    }

    fn intent() -> SnapshotIntent {
        SnapshotIntent {
            provider: Some("claude".to_string()),
            model: None,
            reasoning_effort: None,
            fast_mode: Some(false),
            workspace_hint: Some("/ws".to_string()),
        }
    }

    #[test]
    fn renders_pairs_oldest_first_with_pair_count() {
        let rendered = render_snapshot_pairs(&[pair("old-u", "old-a"), pair("new-u", "new-a")]);
        assert_eq!(rendered.pair_count, 2);
        let old = rendered.rendered_context.find("old-u").unwrap();
        let new = rendered.rendered_context.find("new-u").unwrap();
        assert!(old < new, "pairs must render oldest-first");
    }

    #[test]
    fn empty_or_blank_pairs_yield_zero_count() {
        assert_eq!(render_snapshot_pairs(&[]).pair_count, 0);
        assert_eq!(render_snapshot_pairs(&[pair("  ", "  ")]).pair_count, 0);
    }

    #[test]
    fn per_message_truncation_and_total_cap_drop_oldest() {
        let long = "한".repeat(SNAPSHOT_PAIR_MESSAGE_MAX_CHARS + 500);
        let one = render_snapshot_pairs(&[pair(&long, "a")]);
        assert!(one.rendered_context.contains(&format!(
            "{}{}",
            "한".repeat(SNAPSHOT_PAIR_MESSAGE_MAX_CHARS),
            TRUNCATION_MARKER
        )));

        let many: Vec<_> = (0..SNAPSHOT_MAX_PAIRS)
            .map(|i| {
                pair(
                    &format!("user-{i}-{}", "u".repeat(2_000)),
                    &format!("assistant-{i}-{}", "a".repeat(2_000)),
                )
            })
            .collect();
        let capped = render_snapshot_pairs(&many);
        assert!(capped.rendered_context.chars().count() <= SNAPSHOT_TOTAL_MAX_CHARS);
        // Oldest dropped to fit; newest retained.
        assert!(!capped.rendered_context.contains("user-0-"));
        assert!(
            capped
                .rendered_context
                .contains(&format!("user-{}-", SNAPSHOT_MAX_PAIRS - 1))
        );
    }

    #[test]
    fn caps_captured_pairs_to_max() {
        let many: Vec<_> = (0..(SNAPSHOT_MAX_PAIRS as usize + 5))
            .map(|i| pair(&format!("u{i}"), &format!("a{i}")))
            .collect();
        let rendered = render_snapshot_pairs(&many);
        assert!(rendered.pair_count <= SNAPSHOT_MAX_PAIRS as usize);
    }

    // ── IMMUTABILITY INVARIANT (mutation-proof target) ──────────────────────
    //
    // The guard under test is the digest recompute+compare in
    // `validate_snapshot_pg` / `row_digest`. Modeled purely here: a snapshot
    // captured over pairs [A,B] must stay byte-identical and digest-valid even
    // after the live channel gains pair C. The captured `rendered_context` is
    // frozen at capture, and its digest recomputes to the stored value.
    #[test]
    fn captured_snapshot_is_immutable_against_later_live_context() {
        let at_capture = [pair("design-A", "ok-A"), pair("change-B", "ok-B")];
        let rendered = render_snapshot_pairs(&at_capture);
        let frontier = 42;
        let pc = rendered.pair_count as i32;
        let digest = compute_content_digest(&rendered.rendered_context, frontier, pc, &intent());

        // Live context moves on (a third pair is added after capture).
        let live = [
            pair("design-A", "ok-A"),
            pair("change-B", "ok-B"),
            pair("unrelated-C", "ok-C"),
        ];
        let rendered_live = render_snapshot_pairs(&live);

        // The snapshot rendering is unaffected by later live changes.
        assert_ne!(rendered.rendered_context, rendered_live.rendered_context);
        assert!(!rendered.rendered_context.contains("unrelated-C"));

        // Re-validating the frozen snapshot still matches (guard passes).
        let recomputed =
            compute_content_digest(&rendered.rendered_context, frontier, pc, &intent());
        assert_eq!(digest, recomputed, "frozen snapshot digest must be stable");

        // Tampering with the frozen content (simulating a mutation) breaks the
        // digest — this is exactly what the validate guard rejects.
        let tampered = format!("{}\nunrelated-C", rendered.rendered_context);
        let tampered_digest = compute_content_digest(&tampered, frontier, pc, &intent());
        assert_ne!(
            digest, tampered_digest,
            "any post-capture content change must change the digest"
        );
    }

    fn valid_row() -> ContextSnapshotRow {
        let rendered = render_snapshot_pairs(&[pair("design-A", "ok-A")]);
        let pc = rendered.pair_count as i32;
        let digest = compute_content_digest(&rendered.rendered_context, 7, pc, &intent());
        ContextSnapshotRow {
            id: "smcs_test".to_string(),
            source_channel_id: "123".to_string(),
            source_session_key: None,
            transcript_frontier: 7,
            rendered_context: rendered.rendered_context,
            pair_count: pc,
            provider: intent().provider,
            model: intent().model,
            reasoning_effort: intent().reasoning_effort,
            fast_mode: intent().fast_mode,
            workspace_hint: intent().workspace_hint,
            content_digest: digest,
        }
    }

    // The digest guard (`snapshot_matches_digest`) is what makes a stored snapshot
    // immutable: `validate_snapshot_pg` rejects any row whose content/meta no
    // longer matches its stored digest. Mutation proof: force the guard to always
    // return true and both tampered assertions below fail.
    #[test]
    fn digest_guard_accepts_intact_row_and_rejects_tampered_row() {
        let row = valid_row();
        assert!(snapshot_matches_digest(&row), "an intact row must validate");

        let mut tampered_content = row.clone();
        tampered_content
            .rendered_context
            .push_str("\ninjected-later-context");
        assert!(
            !snapshot_matches_digest(&tampered_content),
            "post-capture content change must be rejected"
        );

        let mut tampered_meta = row.clone();
        tampered_meta.transcript_frontier += 1;
        assert!(
            !snapshot_matches_digest(&tampered_meta),
            "post-capture meta change must be rejected"
        );
    }

    #[test]
    fn inject_prepends_frozen_context_before_base_prompt() {
        let injected = inject_snapshot_prompt("FROZEN-BLOCK", "예약 본문".to_string());
        let ctx = injected.find("FROZEN-BLOCK").expect("frozen context");
        let body = injected.find("예약 본문").expect("base prompt");
        assert!(ctx < body, "frozen context must precede the base prompt");
        // Empty rendered context leaves the base prompt untouched.
        assert_eq!(inject_snapshot_prompt("   ", "base".to_string()), "base");
    }

    #[test]
    fn digest_changes_when_any_meta_field_changes() {
        let base = compute_content_digest("ctx", 1, 1, &intent());
        let mut other = intent();
        other.model = Some("opus".to_string());
        assert_ne!(base, compute_content_digest("ctx", 1, 1, &other));
        assert_ne!(base, compute_content_digest("ctx", 2, 1, &intent()));
        assert_ne!(base, compute_content_digest("ctx", 1, 2, &intent()));
    }

    #[test]
    fn digest_is_lowercase_hex_64() {
        let d = compute_content_digest("ctx", 1, 1, &intent());
        assert_eq!(d.len(), 64);
        assert!(
            d.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    // ── SESSION-KEY NON-COLLISION INVARIANT (mutation-proof target) ─────────
    #[test]
    fn snapshot_session_basis_never_equals_a_channel_name() {
        let definition_id = "smsg_11111111-2222-3333-4444-555555555555";
        let basis = scheduled_snapshot_session_basis(definition_id);
        for channel_name in [
            "general",
            "team-backend",
            "smsg_11111111-2222-3333-4444-555555555555",
            definition_id,
        ] {
            assert_ne!(
                basis, channel_name,
                "scheduled snapshot session basis must never collide with a channel name"
            );
        }
        assert!(basis.starts_with("scheduled:"));
        assert!(basis.contains(definition_id));
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::session_transcripts::{PersistSessionTranscript, persist_turn_db};

    async fn create_pool() -> (
        crate::dispatch::test_support::DispatchPostgresTestDb,
        PgPool,
    ) {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_context_snapshot_4658",
            "context snapshot capture/validate/immutability",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        (db, pool)
    }

    async fn seed_pair(
        pool: &PgPool,
        channel_id: &str,
        turn_id: &str,
        user: &str,
        assistant: &str,
    ) {
        let stored = persist_turn_db(
            Some(pool),
            PersistSessionTranscript {
                turn_id,
                session_key: Some("chan-session"),
                channel_id: Some(channel_id),
                agent_id: None,
                provider: Some("claude"),
                dispatch_id: None,
                user_message: user,
                assistant_message: assistant,
                events: &[],
                duration_ms: None,
            },
        )
        .await
        .expect("persist transcript");
        assert!(stored, "transcript should persist");
    }

    fn intent() -> SnapshotIntent {
        SnapshotIntent {
            provider: Some("claude".to_string()),
            model: None,
            reasoning_effort: None,
            fast_mode: None,
            workspace_hint: Some("agent-primary".to_string()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_then_validate_roundtrip_pg() {
        let (db, pool) = create_pool().await;
        seed_pair(&pool, "555", "t1", "design this", "designed").await;

        let mut tx = pool.begin().await.unwrap();
        let snapshot = capture_snapshot_tx(&mut tx, "555", None, &intent())
            .await
            .expect("capture snapshot");
        tx.commit().await.unwrap();

        assert_eq!(snapshot.pair_count, 1);
        assert!(snapshot.rendered_context.contains("design this"));
        let validated = validate_snapshot_pg(&pool, &snapshot.id)
            .await
            .expect("validate captured snapshot");
        assert_eq!(validated.content_digest, snapshot.content_digest);

        pool.close().await;
        db.drop().await;
    }

    // AC-1 at the capture/validate layer: a snapshot frozen at capture is NOT
    // mutated by later live-context inserts. The captured rendered_context is
    // bounded by the frontier and excludes any transcript added afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_is_immutable_against_post_capture_transcripts_pg() {
        let (db, pool) = create_pool().await;
        seed_pair(&pool, "777", "t1", "freeze-this", "frozen").await;

        let mut tx = pool.begin().await.unwrap();
        let snapshot = capture_snapshot_tx(&mut tx, "777", None, &intent())
            .await
            .expect("capture snapshot");
        tx.commit().await.unwrap();
        let captured_context = snapshot.rendered_context.clone();

        // Live channel context moves on AFTER capture.
        seed_pair(&pool, "777", "t2", "unrelated-later", "noise").await;

        let validated = validate_snapshot_pg(&pool, &snapshot.id)
            .await
            .expect("validate still passes");
        assert_eq!(
            validated.rendered_context, captured_context,
            "the frozen snapshot must not absorb later live context"
        );
        assert!(!validated.rendered_context.contains("unrelated-later"));

        pool.close().await;
        db.drop().await;
    }

    // Mutation proof (guard: snapshot_matches_digest in validate_snapshot_pg):
    // tamper with a stored snapshot's rendered_context and validate must reject
    // it with DigestMismatch. Removing the guard makes this assertion fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_rejects_tampered_snapshot_pg() {
        let (db, pool) = create_pool().await;
        seed_pair(&pool, "888", "t1", "authentic", "ok").await;

        let mut tx = pool.begin().await.unwrap();
        let snapshot = capture_snapshot_tx(&mut tx, "888", None, &intent())
            .await
            .expect("capture snapshot");
        tx.commit().await.unwrap();

        sqlx::query(
            "UPDATE scheduled_message_context_snapshots
             SET rendered_context = rendered_context || '\ninjected-tamper'
             WHERE id = $1",
        )
        .bind(&snapshot.id)
        .execute(&pool)
        .await
        .expect("tamper snapshot content");

        let result = validate_snapshot_pg(&pool, &snapshot.id).await;
        assert!(matches!(
            result,
            Err(SnapshotValidationError::DigestMismatch)
        ));

        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_missing_snapshot_is_missing_pg() {
        let (db, pool) = create_pool().await;
        let result = validate_snapshot_pg(&pool, "smcs_does_not_exist").await;
        assert!(matches!(result, Err(SnapshotValidationError::Missing)));
        pool.close().await;
        db.drop().await;
    }

    // F2: a transient DB failure (here: a closed pool) must classify as `Db`
    // (transient/retryable), NOT `Missing` (deterministic terminal). Mutation
    // proof: revert validate_snapshot_pg's lookup map_err back to `|_| Missing`
    // and this fails (it would report Missing → the recurring definition would be
    // permanently terminalized on a mere connection blip).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_transient_db_error_is_not_missing_pg() {
        let (db, pool) = create_pool().await;
        seed_pair(&pool, "1212", "t1", "present", "ok").await;
        let mut tx = pool.begin().await.unwrap();
        let snapshot = capture_snapshot_tx(&mut tx, "1212", None, &intent())
            .await
            .expect("capture snapshot");
        tx.commit().await.unwrap();

        // Close the pool so the lookup errors transiently against a real, present
        // row — this is a lookup FAILURE, not an absent row.
        pool.close().await;
        let result = validate_snapshot_pg(&pool, &snapshot.id).await;
        assert!(
            matches!(result, Err(SnapshotValidationError::Db(_))),
            "a transient lookup failure must be Db (retryable), not Missing/terminal"
        );
        assert!(result.is_err_and(|error| error.is_transient()));

        db.drop().await;
    }

    // Empty source channel fails closed (EmptyContext) rather than degrading.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_refuses_empty_channel_pg() {
        let (db, pool) = create_pool().await;
        let mut tx = pool.begin().await.unwrap();
        let result = capture_snapshot_tx(&mut tx, "999", None, &intent()).await;
        assert!(matches!(result, Err(CaptureError::EmptyContext)));
        tx.rollback().await.unwrap();
        pool.close().await;
        db.drop().await;
    }

    // A `/clear` boundary after older pairs excludes them from the capture.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_respects_clear_boundary_pg() {
        let (db, pool) = create_pool().await;
        seed_pair(&pool, "1010", "t1", "before-clear", "old").await;
        // Force the pre-clear row's created_at into the past, then record the
        // boundary now so the boundary strictly follows it.
        sqlx::query(
            "UPDATE session_transcripts SET created_at = NOW() - INTERVAL '1 hour'
             WHERE channel_id = '1010'",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::db::session_transcripts::record_channel_clear_boundary(Some(&pool), "1010")
            .await
            .unwrap();
        seed_pair(&pool, "1010", "t2", "after-clear", "new").await;

        let mut tx = pool.begin().await.unwrap();
        let snapshot = capture_snapshot_tx(&mut tx, "1010", None, &intent())
            .await
            .expect("capture after clear");
        tx.commit().await.unwrap();

        assert!(snapshot.rendered_context.contains("after-clear"));
        assert!(!snapshot.rendered_context.contains("before-clear"));

        pool.close().await;
        db.drop().await;
    }
}
