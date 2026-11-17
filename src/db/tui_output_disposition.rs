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
        use sha2::{Digest, Sha256};
        format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(self).expect("output identity"))
        )
    }
}
pub(crate) struct Attempt {
    key: String,
    id: Uuid,
}
pub(crate) struct Receipt {
    key: String,
    message: u64,
}
impl Receipt {
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
    let key = id.key();
    let identity = serde_json::to_value(id).map_err(|e| e.to_string())?;
    let reason = serde_json::to_string(&reason).map_err(|e| e.to_string())?;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO tui_output_dispositions(output_key,identity,reason) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(&key).bind(&identity).bind(&reason).execute(&mut *tx).await.map_err(|e|e.to_string())?;
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
    Ok(Some(Attempt { key, id: attempt }))
}
impl Attempt {
    pub(crate) fn nonce(&self) -> String {
        format!("tui-refusal:{}", self.id.simple())
    }
    pub(crate) async fn confirm(self, pool: &PgPool, message: u64) -> Result<Receipt, String> {
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
        })
    }
}
