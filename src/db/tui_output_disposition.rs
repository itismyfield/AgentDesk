//! A rejection reservation survives restart; only its exact receipt or no-effect proof settles it.
use sqlx::PgPool;
use uuid::Uuid;

pub(crate) mod closure;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub(crate) channel: u64,
    pub(crate) input: u64,
    pub(crate) nonce: String,
    pub(crate) execution: String,
    pub(crate) source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Deadline,
    Invalidated,
    SourceRejected,
    TrustUnavailable,
    /// The channel's O adoption was refused or released, so its held body is discarded.
    AdoptionRefused,
    /// A restarted takeover could not place these turns before one closed with its reason.
    Unplaced,
}

impl Reason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Deadline => {
                "출력 연결 정상화 시간이 만료되어 응답을 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::Invalidated => {
                "이 실행이 취소되거나 교체되어 응답을 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::SourceRejected => {
                "응답의 실행·출처 연결을 검증하지 못해 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::TrustUnavailable => {
                "작업 폴더 신뢰 또는 로그인을 정상화하지 못해 응답을 게시하지 않았습니다. 터미널 세션을 확인해 주세요."
            }
            Self::AdoptionRefused => {
                "이 채널의 출력 연결이 거부되어 응답을 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::Unplaced => {
                "서버 재시작 뒤 이어받지 못한 이전 응답이 있어 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
        }
    }
}

impl Identity {
    pub(crate) fn key(&self) -> String {
        use serde_json::Value;
        use sha2::{Digest, Sha256};
        // The same compact JSON bytes `serde_json::to_vec(self)` writes, built without a fallible call.
        let json = format!(
            r#"{{"channel":{},"input":{},"nonce":{},"execution":{},"source":{}}}"#,
            self.channel,
            self.input,
            Value::from(self.nonce.as_str()),
            Value::from(self.execution.as_str()),
            Value::from(self.source.as_str()),
        );
        format!("sha256:{:x}", Sha256::digest(json))
    }
}
pub(crate) struct Attempt {
    key: String,
    id: Uuid,
    identity: Identity,
    reason: Reason,
}
pub(crate) struct Receipt {
    key: String,
    message: u64,
    reason: Reason,
    template_version: i32,
}
impl Receipt {
    pub(crate) fn reason(&self) -> Reason {
        self.reason
    }
    pub(crate) fn template_version(&self) -> i32 {
        self.template_version
    }
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
    pub(crate) fn message(&self) -> u64 {
        self.message
    }
}
pub(crate) async fn reserve(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
) -> Result<Option<Attempt>, String> {
    persist_intent(pool, id, reason).await?;
    let key = id.key();
    let identity = serde_json::to_value(id).map_err(|e| e.to_string())?;
    let reason = serde_json::to_string(&reason).map_err(|e| e.to_string())?;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let old: (serde_json::Value,String,Option<Uuid>,Option<String>) = sqlx::query_as("SELECT identity,reason,attempt,message_id FROM tui_output_dispositions WHERE output_key=$1 FOR UPDATE")
        .bind(&key).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
    if old.0 != identity || old.1 != reason {
        return Err("rejection identity/reason changed".into());
    }
    if old.2.is_some() || old.3.is_some() {
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(None);
    }
    let attempt = Uuid::new_v4();
    sqlx::query("UPDATE tui_output_dispositions SET attempt=$2 WHERE output_key=$1")
        .bind(&key)
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Some(Attempt {
        key,
        id: attempt,
        identity: id.clone(),
        reason: serde_json::from_str(&reason).map_err(|e| e.to_string())?,
    }))
}
impl Attempt {
    pub(crate) fn nonce(&self) -> String {
        format!("tr1{}", &self.key[7..27])
    }
    async fn confirm(self, pool: &PgPool, message: u64) -> Result<Receipt, String> {
        if message == 0 {
            return Err("notice receipt has no message id".into());
        }
        let n = sqlx::query("UPDATE tui_output_dispositions SET message_id=$3 WHERE output_key=$1 AND attempt=$2 AND (message_id IS NULL OR message_id=$3)")
            .bind(&self.key).bind(self.id).bind(message.to_string()).execute(pool).await.map_err(|e|e.to_string())?.rows_affected();
        if n != 1 {
            return Err("notice receipt does not name this reservation".into());
        }
        Ok(Receipt {
            key: self.key,
            message,
            reason: self.reason,
            template_version: 1,
        })
    }
}

/// Rehydration only authorizes checking history; it never grants another POST.
#[cfg(test)]
pub(crate) async fn pending(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
) -> Result<Option<Attempt>, String> {
    let row: Option<(serde_json::Value, String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT identity,reason,attempt,message_id FROM tui_output_dispositions WHERE output_key=$1")
        .bind(id.key()).fetch_optional(pool).await.map_err(|e| e.to_string())?;
    let Some((identity, old_reason, attempt, message)) = row else {
        return Ok(None);
    };
    if identity != serde_json::to_value(id).map_err(|e| e.to_string())?
        || old_reason != serde_json::to_string(&reason).map_err(|e| e.to_string())?
    {
        return Err("rejection identity/reason changed".into());
    }
    Ok(attempt
        .filter(|_| message.is_none())
        .map(|attempt| Attempt {
            key: id.key(),
            id: attempt,
            identity: id.clone(),
            reason,
        }))
}
impl Attempt {
    pub(crate) fn channel(&self) -> u64 {
        self.identity.channel
    }
    pub(crate) async fn no_effect(self, pool: &PgPool) -> Result<(), String> {
        let n = sqlx::query("UPDATE tui_output_dispositions SET attempt=NULL WHERE output_key=$1 AND attempt=$2 AND message_id IS NULL")
            .bind(self.key).bind(self.id).execute(pool).await.map_err(|e| e.to_string())?.rows_affected();
        if n != 1 {
            return Err("no-effect proof does not name the reservation".into());
        }
        Ok(())
    }
}

pub(crate) struct Observation {
    pub(crate) message: u64,
    pub(crate) channel: u64,
    pub(crate) author: u64,
    pub(crate) nonce: Option<String>,
    pub(crate) content: String,
}
pub(crate) struct VerifiedNotice {
    attempt: Attempt,
    message: u64,
}
impl Attempt {
    pub(crate) fn verify(self, bot: u64, seen: Observation) -> Result<VerifiedNotice, String> {
        if bot == 0
            || seen.message == 0
            || seen.channel != self.channel()
            || seen.author != bot
            || seen.nonce.as_deref() != Some(self.nonce().as_str())
            || seen.content != self.reason.message()
        {
            return Err("notice response/history does not prove the reserved reason effect".into());
        }
        Ok(VerifiedNotice {
            attempt: self,
            message: seen.message,
        })
    }
}
impl VerifiedNotice {
    pub(crate) async fn commit(self, pool: &PgPool) -> Result<Receipt, String> {
        self.attempt.confirm(pool, self.message).await
    }
}

pub(crate) async fn persist_intent(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    persist_intent_in(&mut tx, id, reason).await?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// The intent commits with the caller's transaction, so it lands together with the release it
/// guards. A key that already holds an intent keeps it; a different identity or reason fails.
pub(crate) async fn persist_intent_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &Identity,
    reason: Reason,
) -> Result<(), String> {
    let identity = serde_json::to_value(id).map_err(|e| e.to_string())?;
    let reason = serde_json::to_string(&reason).map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO tui_output_dispositions(output_key,identity,reason) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(id.key()).bind(&identity).bind(&reason)
        .execute(&mut **tx).await.map_err(|e| e.to_string())?;
    let stored: (serde_json::Value, String, i32) = sqlx::query_as(
        "SELECT identity,reason,template_version FROM tui_output_dispositions WHERE output_key=$1 FOR UPDATE")
        .bind(id.key()).fetch_one(&mut **tx).await.map_err(|e| e.to_string())?;
    if stored != (identity, reason, 1) {
        return Err("rejection identity/reason changed".into());
    }
    Ok(())
}

/// Intents never tried, or freed by a proven no-effect: each may take one more post. An intent
/// holding an attempt keeps it and is never listed here; nothing reconciles it automatically yet.
#[cfg(unix)]
pub(crate) async fn unposted(pool: &PgPool) -> Result<Vec<(Identity, Reason)>, String> {
    let rows: Vec<(serde_json::Value, String)> = sqlx::query_as(
        "SELECT identity,reason FROM tui_output_dispositions WHERE attempt IS NULL AND message_id IS NULL ORDER BY output_key")
        .fetch_all(pool).await.map_err(|e| e.to_string())?;
    let mut intents = Vec::with_capacity(rows.len());
    for (identity, reason) in rows {
        let parsed = serde_json::from_value::<Identity>(identity)
            .ok()
            .zip(serde_json::from_str::<Reason>(&reason).ok());
        match parsed {
            Some(intent) => intents.push(intent),
            None => tracing::warn!("tui rejection intent row is unreadable and stays unposted"),
        }
    }
    Ok(intents)
}

pub(crate) async fn record(
    pool: &PgPool,
    id: &Identity,
) -> Result<Option<(Reason, Option<Receipt>)>, String> {
    let row: Option<(serde_json::Value, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT identity,reason,template_version,message_id FROM tui_output_dispositions WHERE output_key=$1")
        .bind(id.key()).fetch_optional(pool).await.map_err(|e| e.to_string())?;
    let Some((identity, reason, template_version, message)) = row else {
        return Ok(None);
    };
    if identity != serde_json::to_value(id).map_err(|e| e.to_string())? || template_version != 1 {
        return Err("notice receipt identity/template changed".into());
    }
    let reason: Reason = serde_json::from_str(&reason).map_err(|e| e.to_string())?;
    let receipt = message
        .map(|message| {
            let message: u64 = message
                .parse()
                .map_err(|_| "invalid persisted notice receipt")?;
            if message == 0 {
                return Err("invalid persisted notice receipt");
            }
            Ok(Receipt {
                key: id.key(),
                message,
                reason,
                template_version,
            })
        })
        .transpose()?;
    Ok(Some((reason, receipt)))
}

pub(crate) async fn confirmed(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
) -> Result<Option<Receipt>, String> {
    let Some((stored, receipt)) = record(pool, id).await? else {
        return Ok(None);
    };
    if stored != reason {
        return Err("notice receipt reason changed".into());
    }
    Ok(receipt)
}
