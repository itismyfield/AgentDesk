//! Fixed-endpoint Kakao calendar adapter. No arbitrary credential-bearing URLs.
use super::{KakaoClient, KakaoError, read_bounded_json};
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    pub app_id: i64,
    pub user_id: i64,
}

/// Recovery accepts only a full, owned primary-calendar detail matching the saved intent.
/// A partial provider response is insufficient evidence and must leave the job unresolved.
pub(crate) fn matches_adoption(detail: &Value, id: &str, desired: &Value) -> bool {
    let remote = detail.get("event").unwrap_or(detail);
    if remote.get("id").and_then(Value::as_str) != Some(id)
        || remote.get("calendar_id").and_then(Value::as_str) != Some("primary")
        || remote.get("is_host").and_then(Value::as_bool) != Some(true)
        || remote.get("title") != desired.get("title")
        || remote.get("rrule").is_some_and(|v| !v.is_null())
        || remote.get("recurrence").is_some_and(|v| !v.is_null())
        || remote["time"]["is_all_day"].as_bool() != Some(false)
        || remote["time"]["time_zone"] != desired["time"]["time_zone"]
    {
        return false;
    }
    for field in ["start_at", "end_at"] {
        let parse = |value: &Value| {
            value
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        };
        let expected = parse(&desired["time"][field]);
        if expected.is_none() || parse(&remote["time"][field]) != expected {
            return false;
        }
    }
    if remote
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        != desired
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
        || remote["location"]["name"].as_str().unwrap_or("")
            != desired["location"]["name"].as_str().unwrap_or("")
    {
        return false;
    }
    // Omitted reminders intentionally retain provider defaults, which cannot be inferred locally.
    if let Some(expected) = desired.get("reminders") {
        let sorted = |value: &Value| -> Option<Vec<i64>> {
            let mut values = value
                .as_array()?
                .iter()
                .map(Value::as_i64)
                .collect::<Option<Vec<_>>>()?;
            values.sort_unstable();
            Some(values)
        };
        if sorted(expected).is_none() || sorted(&remote["reminders"]) != sorted(expected) {
            return false;
        }
    }
    true
}

impl KakaoClient {
    async fn calendar_request(
        &self,
        method: Method,
        url: &'static str,
        fields: &[(&'static str, String)],
    ) -> Result<Value, KakaoError> {
        let response = self.authorized_response(method, url, fields).await?;
        match response.status().as_u16() {
            401 => Err(KakaoError::ReauthorizationRequired),
            403 => Err(KakaoError::ConsentRequired),
            429 => Err(KakaoError::ProviderRejected(429)),
            code if (400..500).contains(&code) => {
                let body: Value = read_bounded_json(response).await?;
                match body.get("code").and_then(Value::as_i64) {
                    Some(code) => Err(KakaoError::ProviderResult(code)),
                    None => Err(KakaoError::ProviderRejected(code)),
                }
            }
            code if (200..300).contains(&code) => read_bounded_json(response).await,
            _ => Err(KakaoError::DeliveryUnknown),
        }
    }

    pub(crate) async fn calendar_identity(&self) -> Result<Identity, KakaoError> {
        #[derive(Deserialize)]
        struct TokenInfo {
            id: i64,
            app_id: i64,
            expires_in: i64,
        }
        #[derive(Deserialize)]
        struct Scope {
            id: String,
            agreed: bool,
        }
        #[derive(Deserialize)]
        struct Scopes {
            id: i64,
            scopes: Vec<Scope>,
        }
        let info: TokenInfo = serde_json::from_value(
            self.calendar_request(
                Method::GET,
                "https://kapi.kakao.com/v1/user/access_token_info",
                &[],
            )
            .await?,
        )
        .map_err(|_| KakaoError::DeliveryUnknown)?;
        let scopes: Scopes = serde_json::from_value(
            self.calendar_request(
                Method::GET,
                "https://kapi.kakao.com/v2/user/scopes",
                &[("scopes", "[\"talk_calendar\"]".into())],
            )
            .await?,
        )
        .map_err(|_| KakaoError::DeliveryUnknown)?;
        if info.id != scopes.id || info.id <= 0 || info.app_id <= 0 || info.expires_in <= 0 {
            return Err(KakaoError::BindingChanged);
        }
        if !scopes
            .scopes
            .iter()
            .any(|scope| scope.id == "talk_calendar" && scope.agreed)
        {
            return Err(KakaoError::ConsentRequired);
        }
        Ok(Identity {
            app_id: info.app_id,
            user_id: info.id,
        })
    }

    pub(crate) async fn calendar_create(&self, event: &Value) -> Result<String, KakaoError> {
        let result = self
            .calendar_request(
                Method::POST,
                "https://kapi.kakao.com/v2/api/calendar/create/event",
                &[
                    ("calendar_id", "primary".into()),
                    ("event", event.to_string()),
                ],
            )
            .await?;
        result
            .get("event_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .map(str::to_string)
            .ok_or(KakaoError::DeliveryUnknown)
    }

    pub(crate) async fn calendar_update(&self, id: &str, event: &Value) -> Result<(), KakaoError> {
        let result = self
            .calendar_request(
                Method::POST,
                "https://kapi.kakao.com/v2/api/calendar/update/event/host",
                &[("event_id", id.into()), ("event", event.to_string())],
            )
            .await?;
        if result.get("event_id").and_then(Value::as_str) != Some(id) {
            return Err(KakaoError::DeliveryUnknown);
        }
        Ok(())
    }

    pub(crate) async fn calendar_delete(&self, id: &str) -> Result<(), KakaoError> {
        let result = self
            .calendar_request(
                Method::DELETE,
                "https://kapi.kakao.com/v2/api/calendar/delete/event",
                &[("event_id", id.into())],
            )
            .await?;
        if result.get("event_id").and_then(Value::as_str) != Some(id) {
            return Err(KakaoError::DeliveryUnknown);
        }
        Ok(())
    }

    pub(crate) async fn calendar_detail(&self, id: &str) -> Result<Value, KakaoError> {
        self.calendar_request(
            Method::GET,
            "https://kapi.kakao.com/v2/api/calendar/event",
            &[("event_id", id.into())],
        )
        .await
    }
}
