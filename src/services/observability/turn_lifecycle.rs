use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnEventMeta {
    pub kind: &'static str,
    pub severity: TurnEventSeverity,
    pub persist: bool,
    pub notify_user: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEventSeverity {
    Info,
    Warn,
}

impl TurnEventSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryDetails {
    pub reason: String,
    pub recovery_action: String,
    pub previous_session_key: Option<String>,
    pub recovered_session_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCompactionDetails {
    pub before_pct: u64,
    pub after_pct: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStrategyDetails {
    pub reason: String,
    pub provider_session_id: Option<String>,
    pub fingerprint: Option<String>,
}

impl SessionStrategyDetails {
    pub fn fresh(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            provider_session_id: None,
            fingerprint: None,
        }
    }

    pub fn resumed(reason: impl Into<String>, provider_session_id: &str) -> Self {
        Self {
            reason: reason.into(),
            provider_session_id: Some(provider_session_id.to_string()),
            fingerprint: Some(provider_session_fingerprint(provider_session_id)),
        }
    }
}

pub fn provider_session_fingerprint(provider_session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider_session_id.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}").chars().take(16).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    SessionFresh(SessionStrategyDetails),
    SessionResumed(SessionStrategyDetails),
    SessionResumeFailedWithRecovery(RecoveryDetails),
    ContextCompacted(ContextCompactionDetails),
}

impl TurnEvent {
    pub const SESSION_FRESH_PERSIST: bool = true;
    pub const SESSION_FRESH_NOTIFY_USER: bool = true;
    pub const SESSION_RESUMED_PERSIST: bool = true;
    pub const SESSION_RESUMED_NOTIFY_USER: bool = false;
    pub const SESSION_RESUME_FAILED_WITH_RECOVERY_PERSIST: bool = true;
    pub const SESSION_RESUME_FAILED_WITH_RECOVERY_NOTIFY_USER: bool = true;
    pub const CONTEXT_COMPACTED_PERSIST: bool = true;
    pub const CONTEXT_COMPACTED_NOTIFY_USER: bool = true;

    pub const SESSION_FRESH_META: TurnEventMeta = TurnEventMeta {
        kind: "session_fresh",
        severity: TurnEventSeverity::Info,
        persist: Self::SESSION_FRESH_PERSIST,
        notify_user: Self::SESSION_FRESH_NOTIFY_USER,
    };
    pub const SESSION_RESUMED_META: TurnEventMeta = TurnEventMeta {
        kind: "session_resumed",
        severity: TurnEventSeverity::Info,
        persist: Self::SESSION_RESUMED_PERSIST,
        notify_user: Self::SESSION_RESUMED_NOTIFY_USER,
    };
    pub const SESSION_RESUME_FAILED_WITH_RECOVERY_META: TurnEventMeta = TurnEventMeta {
        kind: "session_resume_failed_with_recovery",
        severity: TurnEventSeverity::Warn,
        persist: Self::SESSION_RESUME_FAILED_WITH_RECOVERY_PERSIST,
        notify_user: Self::SESSION_RESUME_FAILED_WITH_RECOVERY_NOTIFY_USER,
    };
    pub const CONTEXT_COMPACTED_META: TurnEventMeta = TurnEventMeta {
        kind: "context_compacted",
        severity: TurnEventSeverity::Info,
        persist: Self::CONTEXT_COMPACTED_PERSIST,
        notify_user: Self::CONTEXT_COMPACTED_NOTIFY_USER,
    };

    pub const fn meta(&self) -> TurnEventMeta {
        match self {
            Self::SessionFresh(_) => Self::SESSION_FRESH_META,
            Self::SessionResumed(_) => Self::SESSION_RESUMED_META,
            Self::SessionResumeFailedWithRecovery(_) => {
                Self::SESSION_RESUME_FAILED_WITH_RECOVERY_META
            }
            Self::ContextCompacted(_) => Self::CONTEXT_COMPACTED_META,
        }
    }

    pub const fn persist(&self) -> bool {
        self.meta().persist
    }

    pub const fn notify_user(&self) -> bool {
        self.meta().notify_user
    }

    fn details_json(&self) -> Value {
        match self {
            Self::SessionResumeFailedWithRecovery(details) => {
                serde_json::to_value(details).unwrap_or_else(|_| json!({}))
            }
            Self::ContextCompacted(details) => {
                serde_json::to_value(details).unwrap_or_else(|_| json!({}))
            }
            Self::SessionFresh(details) | Self::SessionResumed(details) => {
                serde_json::to_value(details).unwrap_or_else(|_| json!({}))
            }
        }
    }

    pub const fn notification_reason_code(&self) -> Option<&'static str> {
        match self {
            Self::SessionFresh(_) => Some("lifecycle.session_fresh"),
            Self::SessionResumed(_) => None,
            Self::SessionResumeFailedWithRecovery(_) => Some("lifecycle.session_resume_failed"),
            Self::ContextCompacted(_) => Some("lifecycle.context_compacted"),
        }
    }

    pub fn notification_content(&self) -> Option<String> {
        match self {
            Self::SessionFresh(_) => Some(
                "🆕 새 세션 시작\n이전 대화 컨텍스트 없음. 필요한 정보는 다시 알려주세요."
                    .to_string(),
            ),
            Self::SessionResumed(_) => None,
            Self::SessionResumeFailedWithRecovery(details) => {
                let reason = non_empty_or(&details.reason, "provider 응답 요약 없음");
                let recovery = non_empty_or(&details.recovery_action, "복구 없음");
                Some(format!(
                    "♻️ 이전 대화 이어가기 실패\n사유: {reason}\n복구: {recovery}"
                ))
            }
            Self::ContextCompacted(details) => Some(format!(
                "📦 컨텍스트 자동 압축\n이전 {}% → 이후 {}%\n보존: Goal / Progress / Decisions / Files / Next",
                details.before_pct, details.after_pct
            )),
        }
    }
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnLifecycleEmit {
    pub turn_id: String,
    pub channel_id: String,
    pub session_key: Option<String>,
    pub dispatch_id: Option<String>,
    pub event: TurnEvent,
    pub summary: String,
}

impl TurnLifecycleEmit {
    pub fn new(
        turn_id: impl Into<String>,
        channel_id: impl Into<String>,
        event: TurnEvent,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            channel_id: channel_id.into(),
            session_key: None,
            dispatch_id: None,
            event,
            summary: summary.into(),
        }
    }

    pub fn session_key(mut self, session_key: impl Into<String>) -> Self {
        self.session_key = Some(session_key.into());
        self
    }

    pub fn dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnLifecyclePersistedEvent {
    pub id: i64,
    pub kind: String,
    pub severity: String,
    pub notify_user: bool,
    pub notification_enqueued: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LatestSessionLifecycleEvent {
    pub kind: String,
    pub details_json: Value,
}

pub async fn emit_turn_lifecycle(
    pool: &PgPool,
    event: TurnLifecycleEmit,
) -> Result<Option<TurnLifecyclePersistedEvent>> {
    let meta = event.event.meta();
    if !meta.persist {
        return Ok(None);
    }

    let details_json = event.event.details_json();
    let details_text = serde_json::to_string(&details_json)
        .map_err(|error| anyhow!("serialize turn lifecycle details: {error}"))?;

    let row = sqlx::query(
        "INSERT INTO turn_lifecycle_events (
            turn_id,
            channel_id,
            session_key,
            dispatch_id,
            kind,
            severity,
            summary,
            details_json
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))
         RETURNING id, kind, severity",
    )
    .bind(&event.turn_id)
    .bind(&event.channel_id)
    .bind(&event.session_key)
    .bind(&event.dispatch_id)
    .bind(meta.kind)
    .bind(meta.severity.as_str())
    .bind(&event.summary)
    .bind(&details_text)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("insert turn_lifecycle_events: {error}"))?;

    let notification_enqueued = if meta.notify_user {
        let reason_code = event.event.notification_reason_code().ok_or_else(|| {
            anyhow!("turn lifecycle event marked notify_user without reason_code")
        })?;
        let content = event
            .event
            .notification_content()
            .ok_or_else(|| anyhow!("turn lifecycle event marked notify_user without content"))?;
        let target = notification_target(&event.channel_id);
        Some(
            crate::services::message_outbox::enqueue_lifecycle_notification_pg(
                pool,
                &target,
                None,
                reason_code,
                &content,
            )
            .await
            .map_err(|error| anyhow!("enqueue turn lifecycle notification: {error}"))?,
        )
    } else {
        None
    };

    Ok(Some(TurnLifecyclePersistedEvent {
        id: row
            .try_get::<i64, _>("id")
            .map_err(|error| anyhow!("decode turn lifecycle id: {error}"))?,
        kind: row
            .try_get::<String, _>("kind")
            .map_err(|error| anyhow!("decode turn lifecycle kind: {error}"))?,
        severity: row
            .try_get::<String, _>("severity")
            .map_err(|error| anyhow!("decode turn lifecycle severity: {error}"))?,
        notify_user: meta.notify_user,
        notification_enqueued,
    }))
}

pub async fn load_latest_session_lifecycle_event(
    pool: &PgPool,
    channel_id: &str,
    turn_id: &str,
) -> Result<Option<LatestSessionLifecycleEvent>> {
    let row = sqlx::query(
        "SELECT kind, details_json
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND turn_id = $2
           AND kind IN (
               'session_fresh',
               'session_resumed',
               'session_resume_failed_with_recovery'
           )
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load latest session lifecycle event: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(LatestSessionLifecycleEvent {
        kind: row
            .try_get::<String, _>("kind")
            .map_err(|error| anyhow!("decode session lifecycle kind: {error}"))?,
        details_json: row
            .try_get::<Value, _>("details_json")
            .map_err(|error| anyhow!("decode session lifecycle details_json: {error}"))?,
    }))
}

fn notification_target(channel_id: &str) -> String {
    let channel_id = channel_id.trim();
    if channel_id.starts_with("channel:") {
        channel_id.to_string()
    } else {
        format!("channel:{channel_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_turn_lifecycle_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "turn_lifecycle tests",
            )
            .await
            {
                eprintln!("[turn_lifecycle tests] skipping: {error}");
                return None;
            }

            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_and_migrate(&self) -> Result<PgPool> {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "turn_lifecycle tests",
            )
            .await
            .map_err(|error| anyhow!("{error}"))
        }

        async fn drop(self) {
            if let Err(error) = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "turn_lifecycle tests",
            )
            .await
            {
                eprintln!("[turn_lifecycle tests] drop failed: {error}");
            }
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[test]
    fn turn_event_metadata_is_compile_time_constant_backed() {
        assert!(TurnEvent::SESSION_FRESH_PERSIST);
        assert!(TurnEvent::SESSION_FRESH_NOTIFY_USER);
        assert!(!TurnEvent::SESSION_RESUMED_NOTIFY_USER);
        assert!(TurnEvent::SESSION_RESUME_FAILED_WITH_RECOVERY_NOTIFY_USER);
        assert!(TurnEvent::CONTEXT_COMPACTED_NOTIFY_USER);

        let recovery = TurnEvent::SessionResumeFailedWithRecovery(RecoveryDetails {
            reason: "missing session transcript".to_string(),
            recovery_action: "fresh_session".to_string(),
            previous_session_key: Some("old-session".to_string()),
            recovered_session_key: Some("new-session".to_string()),
        });
        assert_eq!(
            recovery.meta(),
            TurnEvent::SESSION_RESUME_FAILED_WITH_RECOVERY_META
        );
        assert_eq!(recovery.meta().severity, TurnEventSeverity::Warn);
    }

    #[test]
    fn turn_event_notification_policy_and_copy_match_lifecycle_contract() {
        let fresh = TurnEvent::SessionFresh(SessionStrategyDetails::fresh("first_turn"));
        assert_eq!(
            fresh.notification_reason_code(),
            Some("lifecycle.session_fresh")
        );
        assert_eq!(
            fresh.notification_content().as_deref(),
            Some("🆕 새 세션 시작\n이전 대화 컨텍스트 없음. 필요한 정보는 다시 알려주세요.")
        );
        assert_eq!(fresh.details_json()["reason"], "first_turn");

        let resumed = TurnEvent::SessionResumed(SessionStrategyDetails::resumed(
            "db_provider_session_restored",
            "provider-session-123",
        ));
        assert_eq!(resumed.notification_reason_code(), None);
        assert_eq!(resumed.notification_content(), None);
        let resumed_details = resumed.details_json();
        assert_eq!(resumed_details["reason"], "db_provider_session_restored");
        assert_eq!(resumed_details["providerSessionId"], "provider-session-123");
        assert_eq!(
            resumed_details["fingerprint"],
            provider_session_fingerprint("provider-session-123")
        );

        let recovery = TurnEvent::SessionResumeFailedWithRecovery(RecoveryDetails {
            reason: "provider rejected resume token".to_string(),
            recovery_action: "최근 디스코드 메시지 10건을 다음 턴에 자동 주입".to_string(),
            previous_session_key: None,
            recovered_session_key: Some("agentdesk-new".to_string()),
        });
        assert_eq!(
            recovery.notification_reason_code(),
            Some("lifecycle.session_resume_failed")
        );
        assert_eq!(
            recovery.notification_content().as_deref(),
            Some(
                "♻️ 이전 대화 이어가기 실패\n사유: provider rejected resume token\n복구: 최근 디스코드 메시지 10건을 다음 턴에 자동 주입"
            )
        );

        let compacted = TurnEvent::ContextCompacted(ContextCompactionDetails {
            before_pct: 91,
            after_pct: 37,
        });
        assert_eq!(
            compacted.notification_reason_code(),
            Some("lifecycle.context_compacted")
        );
        assert_eq!(
            compacted.notification_content().as_deref(),
            Some(
                "📦 컨텍스트 자동 압축\n이전 91% → 이후 37%\n보존: Goal / Progress / Decisions / Files / Next"
            )
        );
    }

    #[tokio::test]
    async fn emit_turn_lifecycle_persists_round_trip() -> Result<()> {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return Ok(());
        };
        let pool = pg_db.connect_and_migrate().await?;

        let persisted = emit_turn_lifecycle(
            &pool,
            TurnLifecycleEmit::new(
                "discord:42:420",
                "42",
                TurnEvent::SessionResumeFailedWithRecovery(RecoveryDetails {
                    reason: "session transcript missing".to_string(),
                    recovery_action: "start fresh session".to_string(),
                    previous_session_key: Some("agentdesk-old".to_string()),
                    recovered_session_key: Some("agentdesk-new".to_string()),
                }),
                "resume failed; started fresh session",
            )
            .session_key("agentdesk-new")
            .dispatch_id("dispatch-123"),
        )
        .await?
        .expect("event should persist");

        assert_eq!(persisted.kind, "session_resume_failed_with_recovery");
        assert_eq!(persisted.severity, "warn");
        assert!(persisted.notify_user);
        assert_eq!(persisted.notification_enqueued, Some(true));

        let row = sqlx::query(
            "SELECT turn_id, channel_id, session_key, dispatch_id, kind, severity, summary, details_json
             FROM turn_lifecycle_events
             WHERE id = $1",
        )
        .bind(persisted.id)
        .fetch_one(&pool)
        .await?;

        assert_eq!(row.try_get::<String, _>("turn_id")?, "discord:42:420");
        assert_eq!(row.try_get::<String, _>("channel_id")?, "42");
        assert_eq!(
            row.try_get::<Option<String>, _>("session_key")?,
            Some("agentdesk-new".to_string())
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("dispatch_id")?,
            Some("dispatch-123".to_string())
        );
        assert_eq!(
            row.try_get::<String, _>("kind")?,
            "session_resume_failed_with_recovery"
        );
        assert_eq!(row.try_get::<String, _>("severity")?, "warn");
        assert_eq!(
            row.try_get::<String, _>("summary")?,
            "resume failed; started fresh session"
        );
        let details = row.try_get::<Value, _>("details_json")?;
        assert_eq!(details["reason"], "session transcript missing");
        assert_eq!(details["recoveryAction"], "start fresh session");
        assert_eq!(details["previousSessionKey"], "agentdesk-old");
        assert_eq!(details["recoveredSessionKey"], "agentdesk-new");

        let outbox = sqlx::query(
            "SELECT target, content, bot, source, reason_code, session_key
             FROM message_outbox
             ORDER BY id ASC",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(outbox.try_get::<String, _>("target")?, "channel:42");
        assert_eq!(outbox.try_get::<String, _>("bot")?, "notify");
        assert_eq!(
            outbox.try_get::<String, _>("source")?,
            crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE
        );
        assert_eq!(
            outbox.try_get::<Option<String>, _>("reason_code")?,
            Some("lifecycle.session_resume_failed".to_string())
        );
        assert_eq!(
            outbox.try_get::<Option<String>, _>("session_key")?,
            Some("channel:42".to_string())
        );
        let content = outbox.try_get::<String, _>("content")?;
        assert!(content.contains("♻️ 이전 대화 이어가기 실패"));
        assert!(content.contains("사유: session transcript missing"));
        assert!(content.contains("복구: start fresh session"));

        pool.close().await;
        pg_db.drop().await;
        Ok(())
    }

    #[tokio::test]
    async fn emit_turn_lifecycle_enqueues_expected_notifications_and_dedupes() -> Result<()> {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return Ok(());
        };
        let pool = pg_db.connect_and_migrate().await?;

        let fresh_first = emit_turn_lifecycle(
            &pool,
            TurnLifecycleEmit::new(
                "discord:77:1",
                "77",
                TurnEvent::SessionFresh(SessionStrategyDetails::fresh("first_turn")),
                "fresh session",
            )
            .session_key("session-a"),
        )
        .await?
        .expect("fresh event persists");
        assert_eq!(fresh_first.notification_enqueued, Some(true));

        let fresh_duplicate = emit_turn_lifecycle(
            &pool,
            TurnLifecycleEmit::new(
                "discord:77:2",
                "77",
                TurnEvent::SessionFresh(SessionStrategyDetails::fresh(
                    "no_cached_provider_session",
                )),
                "fresh session again",
            )
            .session_key("session-b"),
        )
        .await?
        .expect("duplicate fresh event still persists");
        assert_eq!(fresh_duplicate.notification_enqueued, Some(false));

        let resumed = emit_turn_lifecycle(
            &pool,
            TurnLifecycleEmit::new(
                "discord:77:3",
                "77",
                TurnEvent::SessionResumed(SessionStrategyDetails::resumed(
                    "runtime_cached_provider_session",
                    "provider-session-123",
                )),
                "session resumed",
            )
            .session_key("session-b"),
        )
        .await?
        .expect("resumed event persists");
        assert!(!resumed.notify_user);
        assert_eq!(resumed.notification_enqueued, None);

        let compacted = emit_turn_lifecycle(
            &pool,
            TurnLifecycleEmit::new(
                "discord:77:4",
                "77",
                TurnEvent::ContextCompacted(ContextCompactionDetails {
                    before_pct: 88,
                    after_pct: 41,
                }),
                "context compacted",
            )
            .session_key("session-b"),
        )
        .await?
        .expect("compacted event persists");
        assert_eq!(compacted.notification_enqueued, Some(true));

        let rows = sqlx::query(
            "SELECT reason_code, content, source
             FROM message_outbox
             WHERE target = 'channel:77'
             ORDER BY id ASC",
        )
        .fetch_all(&pool)
        .await?;

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].try_get::<Option<String>, _>("reason_code")?,
            Some("lifecycle.session_fresh".to_string())
        );
        assert!(
            rows[0]
                .try_get::<String, _>("content")?
                .contains("🆕 새 세션 시작")
        );
        assert_eq!(
            rows[1].try_get::<Option<String>, _>("reason_code")?,
            Some("lifecycle.context_compacted".to_string())
        );
        assert!(
            rows[1]
                .try_get::<String, _>("content")?
                .contains("이전 88% → 이후 41%")
        );
        for row in rows {
            assert_eq!(
                row.try_get::<String, _>("source")?,
                crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE
            );
        }

        assert_eq!(
            crate::services::message_outbox::LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS,
            5 * 60
        );

        pool.close().await;
        pg_db.drop().await;
        Ok(())
    }
}
