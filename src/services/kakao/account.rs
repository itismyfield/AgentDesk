//! One credential owner per account, shared by messages and calendar work.
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use super::{KakaoClient, KakaoEnvironment, KakaoError, configured_accounts, parse_enabled};

pub(crate) fn calendar_enabled() -> Result<bool, KakaoError> {
    parse_enabled(
        std::env::var("AGENTDESK_KAKAO_CALENDAR_ENABLED")
            .ok()
            .as_deref(),
    )
}

pub(crate) fn calendar_accounts() -> Result<Vec<String>, KakaoError> {
    if !calendar_enabled()? {
        return Err(KakaoError::Disabled);
    }
    // Separate allowlist: enabling calendar never implicitly grants all message accounts.
    let raw = std::env::var("AGENTDESK_KAKAO_CALENDAR_ACCOUNTS")
        .map_err(|_| KakaoError::InvalidConfiguration("calendar account allowlist is required"))?;
    let allowed = configured_accounts(Some(&raw))?;
    let configured = configured_accounts(std::env::var(super::ACCOUNTS_ENV).ok().as_deref())?;
    if !allowed.is_subset(&configured) {
        return Err(KakaoError::UnknownAccount);
    }
    Ok(allowed.into_iter().collect())
}

pub(crate) fn authorize_calendar(account: &str) -> Result<(), KakaoError> {
    if !calendar_accounts()?.iter().any(|id| id == account) {
        return Err(KakaoError::UnknownAccount);
    }
    Ok(())
}

pub(crate) async fn shared_client(
    account: &str,
    calendar: bool,
) -> Result<Arc<KakaoClient>, KakaoError> {
    let environment = if calendar {
        authorize_calendar(account)?;
        KakaoEnvironment::auth_from_process(Some(account))?
    } else {
        KakaoEnvironment::from_process(Some(account))?
    };
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<KakaoClient>>>> = OnceLock::new();
    let mut clients = CLIENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .await;
    if let Some(client) = clients.get(account) {
        return Ok(client.clone());
    }
    let client = Arc::new(KakaoClient::from_environment(environment)?);
    clients.insert(account.to_string(), client.clone());
    Ok(client)
}
