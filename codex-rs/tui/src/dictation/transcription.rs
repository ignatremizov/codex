//! Browser-authenticated dictation upload, independent of capture and composer lifetimes.
//!
//! One prepared transport pins the recording's credential manager, account, endpoint and routing.
//! Dropping preparation/upload cancels its async work; this module never acquires a microphone.

use super::RecordedAudio;
use crate::legacy_core::config::Config;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthCredentialsStoreMode;
use codex_login::ChatgptAuthSession;
use codex_login::CodexAuth;
use codex_protocol::auth::AuthMode;
use serde::Deserialize;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const UPLOAD_RATE: u32 = 24_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 120);
const BROWSER_AUTH_REQUIRED: &str =
    "Dictation requires browser ChatGPT login auth. Run `codex login`.";

pub(crate) struct PreparedTranscription {
    auth: ChatgptAuthSession,
    account_id: String,
    endpoint: String,
    http: RouteAwareClientPool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TranscriptionFailure {
    #[error("Dictation transcription was canceled.")]
    Canceled,
    #[error("Dictation transcription timed out.")]
    TimedOut,
    #[error("{0}")]
    Failed(String),
}

impl From<String> for TranscriptionFailure {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

// Deliberately no Debug: credentials must never enter diagnostics.
struct UploadCredentials {
    bearer: String,
    account_id: String,
    fedramp: bool,
}

/// Supplies credentials and refresh for one captured source.
///
/// The production adapter always delegates to the retained manager. Test implementations may
/// simulate an authority without overriding process environment or exposing a public test API.
trait TranscriptionAuth: Sync {
    fn credentials(&self) -> impl Future<Output = Result<UploadCredentials, String>> + Send;
    fn refresh(&self) -> impl Future<Output = Result<(), String>> + Send;
}

impl TranscriptionAuth for ChatgptAuthSession {
    async fn credentials(&self) -> Result<UploadCredentials, String> {
        browser_credentials(self.auth().await.map_err(|error| error.to_string())?)
    }

    async fn refresh(&self) -> Result<(), String> {
        self.refresh_token()
            .await
            .map_err(|error| error.to_string())
    }
}

impl PreparedTranscription {
    /// Called once before capture. The caller can cancel this future before acquiring a device.
    pub(crate) async fn prepare(config: &Config) -> Result<Self, String> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            validate_browser_source(config)?;
            let auth = ChatgptAuthSession::from_config(config)
                .await
                .map_err(|error| format!("Failed to initialize dictation auth: {error}"))?;
            let credentials = auth.credentials().await?;
            let http =
                RouteAwareClientPool::with_chatgpt_cloudflare_cookies_without_request_logging(
                    config.http_client_factory(),
                    ClientRouteClass::Api,
                )
                .with_legacy_custom_ca_fallback();
            Ok(Self {
                auth,
                account_id: credentials.account_id,
                endpoint: format!(
                    "{}/transcribe",
                    config.chatgpt_base_url.trim_end_matches('/')
                ),
                http,
            })
        })
        .await
        .map_err(|_| "Dictation authentication timed out.".to_string())?
    }

    pub(crate) async fn transcribe(
        &self,
        audio: RecordedAudio,
        cancellation: CancellationToken,
    ) -> Result<String, TranscriptionFailure> {
        upload_with_deadline(
            &self.auth,
            &self.http,
            &self.endpoint,
            &self.account_id,
            audio,
            cancellation,
            OPERATION_TIMEOUT,
        )
        .await
    }
}

fn validate_browser_source(config: &Config) -> Result<(), String> {
    if codex_login::read_codex_access_token_from_env().is_some()
        || codex_login::is_workload_identity_selected()
    {
        return Err(BROWSER_AUTH_REQUIRED.into());
    }
    // These two reads and manager construction must share the captured profile selection when
    // profile-aware storage is introduced. Never replace them with per-chunk global selection.
    let external = codex_login::load_auth_dot_json(
        config.codex_home.as_path(),
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|error| format!("Failed to inspect dictation auth: {error}"))?;
    if external.is_some() {
        return Err("Dictation does not support external authentication tokens.".into());
    }
    let stored = codex_login::load_auth_dot_json(
        config.codex_home.as_path(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|error| format!("Failed to inspect dictation auth: {error}"))?
    .ok_or_else(|| BROWSER_AUTH_REQUIRED.to_string())?;
    if !matches!(stored.auth_mode, Some(AuthMode::Chatgpt) | None)
        || stored.openai_api_key.is_some()
        || stored.agent_identity.is_some()
        || stored.personal_access_token.is_some()
        || stored.bedrock_api_key.is_some()
        || stored.bedrock_access_keys.is_some()
        || stored.tokens.is_none()
    {
        return Err(BROWSER_AUTH_REQUIRED.into());
    }
    Ok(())
}

fn browser_credentials(auth: Option<CodexAuth>) -> Result<UploadCredentials, String> {
    let auth = match auth {
        Some(auth @ CodexAuth::Chatgpt(_)) => auth,
        Some(
            CodexAuth::ApiKey(_)
            | CodexAuth::ChatgptAuthTokens(_)
            | CodexAuth::PersonalAccessToken(_)
            | CodexAuth::AgentIdentity(_)
            | CodexAuth::Headers(_)
            | CodexAuth::BedrockApiKey(_)
            | CodexAuth::BedrockAccessKeys(_),
        )
        | None => return Err(BROWSER_AUTH_REQUIRED.into()),
    };
    let bearer = auth
        .get_token()
        .map_err(|error| format!("ChatGPT token unavailable: {error}"))?;
    if bearer.trim().is_empty() {
        return Err("ChatGPT token unavailable.".into());
    }
    let account_id = auth
        .get_account_id()
        .filter(|account_id| !account_id.trim().is_empty())
        .ok_or_else(|| "ChatGPT account ID unavailable.".to_string())?;
    Ok(UploadCredentials {
        bearer,
        account_id,
        fedramp: auth.is_fedramp_account(),
    })
}

async fn upload_with_deadline(
    auth: &impl TranscriptionAuth,
    http: &RouteAwareClientPool,
    endpoint: &str,
    account_id: &str,
    audio: RecordedAudio,
    cancellation: CancellationToken,
    deadline: Duration,
) -> Result<String, TranscriptionFailure> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(TranscriptionFailure::Canceled),
        result = tokio::time::timeout(deadline, async {
            let wav = encode_audio(audio)?;
            upload(auth, http, endpoint, account_id, wav).await
        }) => match result {
            Ok(result) => result,
            Err(_) => Err(TranscriptionFailure::TimedOut),
        },
    }
}

async fn upload(
    auth: &impl TranscriptionAuth,
    http: &RouteAwareClientPool,
    endpoint: &str,
    account_id: &str,
    wav: Vec<u8>,
) -> Result<String, TranscriptionFailure> {
    let mut credentials = auth.credentials().await?;
    if credentials.account_id != account_id {
        return Err(TranscriptionFailure::Failed(
            "ChatGPT account changed during dictation; start a new recording.".into(),
        ));
    }
    let mut response = send_upload(http, endpoint, &credentials, &wav).await?;
    if response.status == 401 {
        let original = format!("Transcription request failed (401): {}", response.body);
        auth.refresh()
            .await
            .map_err(|error| format!("{original}\nAuth refresh failed: {error}"))?;
        credentials = auth
            .credentials()
            .await
            .map_err(|error| format!("{original}\n{error}"))?;
        if credentials.account_id != account_id {
            return Err(format!(
                "{original}\nChatGPT account changed during dictation; no retry was sent."
            )
            .into());
        }
        response = send_upload(http, endpoint, &credentials, &wav)
            .await
            .map_err(|error| match error {
                TranscriptionFailure::Canceled => TranscriptionFailure::Canceled,
                TranscriptionFailure::TimedOut => TranscriptionFailure::TimedOut,
                TranscriptionFailure::Failed(error) => {
                    format!("{original}\nRetry failed: {error}").into()
                }
            })?;
    }
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "Transcription request failed ({}): {}",
            response.status, response.body
        )
        .into());
    }
    #[derive(Deserialize)]
    struct Transcript {
        text: String,
    }
    let text = serde_json::from_str::<Transcript>(&response.body)
        .map_err(|error| format!("Invalid transcription response: {error}"))?
        .text
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(TranscriptionFailure::Failed(
            "Transcription was empty.".into(),
        ));
    }
    Ok(text)
}

struct UploadResponse {
    status: u16,
    body: String,
}

async fn send_upload(
    http: &RouteAwareClientPool,
    endpoint: &str,
    credentials: &UploadCredentials,
    wav: &[u8],
) -> Result<UploadResponse, TranscriptionFailure> {
    let boundary = format!("----codex-transcribe-{}", uuid::Uuid::new_v4());
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"codex.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    ).into_bytes();
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        arch => arch,
    };
    let mut headers = codex_login::default_client::default_headers();
    // The route-aware builder appends header values; remove defaults before endpoint overrides.
    headers.remove("originator");
    headers.remove("user-agent");
    let mut request = http
        .post(endpoint)
        .headers(headers)
        .header("Authorization", format!("Bearer {}", credentials.bearer))
        .header("ChatGPT-Account-Id", credentials.account_id.as_str())
        .header("OAI-Product-Sku", "CODEX")
        .header("originator", "Codex Desktop")
        .header(
            "User-Agent",
            format!("Codex Desktop/26.609.41114 (Macintosh; Intel Mac OS X; {arch})"),
        )
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(REQUEST_TIMEOUT)
        .body(body);
    if credentials.fedramp {
        request = request.header("X-OpenAI-Fedramp", "true");
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            TranscriptionFailure::TimedOut
        } else {
            TranscriptionFailure::Failed(format!("Transcription upload failed: {error}"))
        }
    })?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|error| {
        if error.is_timeout() {
            TranscriptionFailure::TimedOut
        } else {
            TranscriptionFailure::Failed(format!("Transcription body failed: {error}"))
        }
    })?;
    Ok(UploadResponse { status, body })
}

fn encode_audio(audio: RecordedAudio) -> Result<Vec<u8>, String> {
    if audio.sample_rate == 0
        || audio.channels == 0
        || audio.data.is_empty()
        || !audio.data.len().is_multiple_of(usize::from(audio.channels))
    {
        return Err("Recording has invalid or empty audio.".into());
    }
    let channels = usize::from(audio.channels);
    let frames = audio.data.len() / channels;
    let output_len = (frames as u64)
        .checked_mul(u64::from(UPLOAD_RATE))
        .ok_or_else(|| "Recording is too large.".to_string())?
        / u64::from(audio.sample_rate);
    let output_len =
        usize::try_from(output_len.max(/*other*/ 1)).map_err(|_| "Recording is too large.")?;
    let data_len = u32::try_from(
        output_len
            .checked_mul(/*rhs*/ 2)
            .ok_or("Recording is too large.")?,
    )
    .map_err(|_| "Recording is too large.")?;
    let riff_len = data_len
        .checked_add(/*rhs*/ 36)
        .ok_or("Recording is too large.")?;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&UPLOAD_RATE.to_le_bytes());
    wav.extend_from_slice(&(UPLOAD_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for index in 0..output_len {
        let position = index as f64 * f64::from(audio.sample_rate) / f64::from(UPLOAD_RATE);
        let left = (position.floor() as usize).min(frames - 1);
        let right = (left + 1).min(frames - 1);
        let mix = |frame: usize| -> f64 {
            audio.data[frame * channels..(frame + 1) * channels]
                .iter()
                .map(|sample| f64::from(*sample))
                .sum::<f64>()
                / channels as f64
        };
        let sample = mix(left) + (mix(right) - mix(left)) * position.fract();
        wav.extend_from_slice(&(sample.round() as i16).to_le_bytes());
    }
    Ok(wav)
}

#[cfg(test)]
#[path = "transcription_tests.rs"]
mod tests;
