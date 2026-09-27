//! Speech-to-text and text-to-speech over the OpenAI audio API shape
//! (`/audio/transcriptions`, `/audio/speech`). Works with OpenAI itself and
//! with local servers that speak the same API, so swapping a model is a config
//! change.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
});

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct OpenAiCompatEndpoint {
    /// Base URL including the version prefix, e.g. `http://127.0.0.1:8000/v1`.
    pub base_url: String,
    pub model: String,
    /// Environment variable holding the bearer token; empty for local servers.
    pub api_key_env: String,
}

impl OpenAiCompatEndpoint {
    fn url(&self, path: &str) -> Result<String> {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            bail!("voice openai_compatible.base_url is not set");
        }
        Ok(format!("{base}/{path}"))
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let key_env = self.api_key_env.trim();
        match (!key_env.is_empty())
            .then(|| std::env::var(key_env).ok())
            .flatten()
        {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }
}

pub(crate) async fn transcribe(
    endpoint: &OpenAiCompatEndpoint,
    audio_path: &Path,
    language: &str,
) -> Result<String> {
    let audio = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("read audio {}", audio_path.display()))?;
    let boundary = format!("agentdesk-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::with_capacity(audio.len() + 512);
    for (name, value) in [
        ("model", endpoint.model.as_str()),
        ("language", language),
        ("response_format", "json"),
    ] {
        if value.trim().is_empty() {
            continue;
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let request = CLIENT
        .post(endpoint.url("audio/transcriptions")?)
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body);
    let response = endpoint
        .authorize(request)
        .send()
        .await
        .context("send transcription request")?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "transcription request failed with {status}: {}",
            response.text().await.unwrap_or_default()
        );
    }
    #[derive(Deserialize)]
    struct Transcription {
        text: String,
    }
    let transcription: Transcription = response
        .json()
        .await
        .context("parse transcription response")?;
    Ok(transcription.text)
}

/// Writes mp3 audio for `text` to a new file under `temp_dir`.
pub(crate) async fn synthesize(
    endpoint: &OpenAiCompatEndpoint,
    voice: &str,
    text: &str,
    temp_dir: &Path,
    file_prefix: &str,
) -> Result<PathBuf> {
    let request = CLIENT
        .post(endpoint.url("audio/speech")?)
        .json(&serde_json::json!({
            "model": endpoint.model,
            "voice": voice,
            "input": text,
            "response_format": "mp3",
        }));
    let response = endpoint
        .authorize(request)
        .send()
        .await
        .context("send speech request")?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "speech request failed with {status}: {}",
            response.text().await.unwrap_or_default()
        );
    }
    let audio = response.bytes().await.context("read speech audio")?;
    if audio.is_empty() {
        bail!("speech request returned empty audio");
    }
    tokio::fs::create_dir_all(temp_dir)
        .await
        .with_context(|| format!("create TTS temp dir {}", temp_dir.display()))?;
    let path = temp_dir.join(format!(
        "{file_prefix}{}-{}.mp3",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, &audio)
        .await
        .with_context(|| format!("write speech audio {}", path.display()))?;
    Ok(path)
}
