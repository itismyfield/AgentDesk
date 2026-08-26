//! Kakao Talk delivery client with fail-closed environment configuration.
//!
//! This module deliberately owns only provider transport. Reservation,
//! idempotency, and retry policy belong to the scheduled external-delivery
//! outbox so other message providers can reuse those guarantees.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::Mutex;

use super::kakao_message::{
    KakaoMessage, KakaoMessageValidationError, default_template, validate_message,
    validate_public_https_url,
};

const TOKEN_URL: &str = "https://kauth.kakao.com/oauth/token";
const FRIEND_SEND_URL: &str =
    "https://kapi.kakao.com/v1/api/talk/friends/message/default/send";
const SELF_SEND_URL: &str = "https://kapi.kakao.com/v2/api/talk/memo/default/send";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);
const RESPONSE_MAX_BYTES: usize = 64 * 1024;

const ENABLED_ENV: &str = "AGENTDESK_KAKAO_ENABLED";
const ACCOUNTS_ENV: &str = "AGENTDESK_KAKAO_ACCOUNTS";
const DEFAULT_ACCOUNT_ENV: &str = "AGENTDESK_KAKAO_DEFAULT_ACCOUNT";
const LANDING_URL_ENV: &str = "AGENTDESK_KAKAO_LANDING_URL";

#[derive(Debug, Error)]
pub enum KakaoError {
    #[error("Kakao delivery is disabled")]
    Disabled,
    #[error("Kakao delivery configuration is invalid: {0}")]
    InvalidConfiguration(&'static str),
    #[error("Kakao account is not configured")]
    UnknownAccount,
    #[error("Kakao credentials are incomplete for the selected account")]
    MissingCredentials,
    #[error(transparent)]
    InvalidMessage(#[from] KakaoMessageValidationError),
    #[error("Kakao friend recipients are invalid")]
    InvalidRecipients,
    #[error("Kakao authorization must be renewed")]
    ReauthorizationRequired,
    #[error("Kakao consent does not permit this delivery")]
    ConsentRequired,
    #[error("Kakao rejected the delivery with HTTP {0}")]
    ProviderRejected(u16),
    #[error("Kakao rejected the delivery with result code {0}")]
    ProviderResult(i64),
    #[error("Kakao delivery result is ambiguous; automatic retry is unsafe")]
    DeliveryUnknown,
    #[error("failed to initialize Kakao HTTP transport")]
    TransportInitialization,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KakaoEnvironment {
    pub account_id: String,
    pub landing_url: String,
    env_prefix: String,
}

impl KakaoEnvironment {
    pub fn from_process(account_id: Option<&str>) -> Result<Self, KakaoError> {
        if !parse_enabled(std::env::var(ENABLED_ENV).ok().as_deref())? {
            return Err(KakaoError::Disabled);
        }
        let accounts = configured_accounts(std::env::var(ACCOUNTS_ENV).ok().as_deref())?;
        let default_account = std::env::var(DEFAULT_ACCOUNT_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        validate_account_id(&default_account)?;
        let account_id = account_id.unwrap_or(&default_account);
        validate_account_id(account_id)?;
        if !accounts.contains(account_id) {
            return Err(KakaoError::UnknownAccount);
        }
        let landing_url = std::env::var(LANDING_URL_ENV)
            .map_err(|_| KakaoError::InvalidConfiguration("landing URL is missing"))?;
        validate_public_https_url(&landing_url, "landing_url")?;
        Ok(Self {
            account_id: account_id.to_string(),
            landing_url,
            env_prefix: account_env_prefix(account_id),
        })
    }

    pub fn default_account() -> Result<String, KakaoError> {
        Self::from_process(None).map(|config| config.account_id)
    }

    fn credential(&self, suffix: &str) -> Option<String> {
        std::env::var(format!("{}_{suffix}", self.env_prefix))
            .ok()
            .filter(|value| !value.trim().is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KakaoDeliverySummary {
    pub requested_count: usize,
    pub successful_count: usize,
    pub failed_count: usize,
}

struct TokenState {
    access_token: Option<String>,
    refresh_token: Option<String>,
    access_expires_at: Option<Instant>,
    refreshed_once: bool,
}

pub struct KakaoClient {
    http: reqwest::Client,
    environment: KakaoEnvironment,
    rest_api_key: Option<String>,
    client_secret: Option<String>,
    tokens: Mutex<TokenState>,
}

impl KakaoClient {
    pub fn from_process(account_id: Option<&str>) -> Result<Self, KakaoError> {
        let environment = KakaoEnvironment::from_process(account_id)?;
        let rest_api_key = environment.credential("REST_API_KEY");
        let client_secret = environment.credential("CLIENT_SECRET");
        let access_token = environment.credential("ACCESS_TOKEN");
        let refresh_token = environment.credential("REFRESH_TOKEN");
        if access_token.is_none() && (refresh_token.is_none() || rest_api_key.is_none()) {
            return Err(KakaoError::MissingCredentials);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|_| KakaoError::TransportInitialization)?;
        Ok(Self {
            http,
            environment,
            rest_api_key,
            client_secret,
            tokens: Mutex::new(TokenState {
                access_token,
                refresh_token,
                access_expires_at: None,
                refreshed_once: false,
            }),
        })
    }

    pub fn account_id(&self) -> &str {
        &self.environment.account_id
    }

    pub async fn send_to_friends(
        &self,
        receiver_uuids: &[String],
        message: &KakaoMessage,
    ) -> Result<KakaoDeliverySummary, KakaoError> {
        validate_recipients(receiver_uuids)?;
        validate_message(message)?;
        let receiver_uuids_json =
            serde_json::to_string(receiver_uuids).map_err(|_| KakaoError::InvalidRecipients)?;
        let form = vec![
            ("receiver_uuids", receiver_uuids_json),
            (
                "template_object",
                default_template(message, &self.environment.landing_url),
            ),
        ];
        let response: FriendSendResponse = self.authorized_form(FRIEND_SEND_URL, &form).await?;
        classify_friend_response(receiver_uuids, response)
    }

    pub async fn send_to_self(
        &self,
        message: &KakaoMessage,
    ) -> Result<KakaoDeliverySummary, KakaoError> {
        validate_message(message)?;
        let form = vec![(
            "template_object",
            default_template(message, &self.environment.landing_url),
        )];
        let response: SelfSendResponse = self.authorized_form(SELF_SEND_URL, &form).await?;
        if response.result_code != 0 {
            return Err(KakaoError::ProviderResult(response.result_code));
        }
        Ok(KakaoDeliverySummary {
            requested_count: 1,
            successful_count: 1,
            failed_count: 0,
        })
    }

    async fn authorized_form<T: DeserializeOwned>(
        &self,
        url: &'static str,
        form: &[(&'static str, String)],
    ) -> Result<T, KakaoError> {
        let token = self.access_token(false).await?;
        let mut response = self
            .http
            .post(url)
            .bearer_auth(&token)
            .form(form)
            .send()
            .await
            .map_err(|_| KakaoError::DeliveryUnknown)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.access_token(true).await?;
            response = self
                .http
                .post(url)
                .bearer_auth(&token)
                .form(form)
                .send()
                .await
                .map_err(|_| KakaoError::DeliveryUnknown)?;
        }
        match response.status() {
            reqwest::StatusCode::UNAUTHORIZED => Err(KakaoError::ReauthorizationRequired),
            reqwest::StatusCode::FORBIDDEN => Err(KakaoError::ConsentRequired),
            status if status.is_client_error() => {
                Err(KakaoError::ProviderRejected(status.as_u16()))
            }
            status if !status.is_success() => Err(KakaoError::DeliveryUnknown),
            _ => read_bounded_json(response).await,
        }
    }

    async fn access_token(&self, force_refresh: bool) -> Result<String, KakaoError> {
        let mut tokens = self.tokens.lock().await;
        let refresh_due = tokens.refresh_token.is_some()
            && self.rest_api_key.is_some()
            && (!tokens.refreshed_once
                || tokens
                    .access_expires_at
                    .is_some_and(|expires_at| expires_at <= Instant::now()));
        if force_refresh || refresh_due {
            self.refresh_locked(&mut tokens).await?;
        }
        tokens
            .access_token
            .clone()
            .ok_or(KakaoError::MissingCredentials)
    }

    async fn refresh_locked(&self, tokens: &mut TokenState) -> Result<(), KakaoError> {
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or(KakaoError::ReauthorizationRequired)?;
        let rest_api_key = self
            .rest_api_key
            .as_deref()
            .ok_or(KakaoError::MissingCredentials)?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("client_id", rest_api_key.to_string()),
            ("refresh_token", refresh_token),
        ];
        if let Some(client_secret) = self.client_secret.as_deref() {
            form.push(("client_secret", client_secret.to_string()));
        }
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|_| KakaoError::ReauthorizationRequired)?;
        if !response.status().is_success() {
            return Err(KakaoError::ReauthorizationRequired);
        }
        let refreshed: RefreshResponse = read_bounded_json(response)
            .await
            .map_err(|_| KakaoError::ReauthorizationRequired)?;
        tokens.access_token = Some(refreshed.access_token);
        if let Some(refresh_token) = refreshed.refresh_token {
            tokens.refresh_token = Some(refresh_token);
        }
        let lifetime = Duration::from_secs(refreshed.expires_in);
        tokens.access_expires_at =
            Some(Instant::now() + lifetime.saturating_sub(TOKEN_REFRESH_SKEW));
        tokens.refreshed_once = true;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FriendSendResponse {
    #[serde(default)]
    successful_receiver_uuids: Vec<String>,
    #[serde(default)]
    failure_info: Vec<FriendFailure>,
}

#[derive(Debug, Deserialize)]
struct FriendFailure {
    #[serde(default)]
    receiver_uuids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SelfSendResponse {
    result_code: i64,
}

fn parse_enabled(raw: Option<&str>) -> Result<bool, KakaoError> {
    let normalized = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    match normalized.as_deref() {
        None | Some("0" | "false" | "no" | "off") => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some(_) => Err(KakaoError::InvalidConfiguration(
            "AGENTDESK_KAKAO_ENABLED must be a boolean",
        )),
    }
}

fn configured_accounts(raw: Option<&str>) -> Result<BTreeSet<String>, KakaoError> {
    let raw = raw.unwrap_or("default");
    let accounts = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            validate_account_id(value)?;
            Ok(value.to_string())
        })
        .collect::<Result<BTreeSet<_>, KakaoError>>()?;
    if accounts.is_empty() || accounts.len() > 16 {
        return Err(KakaoError::InvalidConfiguration(
            "one to sixteen Kakao accounts must be configured",
        ));
    }
    Ok(accounts)
}

fn validate_account_id(account_id: &str) -> Result<(), KakaoError> {
    if account_id.is_empty()
        || account_id.len() > 32
        || !account_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
        })
    {
        return Err(KakaoError::InvalidConfiguration(
            "Kakao account ids must use lowercase letters, digits, or hyphens",
        ));
    }
    Ok(())
}

fn account_env_prefix(account_id: &str) -> String {
    if account_id == "default" {
        "KAKAO".to_string()
    } else {
        format!("KAKAO_{}", account_id.replace('-', "_").to_ascii_uppercase())
    }
}

pub fn validate_recipients(receiver_uuids: &[String]) -> Result<(), KakaoError> {
    if receiver_uuids.is_empty() || receiver_uuids.len() > 5 {
        return Err(KakaoError::InvalidRecipients);
    }
    let unique = receiver_uuids.iter().collect::<BTreeSet<_>>();
    if unique.len() != receiver_uuids.len()
        || receiver_uuids.iter().any(|value| {
            value.is_empty()
                || value.len() > 128
                || !value.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        return Err(KakaoError::InvalidRecipients);
    }
    Ok(())
}

fn classify_friend_response(
    requested: &[String],
    response: FriendSendResponse,
) -> Result<KakaoDeliverySummary, KakaoError> {
    let requested = requested.iter().collect::<BTreeSet<_>>();
    let successful = response
        .successful_receiver_uuids
        .iter()
        .collect::<BTreeSet<_>>();
    let failed = response
        .failure_info
        .iter()
        .flat_map(|failure| failure.receiver_uuids.iter())
        .collect::<BTreeSet<_>>();
    let failed_occurrences = response
        .failure_info
        .iter()
        .map(|failure| failure.receiver_uuids.len())
        .sum::<usize>();
    if successful.len() != response.successful_receiver_uuids.len()
        || failed.len() != failed_occurrences
        || !successful.is_disjoint(&failed)
        || !successful.is_subset(&requested)
        || !failed.is_subset(&requested)
        || successful.len() + failed.len() != requested.len()
    {
        return Err(KakaoError::DeliveryUnknown);
    }
    Ok(KakaoDeliverySummary {
        requested_count: requested.len(),
        successful_count: successful.len(),
        failed_count: failed.len(),
    })
}

async fn read_bounded_json<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, KakaoError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| KakaoError::DeliveryUnknown)?;
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_MAX_BYTES {
            return Err(KakaoError::DeliveryUnknown);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| KakaoError::DeliveryUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_is_disabled_by_default_and_parses_explicit_values() {
        assert!(!parse_enabled(None).unwrap());
        assert!(parse_enabled(Some("true")).unwrap());
        assert!(parse_enabled(Some("TRUE")).unwrap());
        assert!(!parse_enabled(Some("off")).unwrap());
        assert!(parse_enabled(Some("sometimes")).is_err());
    }

    #[test]
    fn account_ids_map_to_stable_non_colliding_environment_prefixes() {
        assert_eq!(account_env_prefix("default"), "KAKAO");
        assert_eq!(account_env_prefix("work-bot"), "KAKAO_WORK_BOT");
        assert!(validate_account_id("work_bot").is_err());
        assert!(validate_account_id("Work").is_err());
    }

    #[test]
    fn configured_accounts_are_bounded_and_deduplicated() {
        let accounts = configured_accounts(Some("default,work,work")).unwrap();
        assert_eq!(
            accounts.into_iter().collect::<Vec<_>>(),
            vec!["default".to_string(), "work".to_string()]
        );
        assert!(configured_accounts(Some("")).is_err());
    }

    #[test]
    fn recipients_require_one_to_five_unique_printable_ids() {
        let valid = vec!["friend-a".to_string(), "friend-b".to_string()];
        assert!(validate_recipients(&valid).is_ok());
        assert!(validate_recipients(&[]).is_err());
        assert!(validate_recipients(&["same".to_string(), "same".to_string()]).is_err());
        assert!(validate_recipients(&vec!["friend".to_string(); 6]).is_err());
    }

    #[test]
    fn friend_response_must_partition_the_requested_recipients() {
        let requested = vec!["a".to_string(), "b".to_string()];
        let summary = classify_friend_response(
            &requested,
            FriendSendResponse {
                successful_receiver_uuids: vec!["a".to_string()],
                failure_info: vec![FriendFailure {
                    receiver_uuids: vec!["b".to_string()],
                }],
            },
        )
        .unwrap();
        assert_eq!(summary.successful_count, 1);
        assert_eq!(summary.failed_count, 1);

        let ambiguous = FriendSendResponse {
            successful_receiver_uuids: vec!["a".to_string()],
            failure_info: Vec::new(),
        };
        assert!(classify_friend_response(&requested, ambiguous).is_err());
    }
}
