//! Repository layer for round-table meeting persistence.
//!
//! All raw SQL for the `meetings` and `meeting_transcripts` tables, plus the
//! `kv_meta` keys used by the meeting feature, lives here.  Route handlers
//! delegate to these functions and never issue SQL directly.
//!
//! Extracted from `src/server/routes/meetings.rs` as part of #3570 (route SRP
//! decomposition).

use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

// ── Transcript helpers ──────────────────────────────────────────────────────

/// Load all transcript entries for a meeting, ordered by `seq ASC, id ASC`.
pub async fn load_transcripts_pg(pool: &PgPool, meeting_id: &str) -> Vec<JsonValue> {
    sqlx::query(
        "SELECT id, seq, round, speaker_agent_id, speaker_name, content, is_summary
         FROM meeting_transcripts
         WHERE meeting_id = $1
         ORDER BY seq ASC, id ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| {
        let speaker_agent_id = row
            .try_get::<Option<String>, _>("speaker_agent_id")
            .ok()
            .flatten();
        json!({
            "id": row.try_get::<i64, _>("id").unwrap_or(0),
            "seq": row.try_get::<Option<i64>, _>("seq").ok().flatten(),
            "round": row.try_get::<Option<i64>, _>("round").ok().flatten(),
            "speaker_role_id": speaker_agent_id,
            "speaker_agent_id": row.try_get::<Option<String>, _>("speaker_agent_id").ok().flatten(),
            "speaker_name": row.try_get::<Option<String>, _>("speaker_name").ok().flatten(),
            "content": row.try_get::<Option<String>, _>("content").ok().flatten(),
            "is_summary": row.try_get::<Option<bool>, _>("is_summary").ok().flatten().unwrap_or(false),
        })
    })
    .collect()
}

// ── KV-meta helpers ─────────────────────────────────────────────────────────

/// Upsert a single `kv_meta` key/value pair.
pub async fn upsert_kv_meta_pg(pool: &PgPool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Fetch a single `kv_meta` value by key (returns `None` when absent).
pub async fn get_kv_meta_pg(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

// ── Meeting enrichment helpers ──────────────────────────────────────────────

/// Attach `issue_repo` and `issue_urls` fields to a meeting JSON object.
pub async fn enrich_meeting_with_issue_data_pg(
    pool: &PgPool,
    meeting_id: &str,
    obj: &mut serde_json::Map<String, JsonValue>,
) {
    let issue_repo = get_kv_meta_pg(pool, &format!("meeting_issue_repo:{meeting_id}")).await;
    obj.insert("issue_repo".to_string(), json!(issue_repo));

    let rows = sqlx::query("SELECT key, value FROM kv_meta WHERE key LIKE $1 ORDER BY key")
        .bind(format!("meeting:{meeting_id}:issue:%:url"))
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let mut issue_urls = serde_json::Map::new();
    for row in rows {
        let key = row.try_get::<String, _>("key").unwrap_or_default();
        let value = row.try_get::<Option<String>, _>("value").ok().flatten();
        if let Some(issue_key) = key
            .strip_prefix(&format!("meeting:{meeting_id}:issue:"))
            .and_then(|rest| rest.strip_suffix(":url"))
        {
            issue_urls.insert(issue_key.to_string(), json!(value));
        }
    }
    obj.insert(
        "issue_urls".to_string(),
        serde_json::Value::Object(issue_urls),
    );
}

/// Attach `meeting_hash` and `thread_hash` fields to a meeting JSON object.
pub async fn enrich_meeting_with_query_hashes_pg(
    pool: &PgPool,
    meeting_id: &str,
    obj: &mut serde_json::Map<String, JsonValue>,
) {
    let meeting_hash = get_kv_meta_pg(pool, &format!("meeting_query_hash:{meeting_id}")).await;
    obj.insert("meeting_hash".to_string(), json!(meeting_hash));

    let thread_hash = get_kv_meta_pg(pool, &format!("meeting_thread_hash:{meeting_id}")).await;
    obj.insert("thread_hash".to_string(), json!(thread_hash));
}

// ── Row mapping ─────────────────────────────────────────────────────────────

/// Map a `meetings` table row to the canonical JSON shape returned by all
/// read endpoints.
pub fn meeting_row_to_json_pg(row: &sqlx::postgres::PgRow) -> JsonValue {
    let participant_names = row
        .try_get::<Option<String>, _>("participant_names")
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str::<JsonValue>(&value).ok())
        .unwrap_or_else(|| json!([]));
    json!({
        "id": row.try_get::<String, _>("id").unwrap_or_default(),
        "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
        "thread_id": row.try_get::<Option<String>, _>("thread_id").ok().flatten(),
        "agenda": row.try_get::<Option<String>, _>("title").ok().flatten(),
        "title": row.try_get::<Option<String>, _>("title").ok().flatten(),
        "status": row.try_get::<Option<String>, _>("status").ok().flatten(),
        "effective_rounds": row.try_get::<Option<i64>, _>("effective_rounds").ok().flatten(),
        "started_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at").ok().flatten().map(|ts| ts.timestamp_millis()),
        "completed_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").ok().flatten().map(|ts| ts.timestamp_millis()),
        "summary": row.try_get::<Option<String>, _>("summary").ok().flatten(),
        "primary_provider": row.try_get::<Option<String>, _>("primary_provider").ok().flatten(),
        "reviewer_provider": row.try_get::<Option<String>, _>("reviewer_provider").ok().flatten(),
        "participant_names": participant_names,
        "selection_reason": row.try_get::<Option<String>, _>("selection_reason").ok().flatten(),
        "created_at": row.try_get::<Option<i64>, _>("created_at").ok().flatten(),
    })
}

const MEETING_SELECT: &str = "SELECT id, channel_id, thread_id, title, status, \
     effective_rounds::BIGINT AS effective_rounds, started_at, completed_at, summary, \
     primary_provider, reviewer_provider, participant_names, selection_reason, created_at \
     FROM meetings";

// ── Compound read operations ─────────────────────────────────────────────────

/// Load a single meeting by id, fully enriched with transcripts, issue data,
/// and query hashes.  Returns `None` when no row is found.
pub async fn load_meeting_pg(
    pool: &PgPool,
    id: &str,
    apply_fallback: impl FnOnce(&mut serde_json::Map<String, JsonValue>, &[JsonValue]),
) -> Result<Option<JsonValue>, sqlx::Error> {
    let Some(row) = sqlx::query(&format!("{MEETING_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let mut meeting = meeting_row_to_json_pg(&row);
    let transcripts = load_transcripts_pg(pool, id).await;
    let obj = meeting.as_object_mut().expect("meeting json object"); // agentdesk-audit: allow-unwrap — meeting_row_to_json_pg always returns a json!({…}) Object
    obj.insert("transcripts".to_string(), json!(&transcripts));
    obj.insert("entries".to_string(), json!(&transcripts));
    enrich_meeting_with_issue_data_pg(pool, id, obj).await;
    enrich_meeting_with_query_hashes_pg(pool, id, obj).await;
    apply_fallback(obj, &transcripts);
    Ok(Some(meeting))
}

/// Fetch all meetings ordered by `started_at DESC`, fully enriched.
pub async fn list_meetings_pg(
    pool: &PgPool,
    apply_fallback: impl Fn(&mut serde_json::Map<String, JsonValue>, &[JsonValue]),
) -> Result<Vec<JsonValue>, sqlx::Error> {
    let rows = sqlx::query(&format!("{MEETING_SELECT} ORDER BY started_at DESC"))
        .fetch_all(pool)
        .await?;

    let mut meetings: Vec<JsonValue> = rows.iter().map(meeting_row_to_json_pg).collect();
    for meeting in meetings.iter_mut() {
        let meeting_id = meeting
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(mid) = meeting_id {
            let transcripts = load_transcripts_pg(pool, &mid).await;
            let obj = meeting.as_object_mut().unwrap(); // agentdesk-audit: allow-unwrap — meeting_row_to_json_pg always returns a json!({…}) Object
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(&transcripts));
            enrich_meeting_with_issue_data_pg(pool, &mid, obj).await;
            enrich_meeting_with_query_hashes_pg(pool, &mid, obj).await;
            apply_fallback(obj, &transcripts);
        }
    }
    Ok(meetings)
}

// ── Write operations ─────────────────────────────────────────────────────────

/// Delete a meeting and its transcripts.  Returns `true` if a row was deleted,
/// `false` if no meeting with that id existed.
pub async fn delete_meeting_pg(pool: &PgPool, id: &str) -> Result<bool, sqlx::Error> {
    let _ = sqlx::query("DELETE FROM meeting_transcripts WHERE meeting_id = $1")
        .bind(id)
        .execute(pool)
        .await;

    let result = sqlx::query("DELETE FROM meetings WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Return `true` when a meeting row with the given id exists.
pub async fn meeting_exists_pg(pool: &PgPool, id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM meetings WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map(|count| count > 0)
        .unwrap_or(false)
}

/// Store the issue repo for a meeting in `kv_meta`.
pub async fn upsert_issue_repo_pg(
    pool: &PgPool,
    meeting_id: &str,
    repo: &str,
) -> Result<(), sqlx::Error> {
    upsert_kv_meta_pg(pool, &format!("meeting_issue_repo:{meeting_id}"), repo).await
}

// ── Issue-tracking helpers ───────────────────────────────────────────────────

/// Fetch the issue repo stored for a meeting (returns `None` when unset).
pub async fn get_meeting_issue_repo_pg(pool: &PgPool, meeting_id: &str) -> Option<String> {
    get_kv_meta_pg(pool, &format!("meeting_issue_repo:{meeting_id}")).await
}

/// Fetch all summary transcript texts for a meeting, ordered by `seq ASC`.
pub async fn get_meeting_summaries_pg(pool: &PgPool, meeting_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT content FROM meeting_transcripts
         WHERE meeting_id = $1 AND is_summary = true
         ORDER BY seq ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// Return `true` when the given issue item key has been discarded.
pub async fn is_issue_discarded_pg(pool: &PgPool, meeting_id: &str, key: &str) -> bool {
    get_kv_meta_pg(pool, &format!("meeting:{meeting_id}:issue:{key}:discarded"))
        .await
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Return the previously-stored issue URL, if any.
pub async fn get_issue_url_pg(pool: &PgPool, meeting_id: &str, key: &str) -> Option<String> {
    get_kv_meta_pg(pool, &format!("meeting:{meeting_id}:issue:{key}:url")).await
}

/// Store a successfully-created issue URL.
pub async fn store_issue_url_pg(
    pool: &PgPool,
    meeting_id: &str,
    key: &str,
    url: &str,
) -> Result<(), sqlx::Error> {
    upsert_kv_meta_pg(pool, &format!("meeting:{meeting_id}:issue:{key}:url"), url).await
}

/// Mark a single issue item as discarded.
pub async fn discard_issue_pg(
    pool: &PgPool,
    meeting_id: &str,
    key: &str,
) -> Result<(), sqlx::Error> {
    upsert_kv_meta_pg(
        pool,
        &format!("meeting:{meeting_id}:issue:{key}:discarded"),
        "true",
    )
    .await
}

/// Mark all summary items for a meeting as discarded.
/// Returns the count of items that were discarded.
pub async fn discard_all_issues_pg(pool: &PgPool, meeting_id: &str) -> i64 {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM meeting_transcripts WHERE meeting_id = $1 AND is_summary = true",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    for i in 0..count {
        let _ = upsert_kv_meta_pg(
            pool,
            &format!("meeting:{meeting_id}:issue:item-{i}:discarded"),
            "true",
        )
        .await;
    }

    count
}

// ── Upsert operation (upsert_meeting handler) ────────────────────────────────

/// Parameters for the big meeting upsert.
pub struct UpsertMeetingParams<'a> {
    pub id: &'a str,
    pub channel_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub agenda: &'a str,
    pub status: &'a str,
    pub total_rounds: i64,
    pub started_at_dt: chrono::DateTime<chrono::Utc>,
    pub completed_at_dt: Option<chrono::DateTime<chrono::Utc>>,
    pub summary: Option<&'a str>,
    pub primary_provider: Option<&'a str>,
    pub reviewer_provider: Option<&'a str>,
    pub participant_names_json: &'a str,
    pub selection_reason: Option<&'a str>,
    pub created_at: i64,
    pub agenda_update: Option<&'a str>,
    pub status_update: Option<&'a str>,
    pub total_rounds_update: Option<i64>,
    pub participant_names_update_json: Option<&'a str>,
}

/// Execute the INSERT … ON CONFLICT meeting upsert.
pub async fn upsert_meeting_record_pg(
    pool: &PgPool,
    p: &UpsertMeetingParams<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO meetings (
            id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
            primary_provider, reviewer_provider, participant_names, selection_reason, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT(id) DO UPDATE SET
            channel_id = COALESCE(EXCLUDED.channel_id, meetings.channel_id),
            thread_id = COALESCE(EXCLUDED.thread_id, meetings.thread_id),
            title = COALESCE($15, meetings.title),
            status = COALESCE($16, meetings.status),
            effective_rounds = COALESCE($17, meetings.effective_rounds),
            started_at = COALESCE(meetings.started_at, excluded.started_at),
            completed_at = COALESCE($18, meetings.completed_at),
            summary = COALESCE($19, meetings.summary),
            primary_provider = COALESCE($20, meetings.primary_provider),
            reviewer_provider = COALESCE($21, meetings.reviewer_provider),
            participant_names = COALESCE($22, meetings.participant_names),
            selection_reason = COALESCE($23, meetings.selection_reason),
            created_at = COALESCE(meetings.created_at, EXCLUDED.created_at)",
    )
    .bind(p.id)
    .bind(p.channel_id)
    .bind(p.thread_id)
    .bind(p.agenda)
    .bind(p.status)
    .bind(p.total_rounds)
    .bind(p.started_at_dt)
    .bind(p.completed_at_dt)
    .bind(p.summary)
    .bind(p.primary_provider)
    .bind(p.reviewer_provider)
    .bind(p.participant_names_json)
    .bind(p.selection_reason)
    .bind(p.created_at)
    .bind(p.agenda_update)
    .bind(p.status_update)
    .bind(p.total_rounds_update)
    .bind(p.completed_at_dt)
    .bind(p.summary)
    .bind(p.primary_provider)
    .bind(p.reviewer_provider)
    .bind(p.participant_names_update_json)
    .bind(p.selection_reason)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Check whether a meeting row already existed before an upsert.
pub async fn meeting_existed_pg(pool: &PgPool, meeting_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM meetings WHERE id = $1")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Fetch the `thread_id` stored for a meeting.
pub async fn get_meeting_thread_id_pg(pool: &PgPool, meeting_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT thread_id FROM meetings WHERE id = $1")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Return the next available `seq` value for a meeting's transcripts.
pub async fn get_next_transcript_seq_pg(pool: &PgPool, meeting_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(seq), 0)::BIGINT + 1 FROM meeting_transcripts WHERE meeting_id = $1",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .unwrap_or(1)
}

/// Transcript entry parameters for bulk insert.
pub struct TranscriptEntry {
    pub seq: i64,
    pub round: Option<i64>,
    pub speaker_role_id: Option<String>,
    pub speaker_name: Option<String>,
    pub content: Option<String>,
    pub is_summary: bool,
}

/// Replace all transcripts for a meeting (DELETE then bulk INSERT).
pub async fn replace_transcripts_pg(
    pool: &PgPool,
    meeting_id: &str,
    entries: &[TranscriptEntry],
) -> Result<i64, sqlx::Error> {
    sqlx::query("DELETE FROM meeting_transcripts WHERE meeting_id = $1")
        .bind(meeting_id)
        .execute(pool)
        .await?;

    let mut next_seq = 1i64;
    for entry in entries {
        next_seq = next_seq.max(entry.seq + 1);
        sqlx::query(
            "INSERT INTO meeting_transcripts (
                meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(meeting_id)
        .bind(entry.seq)
        .bind(entry.round)
        .bind(entry.speaker_role_id.as_deref())
        .bind(entry.speaker_name.as_deref())
        .bind(entry.content.as_deref())
        .bind(entry.is_summary)
        .execute(pool)
        .await?;
    }
    Ok(next_seq)
}

/// Fetch `effective_rounds` for a meeting.
pub async fn get_effective_rounds_pg(pool: &PgPool, meeting_id: &str) -> Option<i64> {
    sqlx::query_scalar::<_, i64>("SELECT effective_rounds::BIGINT FROM meetings WHERE id = $1")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Fetch the most-recent summary transcript id, if any.
pub async fn get_latest_summary_id_pg(pool: &PgPool, meeting_id: &str) -> Option<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id
         FROM meeting_transcripts
         WHERE meeting_id = $1 AND is_summary = true
         ORDER BY seq DESC, id DESC
         LIMIT 1",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Update an existing summary transcript row.
pub async fn update_summary_transcript_pg(
    pool: &PgPool,
    summary_id: i64,
    summary_round: i64,
    summary_text: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meeting_transcripts
         SET round = $2,
             speaker_agent_id = NULL,
             speaker_name = $3,
             content = $4,
             is_summary = true
         WHERE id = $1",
    )
    .bind(summary_id)
    .bind(summary_round)
    .bind("Summary")
    .bind(summary_text)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Insert a new summary transcript row.
pub async fn insert_summary_transcript_pg(
    pool: &PgPool,
    meeting_id: &str,
    next_seq: i64,
    summary_round: i64,
    summary_text: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO meeting_transcripts (
            meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
        ) VALUES ($1, $2, $3, NULL, $4, $5, true)",
    )
    .bind(meeting_id)
    .bind(next_seq)
    .bind(summary_round)
    .bind("Summary")
    .bind(summary_text)
    .execute(pool)
    .await
    .map(|_| ())
}

// ── Query hash helpers ───────────────────────────────────────────────────────

fn short_query_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..6])
}

pub fn meeting_query_hash(meeting_id: &str) -> String {
    format!(
        "#meeting-{}",
        short_query_hash(&format!("meeting:{meeting_id}"))
    )
}

pub fn thread_query_hash(thread_id: &str) -> String {
    format!(
        "#thread-{}",
        short_query_hash(&format!("thread:{thread_id}"))
    )
}

/// Persist meeting and thread query hashes in `kv_meta` for lookup-by-hash.
pub async fn persist_meeting_query_hashes_pg(
    pool: &PgPool,
    meeting_id: &str,
    thread_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let meeting_hash = meeting_query_hash(meeting_id);
    let normalized_thread_id = thread_id.map(str::trim).filter(|value| !value.is_empty());
    let computed_thread_hash = normalized_thread_id.map(|tid| thread_query_hash(tid));

    upsert_kv_meta_pg(
        pool,
        &format!("meeting_query_hash:{meeting_id}"),
        &meeting_hash,
    )
    .await?;
    upsert_kv_meta_pg(
        pool,
        &format!("meeting_query_hash_lookup:{meeting_hash}"),
        meeting_id,
    )
    .await?;

    if let (Some(thread_id), Some(thread_hash)) =
        (normalized_thread_id, computed_thread_hash.as_deref())
    {
        upsert_kv_meta_pg(
            pool,
            &format!("meeting_thread_hash:{meeting_id}"),
            thread_hash,
        )
        .await?;
        upsert_kv_meta_pg(
            pool,
            &format!("meeting_thread_hash_lookup:{thread_hash}"),
            &json!({"meeting_id": meeting_id, "thread_id": thread_id}).to_string(),
        )
        .await?;
    }
    Ok(())
}

// reason: HashMap used by callers that need to batch-look up issue URL/discarded state.
#[allow(dead_code)]
pub(crate) type IssueStateMap = HashMap<String, String>;
