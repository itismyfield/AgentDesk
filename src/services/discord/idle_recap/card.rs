use super::*;

/// Compose the recap card body. The header is deterministic; the optional
/// summary and suggested reply come from the bounded recap composer output.
pub(crate) fn compose_recap_text(
    snapshot: &RecapSnapshot,
    composer: Option<&RecapComposerOutput>,
    relay_probe: &RelayIntegrityProbe,
) -> String {
    let mut lines = vec![compose_recap_header(snapshot, relay_probe.status)];
    if let Some(summary) = composer
        .and_then(|output| output.summary.as_deref())
        .and_then(sanitize_recap_line)
    {
        // Blank line separates the header block from the summary so the card
        // reads as distinct sections instead of one cramped quote.
        lines.push(String::new());
        lines.push("> 📝 **요약**".to_string());
        lines.push(format!("> {summary}"));
    }
    if let Some(suggested_reply) = composer
        .and_then(|output| output.suggested_reply.as_deref())
        .and_then(sanitize_recap_line)
    {
        // The suggested reply gets its own labelled block on a separate line so
        // it is easy to read (and copy) rather than trailing the summary.
        // `suggested_reply_from_recap_content` parses the line after this label.
        lines.push(String::new());
        lines.push("> 💬 **추천 답변**".to_string());
        lines.push(format!("> {suggested_reply}"));
    }
    lines.join("\n")
}

pub(crate) fn suggested_reply_from_recap_content(content: &str) -> Option<String> {
    // Handles both the legacy inline form (`> 추천 답변: <reply>`) and the
    // current labelled form (`> 💬 **추천 답변**` on one line, `> <reply>` on
    // the next). Decorative emoji/markdown is tolerated around the label, but
    // incidental mentions of "추천 답변" in summary text are ignored.
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim().trim_start_matches('>').trim();
        if let Some(inline) = suggested_reply_inline_value(trimmed)
            && let Some(reply) = sanitize_recap_line(inline)
        {
            return Some(reply);
        }
        if !suggested_reply_label_is_standalone(trimmed) {
            continue;
        }
        // Label-only line: the reply lives on the next quoted line.
        if let Some(next) = lines.next() {
            let next_trimmed = next
                .trim()
                .trim_start_matches('>')
                .trim()
                .trim_start_matches('*')
                .trim();
            if let Some(reply) = sanitize_recap_line(next_trimmed) {
                return Some(reply);
            }
        }
        return None;
    }
    None
}

pub(super) fn suggested_reply_inline_value(line: &str) -> Option<&str> {
    line.strip_prefix("추천 답변:")
        .or_else(|| line.strip_prefix("추천 답변 :"))
}

pub(super) fn suggested_reply_label_is_standalone(line: &str) -> bool {
    let normalized = line
        .chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '>' | '*' | '_' | '`' | ':' | '：' | '💬' | '📝' | '📦' | '·' | '-' | '—'
                )
        })
        .collect::<String>();
    normalized == "추천답변"
}

pub(super) fn compose_recap_header(
    snapshot: &RecapSnapshot,
    relay_status: RelayIntegrityStatus,
) -> String {
    let now = Utc::now();
    let idle_since = snapshot
        .last_heartbeat
        .map(|t| format_korean_duration(now - t))
        .unwrap_or_else(|| "방금 전".to_string());

    let state_label = match relay_status {
        RelayIntegrityStatus::Suspect => "릴레이 누락 의심",
        RelayIntegrityStatus::Ok | RelayIntegrityStatus::Unknown => "이어서 질문 가능",
    };
    let provider_label = snapshot.provider.trim();
    let provider_label = if provider_label.is_empty() {
        "unknown"
    } else {
        provider_label
    };
    let context_label = match select_recap_context(snapshot, now) {
        RecapContextDisplay::Known { used, window } => {
            let used_label = format_token_count(used);
            let window_label = format_token_count(window);
            let pct = if window == 0 {
                None
            } else {
                Some(((u128::from(used) * 100) / u128::from(window)).min(100) as u64)
            };
            match pct {
                Some(percent) if used > window => {
                    format!("{used_label} / {window_label} tokens ({percent}%+, over limit)")
                }
                Some(percent) => format!("{used_label} / {window_label} tokens ({percent}%)"),
                None => "context unknown".to_string(),
            }
        }
        RecapContextDisplay::Stale => "context stale".to_string(),
        RecapContextDisplay::Unknown => "context unknown".to_string(),
    };
    format!(
        "📦 응답 완료 · {state_label}\n세션: {provider_label} · {context_label} · idle {idle_since}"
    )
}

/// Post the recap card via the configured notify bot. Routes through
/// `super::super::http::send_channel_message_with_components` so the
/// maintainability audit's `direct_discord_sends` rule (hard gate, #1282)
/// stays happy — that helper lives in the allowlisted `discord/http.rs`
/// module.
///
/// The recap card always carries `[새 세션 시작]` and may add bounded
/// diagnostic/suggested-reply actions. The interaction handler resolves the
/// message-id suffix back to a `session_key` through
/// `sessions.idle_recap_message_id` before acting.
pub(crate) async fn post_recap_card(
    http: &serenity::Http,
    channel_id: u64,
    content: &str,
    actions: RecapCardActions,
) -> Result<u64, String> {
    // We need the post-then-edit dance because the custom_id has to embed
    // the message id, but the message id is only known after Discord
    // assigns it. Step 1: post the card with a placeholder button whose
    // custom_id encodes a sentinel. Step 2: edit the card with the real
    // button. The brief window with the sentinel is harmless — the
    // interaction handler ignores unknown ids.
    let placeholder = make_recap_components("0", actions);
    let msg = super::super::http::send_channel_message_with_components(
        http,
        ChannelId::new(channel_id),
        content,
        placeholder,
    )
    .await
    .map_err(|e| format!("send_message: {e}"))?;
    let id = msg.id.get();
    let real = make_recap_components(&id.to_string(), actions);
    if let Err(e) = super::super::http::edit_channel_message_with_components(
        http,
        ChannelId::new(channel_id),
        msg.id,
        content,
        real,
    )
    .await
    {
        // Edit failure here is non-fatal — the placeholder button is
        // harmless. Surface the error in logs for diagnostics.
        tracing::warn!(
            error = %e,
            channel_id = channel_id,
            message_id = id,
            "idle_recap: button id rewrite edit failed (placeholder button left in place)"
        );
    }
    Ok(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecapCardActions {
    pub(crate) relay_investigate: bool,
    pub(crate) suggested_reply: bool,
    pub(crate) context_compact: bool,
}

impl RecapCardActions {
    pub(crate) fn for_probe_and_composer(
        relay_probe: &RelayIntegrityProbe,
        composer: Option<&RecapComposerOutput>,
    ) -> Self {
        Self {
            relay_investigate: relay_probe.is_suspect(),
            suggested_reply: composer
                .and_then(|output| output.suggested_reply.as_deref())
                .and_then(sanitize_recap_line)
                .is_some(),
            context_compact: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecapButtonKind {
    ClearSession,
    ContextCompact,
    RelayInvestigate,
    SendSuggestedReply,
}

pub(super) fn recap_button_plan(actions: RecapCardActions) -> Vec<RecapButtonKind> {
    let mut plan = vec![RecapButtonKind::ClearSession];
    if actions.context_compact {
        plan.push(RecapButtonKind::ContextCompact);
    }
    if actions.relay_investigate {
        plan.push(RecapButtonKind::RelayInvestigate);
    }
    if actions.suggested_reply {
        plan.push(RecapButtonKind::SendSuggestedReply);
    }
    plan
}

fn make_recap_components(
    message_id_suffix: &str,
    actions: RecapCardActions,
) -> Vec<CreateActionRow> {
    let buttons = recap_button_plan(actions)
        .into_iter()
        .map(|kind| recap_button(kind, message_id_suffix))
        .collect();
    vec![CreateActionRow::Buttons(buttons)]
}

fn recap_button(kind: RecapButtonKind, message_id_suffix: &str) -> CreateButton {
    let (prefix, label, style) = match kind {
        RecapButtonKind::ClearSession => (
            IDLE_RECAP_CLEAR_BUTTON_PREFIX,
            "새 세션 시작",
            ButtonStyle::Secondary,
        ),
        RecapButtonKind::ContextCompact => (
            IDLE_RECAP_COMPACT_BUTTON_PREFIX,
            "맥락 압축",
            ButtonStyle::Secondary,
        ),
        RecapButtonKind::RelayInvestigate => (
            IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX,
            "릴레이 조사",
            ButtonStyle::Danger,
        ),
        RecapButtonKind::SendSuggestedReply => (
            IDLE_RECAP_SUGGEST_BUTTON_PREFIX,
            "추천 답변 보내기",
            ButtonStyle::Primary,
        ),
    };
    CreateButton::new(format!("{prefix}{message_id_suffix}"))
        .style(style)
        .label(label)
}

pub(super) fn content_looks_like_idle_recap_card(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed == "📦" || trimmed.starts_with("📦 ")
}

fn component_is_idle_recap_clear_button(component: &serenity::ActionRowComponent) -> bool {
    match component {
        serenity::ActionRowComponent::Button(button) => match &button.data {
            ButtonKind::NonLink { custom_id, .. } => {
                custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
            }
            _ => false,
        },
        _ => false,
    }
}

fn message_is_idle_recap_card(message: &serenity::Message) -> bool {
    content_looks_like_idle_recap_card(&message.content)
        && message.components.iter().any(|row| {
            row.components
                .iter()
                .any(component_is_idle_recap_clear_button)
        })
}

/// Delete the previous recap card if one is recorded and still looks like an
/// idle-recap card. A stale/corrupt `sessions.idle_recap_message_id` must
/// never be allowed to delete a real turn response, so this probes Discord
/// first and only deletes messages carrying both the recap content marker and
/// the recap button custom id. Errors are swallowed so the renderer never
/// fails the cycle just because Discord has GC'd the old message itself.
/// Same allowlist rationale as `post_recap_card`.
pub(crate) async fn delete_previous_card(http: &serenity::Http, channel_id: u64, message_id: u64) {
    let channel = ChannelId::new(channel_id);
    let message = MessageId::new(message_id);
    match http.get_message(channel, message).await {
        Ok(current) if message_is_idle_recap_card(&current) => {
            // #3607: observe the delete; idle-recap cards are not provider-scoped.
            let result = super::super::http::delete_channel_message(http, channel, message).await;
            crate::services::observability::emit_relay_delete_result(
                "",
                channel_id,
                message_id,
                "idle_recap_previous_card",
                "delete_nonterminal",
                &result,
            );
        }
        Ok(current) => {
            tracing::warn!(
                channel_id = channel_id,
                message_id = message_id,
                author_id = current.author.id.get(),
                "idle_recap: preserving recorded message because it is not an idle recap card"
            );
        }
        Err(error) => {
            tracing::debug!(
                channel_id = channel_id,
                message_id = message_id,
                error = %error,
                "idle_recap: previous card probe failed; skipping destructive delete"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedRecapCard {
    pub(crate) session_key: String,
    pub(crate) channel_id: u64,
    pub(crate) message_id: u64,
}

pub(crate) async fn lookup_recorded_recaps_for_channel(
    pool: &PgPool,
    channel_id: u64,
) -> Result<Vec<RecordedRecapCard>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT session_key, idle_recap_channel_id, idle_recap_message_id
         FROM sessions
         WHERE idle_recap_channel_id = $1
           AND idle_recap_message_id IS NOT NULL
         ORDER BY idle_recap_message_id DESC",
    )
    .bind(channel_id as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(session_key, channel_id, message_id)| {
            Some(RecordedRecapCard {
                session_key,
                channel_id: u64::try_from(channel_id).ok()?,
                message_id: u64::try_from(message_id).ok()?,
            })
        })
        .collect())
}

pub(crate) async fn delete_older_recorded_recaps_for_channel(
    http: &serenity::Http,
    pool: &PgPool,
    channel_id: u64,
    current_message_id: u64,
) -> Result<(), sqlx::Error> {
    let cards = lookup_recorded_recaps_for_channel(pool, channel_id)
        .await?
        .into_iter()
        .filter(|card| recap_card_should_be_superseded(card.message_id, current_message_id))
        .collect();
    delete_recorded_recap_cards(http, pool, cards).await;
    Ok(())
}

async fn delete_recorded_recap_cards(
    http: &serenity::Http,
    pool: &PgPool,
    cards: Vec<RecordedRecapCard>,
) {
    for card in cards {
        delete_previous_card(http, card.channel_id, card.message_id).await;
        if let Err(error) = clear_recap_pointer(pool, &card.session_key, card.message_id).await {
            tracing::warn!(
                error = %error,
                session_key = %card.session_key,
                channel_id = card.channel_id,
                message_id = card.message_id,
                "idle_recap: failed to clear superseded recap pointer"
            );
        }
    }
}

pub(crate) async fn recap_channel_has_newer_card(
    pool: &PgPool,
    channel_id: u64,
    message_id: u64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
            FROM sessions
            WHERE idle_recap_channel_id = $1
              AND idle_recap_message_id IS NOT NULL
              AND idle_recap_message_id > $2
         )",
    )
    .bind(channel_id as i64)
    .bind(message_id as i64)
    .fetch_one(pool)
    .await
}

pub(super) fn recap_card_should_be_superseded(
    candidate_message_id: u64,
    current_message_id: u64,
) -> bool {
    candidate_message_id < current_message_id
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistRecapMessageIdResult {
    Persisted {
        previous_card: Option<RecordedRecapCard>,
    },
    LostDeleteAndSkip,
}

/// Persist the freshly-posted message id (and the channel it lives in) so
/// the next cycle can delete it and `message_handler` can clear it the
/// moment the user sends their next turn.
///
/// #3148 (compare-and-swap on the turn generation): the persist is conditional
/// on `idle_recap_turn_generation` still equalling `captured_generation` — the
/// value read at snapshot load (`load_recap_snapshot`), ~20s before this
/// commit. A turn claimed anywhere in the compose/persist window calls
/// `bump_turn_generation`, which increments the same row's counter; the two
/// UPDATEs serialize on the Postgres row, so if a claim committed first this
/// CAS matches 0 rows and the caller deletes the just-posted card instead of
/// stamping it over the now-active turn (closing Window 1 atomically). Returns
/// `LostDeleteAndSkip` when the generation CAS lost or this session already
/// points at a newer recap card.
pub(crate) async fn persist_recap_message_id(
    pool: &PgPool,
    session_key: &str,
    channel_id: u64,
    message_id: u64,
    captured_generation: i64,
) -> Result<PersistRecapMessageIdResult, sqlx::Error> {
    let previous = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
        "WITH current AS (
             SELECT session_key,
                    idle_recap_message_id AS previous_message_id,
                    idle_recap_channel_id AS previous_channel_id
             FROM sessions
             WHERE session_key = $3
               AND idle_recap_turn_generation = $4
               AND (
                    idle_recap_message_id IS NULL
                    OR idle_recap_message_id < $1
               )
             FOR UPDATE
         ),
         updated AS (
             UPDATE sessions AS s
             SET idle_recap_message_id = $1,
                 idle_recap_channel_id = $2,
                 idle_recap_posted_at  = NOW()
             FROM current
             WHERE s.session_key = current.session_key
             RETURNING current.previous_message_id, current.previous_channel_id
         )
         SELECT previous_message_id, previous_channel_id
         FROM updated",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(session_key)
    .bind(captured_generation)
    .fetch_optional(pool)
    .await?;

    let Some((previous_message_id, previous_channel_id)) = previous else {
        return Ok(PersistRecapMessageIdResult::LostDeleteAndSkip);
    };

    let previous_card =
        previous_message_id
            .zip(previous_channel_id)
            .and_then(|(message_id, channel_id)| {
                Some(RecordedRecapCard {
                    session_key: session_key.to_string(),
                    channel_id: u64::try_from(channel_id).ok()?,
                    message_id: u64::try_from(message_id).ok()?,
                })
            });
    Ok(PersistRecapMessageIdResult::Persisted { previous_card })
}
