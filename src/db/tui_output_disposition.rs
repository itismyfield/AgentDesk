//! A rejection reservation survives restart; only its exact receipt or no-effect proof settles it.
use sqlx::PgPool;
use uuid::Uuid;
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
    let identity = serde_json::to_value(id).map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO tui_output_dispositions(output_key,identity,reason) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(id.key()).bind(identity).bind(serde_json::to_string(&reason).map_err(|e| e.to_string())?)
        .execute(pool).await.map_err(|e| e.to_string())?;
    if record(pool, id)
        .await?
        .is_none_or(|(stored, _)| stored != reason)
    {
        return Err("rejection identity/reason changed".into());
    }
    Ok(())
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
