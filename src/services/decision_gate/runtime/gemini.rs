use super::super::types::{DecisionRequest, DecisionResult, NoAnswerReason};
use super::{DecisionGate, parse_response};
use serde_json::Value;
use std::time::Duration;

pub(super) const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/decisions";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

impl DecisionGate {
    pub(super) async fn send(
        &self,
        request: &DecisionRequest,
        body: Value,
        budget: Duration,
    ) -> Result<(DecisionResult, Option<u64>), NoAnswerReason> {
        let client = self
            .client
            .as_ref()
            .ok_or(NoAnswerReason::ClientUnavailable)?;
        let header = self
            .key
            .as_ref()
            .and_then(|key| key.header())
            .ok_or(NoAnswerReason::ClientUnavailable)?;
        let mut response = client
            .post(&self.endpoint)
            .header("x-goog-api-key", header)
            .json(&body)
            .timeout(budget)
            .send()
            .await
            .map_err(classify)?;
        match response.status().as_u16() {
            401 | 403 => return Err(NoAnswerReason::Authentication),
            429 => return Err(NoAnswerReason::RateLimited),
            _ if !response.status().is_success() => return Err(NoAnswerReason::ProviderError),
            _ => {}
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(NoAnswerReason::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(classify)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(NoAnswerReason::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: Value =
            serde_json::from_slice(&bytes).map_err(|_| NoAnswerReason::InvalidResponse)?;
        let result = parse_response(&request.question, &data)?;
        let tokens = data.get("usage").and_then(|usage| {
            usage
                .get("total_input_tokens")
                .and_then(Value::as_u64)
                .or_else(|| usage.get("totalInputTokens").and_then(Value::as_u64))
        });
        Ok((result, tokens))
    }
}

fn classify(error: reqwest::Error) -> NoAnswerReason {
    if error.is_timeout() {
        NoAnswerReason::Timeout
    } else {
        NoAnswerReason::Transport
    }
}
