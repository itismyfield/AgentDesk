use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
};
use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Mutex;

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/decisions";
const MAX_BYTES: usize = 1024 * 1024;
const SAMPLE_BUDGET: Duration = Duration::from_secs(5);
const CAPTURE_BUDGET: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct Record {
    pub(super) request: Value,
    pub(super) response: Option<Value>,
    pub(super) raw_body: Option<String>,
    pub(super) status: u16,
}

struct Inner {
    upstream: Option<reqwest::Client>,
    header: Option<HeaderValue>,
    record: Mutex<Option<Record>>,
    requests: AtomicUsize,
}

pub(super) struct Relay {
    endpoint: String,
    client: reqwest::Client,
    inner: Arc<Inner>,
}

impl Relay {
    pub(super) fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub(super) async fn clear(&self) {
        *self.inner.record.lock().await = None;
    }

    pub(super) async fn record(&self) -> Option<Record> {
        self.inner.record.lock().await.clone()
    }

    pub(super) fn request_count(&self) -> usize {
        self.inner.requests.load(Ordering::SeqCst)
    }

    pub(super) async fn post(&self, body: Value, budget: Duration) -> super::Result<Record> {
        if budget.is_zero() {
            return Err("capture_invalid_budget");
        }
        self.client
            .post(&self.endpoint)
            .json(&body)
            .timeout(budget.min(CAPTURE_BUDGET))
            .send()
            .await
            .map_err(|_| "capture_transport_failed")?;
        self.record().await.ok_or("capture_record_missing")
    }
}

pub(super) async fn start(
    key: String,
    self_test: bool,
) -> super::Result<(Relay, tokio::task::JoinHandle<()>)> {
    let client = || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(1))
            .timeout(CAPTURE_BUDGET)
            .build()
            .map_err(|_| "relay_client_unavailable")
    };
    let (upstream, header) = if self_test {
        (None, None)
    } else {
        let mut header = HeaderValue::from_str(&key).map_err(|_| "invalid_api_key")?;
        header.set_sensitive(true);
        (Some(client()?), Some(header))
    };
    let inner = Arc::new(Inner {
        upstream,
        header,
        record: Mutex::new(None),
        requests: AtomicUsize::new(0),
    });
    let local_client = client()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "relay_bind_failed")?;
    let endpoint = format!(
        "http://{}/v1beta/decisions",
        listener.local_addr().map_err(|_| "relay_address_failed")?
    );
    let app = Router::new()
        .route("/v1beta/decisions", post(receive))
        .layer(DefaultBodyLimit::max(MAX_BYTES))
        .with_state(inner.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((
        Relay {
            endpoint,
            client: local_client,
            inner,
        },
        task,
    ))
}

async fn receive(State(inner): State<Arc<Inner>>, bytes: Bytes) -> (StatusCode, Json<Value>) {
    let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
        return (StatusCode::BAD_REQUEST, Json(json!({})));
    };
    let index = inner.requests.fetch_add(1, Ordering::SeqCst);
    *inner.record.lock().await = Some(Record {
        request: request.clone(),
        response: None,
        raw_body: None,
        status: 0,
    });
    let (status, response, raw_body) = match (&inner.upstream, &inner.header) {
        (Some(client), Some(header)) => forward(client, header, &request).await,
        _ => {
            let (status, response) = synthetic(&request, index);
            let raw_body = response
                .as_ref()
                .and_then(|body| serde_json::to_string(body).ok());
            (status, response, raw_body)
        }
    };
    let mut slot = inner.record.lock().await;
    if inner.requests.load(Ordering::SeqCst) == index + 1 && slot.is_some() {
        *slot = Some(Record {
            request,
            response: response.clone(),
            raw_body,
            status,
        });
    }
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(response.unwrap_or_else(|| json!({}))))
}

async fn forward(
    client: &reqwest::Client,
    header: &HeaderValue,
    request: &Value,
) -> (u16, Option<Value>, Option<String>) {
    let budget = if is_capture(request) {
        CAPTURE_BUDGET
    } else {
        SAMPLE_BUDGET
    };
    let result = async {
        let mut response = client
            .post(ENDPOINT)
            .header("x-goog-api-key", header.clone())
            .json(request)
            .timeout(budget)
            .send()
            .await
            .map_err(|_| ())?;
        let status = response.status().as_u16();
        // Error bodies and all upstream headers stay outside the evidence record.
        if !response.status().is_success() {
            return Ok((status, None, None));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BYTES as u64)
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let raw_body = String::from_utf8(bytes).map_err(|_| ())?;
        let body = serde_json::from_str(&raw_body).map_err(|_| ())?;
        Ok::<_, ()>((status, Some(body), Some(raw_body)))
    };
    tokio::time::timeout(budget, result)
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .unwrap_or((502, None, None))
}

fn is_capture(request: &Value) -> bool {
    request["questions"]
        .as_object()
        .is_some_and(|questions| questions.len() == 4)
}

fn synthetic(request: &Value, index: usize) -> (u16, Option<Value>) {
    if is_capture(request) {
        let fixture: Value = match serde_json::from_str(include_str!(
            "../../tests/fixtures/decision_gate/comment_shape.json"
        )) {
            Ok(fixture) => fixture,
            Err(_) => return (502, None),
        };
        return (
            200,
            Some(json!({
                "answers": {
                    "q_noul": fixture["noul"]["answers"]["q"],
                    "q_choice_current": fixture["choice"]["answers"]["q"],
                    "q_choice_255": fixture["choice_255"]["answers"]["q"],
                    "q_score": fixture["score"]["answers"]["q"]
                },
                "usage": {"total_input_tokens": 209}
            })),
        );
    }
    let Some(criteria) = request["questions"]["q"]["criteria"].as_object() else {
        return (400, None);
    };
    let Some(candidate) = criteria.keys().find(|key| key.as_str() != "none") else {
        return (400, None);
    };
    let phase = index.saturating_sub(1) % 6;
    if phase == 4 {
        return (401, None);
    }
    let (probability, confidence) = match phase {
        0 => (0.8, 0.1),
        1 => (0.799999, 0.999),
        2 => (0.05, 0.9),
        _ => (0.95, 0.2),
    };
    let mut probabilities: serde_json::Map<String, Value> = criteria
        .keys()
        .map(|key| {
            let value = if key == candidate {
                probability
            } else if key == "none" {
                1.0 - probability
            } else {
                0.0
            };
            (key.clone(), json!(value))
        })
        .collect();
    if phase == 3 {
        probabilities.remove(candidate);
    }
    let mut body = json!({
        "answers": {"q": {
            "type": "choice", "choice": if phase == 2 { "none" } else { candidate.as_str() },
            "confidence": confidence, "probabilities": probabilities
        }},
        "usage": {"total_input_tokens": 100 + criteria.len()}
    });
    if phase == 5 {
        body["provider_debug"] = json!(super::SENTINEL);
    }
    (200, Some(body))
}
