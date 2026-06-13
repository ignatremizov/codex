use super::RecordedAudio;
use super::clip_duration_seconds;
use super::convert_pcm16;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::legacy_core::config::Config;
use base64::Engine;
use codex_app_server_protocol::AuthMode;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::ChatgptAuthSession;
use codex_login::CodexAuth;
use codex_login::default_client::build_reqwest_client;
use codex_login::load_auth_dot_json;
use codex_login::read_codex_access_token_from_env;
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::error;
use tracing::info;

const CODEX_PRODUCT_SKU: &str = "codex";
const TRANSCRIPTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
const TRANSCRIPTION_UPLOAD_SAMPLE_RATE: u32 = 24_000;
const TRANSCRIPTION_UPLOAD_CHANNELS: u16 = 1;
const TRANSCRIPTION_UPLOAD_CONTENT_TYPE: &str = "audio/wav";
const TRANSCRIPTION_UPLOAD_FILENAME: &str = "codex.wav";

#[derive(Clone)]
struct TranscriptionUpload {
    bytes: Vec<u8>,
    filename: &'static str,
    content_type: &'static str,
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

pub(crate) fn validate_transcription_auth(config: &Config) -> Result<(), String> {
    if read_codex_access_token_from_env().is_some() {
        return Err(
            "Dictation requires local ChatGPT login auth; CODEX_ACCESS_TOKEN auth is not supported."
                .to_string(),
        );
    }

    if load_auth_dot_json(
        config.codex_home.as_path(),
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|error| format!("failed to inspect external ChatGPT auth: {error}"))?
    .is_some()
    {
        return Err(
            "Dictation requires local ChatGPT login auth; external app-server auth tokens are not supported."
                .to_string(),
        );
    }

    let auth = load_auth_dot_json(
        config.codex_home.as_path(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|error| format!("failed to inspect ChatGPT auth: {error}"))?
    .ok_or_else(|| "Dictation requires ChatGPT login auth. Run `codex login`.".to_string())?;

    match auth.auth_mode {
        Some(AuthMode::Chatgpt) | None => {}
        Some(AuthMode::ChatgptAuthTokens) => {
            return Err(
                "Dictation requires local ChatGPT login auth; external app-server auth tokens are not supported."
                    .to_string(),
            );
        }
        Some(AuthMode::PersonalAccessToken) => {
            return Err(
                "Dictation requires browser ChatGPT login auth; personal access token auth is not supported."
                    .to_string(),
            );
        }
        Some(AuthMode::ApiKey | AuthMode::AgentIdentity | AuthMode::BedrockApiKey) => {
            return Err("Dictation requires ChatGPT login auth. Run `codex login`.".to_string());
        }
    }

    if auth.openai_api_key.is_some()
        || auth.agent_identity.is_some()
        || auth.personal_access_token.is_some()
        || auth.bedrock_api_key.is_some()
    {
        return Err("Dictation requires ChatGPT login auth. Run `codex login`.".to_string());
    }

    if auth.tokens.is_none() {
        return Err(
            "Dictation requires local ChatGPT token data. Run `codex login` again.".to_string(),
        );
    }

    Ok(())
}

pub(crate) fn transcribe_async(
    id: String,
    audio: RecordedAudio,
    config: Config,
    tx: AppEventSender,
) {
    std::thread::spawn(move || {
        const MIN_DURATION_SECONDS: f32 = 1.0;
        let duration_seconds = clip_duration_seconds(&audio);
        if duration_seconds < MIN_DURATION_SECONDS {
            let message = format!(
                "recording too short ({duration_seconds:.2}s); minimum is {MIN_DURATION_SECONDS:.2}s"
            );
            info!("{message}");
            tx.send(AppEvent::TranscriptionFailed { id, error: message });
            return;
        }

        let upload = match build_transcription_upload(&audio) {
            Ok(upload) => upload,
            Err(error) => {
                error!("failed to encode voice recording: {error}");
                tx.send(AppEvent::TranscriptionFailed { id, error });
                return;
            }
        };

        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                let message = format!("failed to create transcription runtime: {error}");
                error!("{message}");
                tx.send(AppEvent::TranscriptionFailed { id, error: message });
                return;
            }
        };

        match runtime.block_on(transcribe_upload(upload, config)) {
            Ok(text) => {
                info!("voice transcription succeeded");
                tx.send(AppEvent::TranscriptionComplete { id, text });
            }
            Err(error) => {
                error!("voice transcription error: {error}");
                tx.send(AppEvent::TranscriptionFailed { id, error });
            }
        }
    });
}

fn build_transcription_upload(audio: &RecordedAudio) -> Result<TranscriptionUpload, String> {
    if audio.sample_rate == 0 || audio.channels == 0 {
        return Err("recording has invalid audio format".to_string());
    }

    let samples = if audio.sample_rate == TRANSCRIPTION_UPLOAD_SAMPLE_RATE
        && audio.channels == TRANSCRIPTION_UPLOAD_CHANNELS
    {
        audio.data.clone()
    } else {
        convert_pcm16(
            &audio.data,
            audio.sample_rate,
            audio.channels,
            TRANSCRIPTION_UPLOAD_SAMPLE_RATE,
            TRANSCRIPTION_UPLOAD_CHANNELS,
        )
    };
    if samples.is_empty() {
        return Err("recording did not contain audio samples".to_string());
    }

    Ok(TranscriptionUpload {
        bytes: encode_wav_pcm16(
            &samples,
            TRANSCRIPTION_UPLOAD_SAMPLE_RATE,
            TRANSCRIPTION_UPLOAD_CHANNELS,
        )?,
        filename: TRANSCRIPTION_UPLOAD_FILENAME,
        content_type: TRANSCRIPTION_UPLOAD_CONTENT_TYPE,
    })
}

fn encode_wav_pcm16(samples: &[i16], sample_rate: u32, channels: u16) -> Result<Vec<u8>, String> {
    if sample_rate == 0 || channels == 0 {
        return Err("invalid WAV audio format".to_string());
    }

    let data_len = samples
        .len()
        .checked_mul(std::mem::size_of::<i16>())
        .ok_or_else(|| "recording is too large to encode".to_string())?;
    let data_len_u32 =
        u32::try_from(data_len).map_err(|_| "recording is too large to encode".to_string())?;
    let riff_chunk_size = 36u32
        .checked_add(data_len_u32)
        .ok_or_else(|| "recording is too large to encode".to_string())?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|rate| rate.checked_mul(2))
        .ok_or_else(|| "invalid WAV audio format".to_string())?;
    let block_align = channels
        .checked_mul(2)
        .ok_or_else(|| "invalid WAV audio format".to_string())?;

    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_chunk_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len_u32.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

async fn transcribe_upload(upload: TranscriptionUpload, config: Config) -> Result<String, String> {
    validate_transcription_auth(&config)?;
    let auth_session = ChatgptAuthSession::from_config(&config).await;
    let auth = chatgpt_oauth_auth(auth_session.auth().await)?;
    let client = build_reqwest_client();
    let endpoint = transcribe_endpoint(&config.chatgpt_base_url);

    let mut response = send_transcription_request(&client, &endpoint, &auth, upload.clone())
        .await
        .map_err(|error| format!("transcription request failed: {error}"))?;

    if response.status() == StatusCode::UNAUTHORIZED {
        auth_session
            .refresh_token()
            .await
            .map_err(|error| format!("failed to refresh ChatGPT auth: {error}"))?;
        let auth = chatgpt_oauth_auth(auth_session.auth().await)?;
        response = send_transcription_request(&client, &endpoint, &auth, upload)
            .await
            .map_err(|error| format!("transcription request failed after auth refresh: {error}"))?;
    }

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read transcription response: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "transcription request failed ({status}): {}",
            truncate_error_body(&body)
        ));
    }

    let response: TranscriptionResponse = serde_json::from_str(&body)
        .map_err(|error| format!("failed to parse transcription response: {error}"))?;
    let text = response.text.trim().to_string();
    if text.is_empty() {
        return Err("transcription was empty".to_string());
    }
    Ok(text)
}

fn chatgpt_oauth_auth(auth: Option<CodexAuth>) -> Result<CodexAuth, String> {
    match auth {
        Some(auth @ CodexAuth::Chatgpt(_)) => Ok(auth),
        Some(CodexAuth::ChatgptAuthTokens(_)) => Err(
            "dictation requires local ChatGPT login auth; external app-server auth tokens are not supported."
                .to_string(),
        ),
        Some(CodexAuth::PersonalAccessToken(_)) => Err(
            "dictation requires browser ChatGPT login auth; personal access token auth is not supported."
                .to_string(),
        ),
        Some(
            CodexAuth::ApiKey(_) | CodexAuth::AgentIdentity(_) | CodexAuth::BedrockApiKey(_),
        )
        | None => Err("dictation requires ChatGPT login auth; run `codex login`".to_string()),
    }
}

fn transcribe_endpoint(chatgpt_base_url: &str) -> String {
    format!("{}/transcribe", chatgpt_base_url.trim_end_matches('/'))
}

async fn send_transcription_request(
    client: &reqwest::Client,
    endpoint: &str,
    auth: &CodexAuth,
    upload: TranscriptionUpload,
) -> Result<reqwest::Response, String> {
    let token = auth
        .get_token()
        .map_err(|error| format!("ChatGPT auth token is unavailable: {error}"))?;
    let account_id = auth
        .get_account_id()
        .ok_or_else(|| "ChatGPT account id is unavailable".to_string())?;
    let (content_type, body) = build_base64_multipart_body(upload);
    let mut request = client
        .post(endpoint)
        .bearer_auth(token)
        .header("ChatGPT-Account-Id", account_id)
        .header("OAI-Product-Sku", CODEX_PRODUCT_SKU)
        .header("X-Codex-Base64", "1")
        .header("Content-Type", content_type)
        .timeout(TRANSCRIPTION_REQUEST_TIMEOUT)
        .body(body);

    if auth.is_fedramp_account() {
        request = request.header("X-OpenAI-Fedramp", "true");
    }

    request
        .send()
        .await
        .map_err(|error| format!("failed to send transcription request: {error}"))
}

fn build_base64_multipart_body(upload: TranscriptionUpload) -> (String, String) {
    let boundary = multipart_boundary();
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n",
            upload.filename
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", upload.content_type).as_bytes());
    body.extend_from_slice(&upload.bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (
        format!("multipart/form-data; boundary={boundary}"),
        base64::engine::general_purpose::STANDARD.encode(body),
    )
}

fn multipart_boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("----codex-transcribe-{nanos}")
}

fn truncate_error_body(body: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 500;
    let mut truncated = body.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if body.chars().count() > MAX_ERROR_BODY_CHARS {
        truncated.push_str("...");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::TranscriptionUpload;
    use super::build_base64_multipart_body;
    use super::build_transcription_upload;
    use crate::voice::RecordedAudio;
    use base64::Engine as _;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_transcription_upload_encodes_normalized_wav() {
        let upload = build_transcription_upload(&RecordedAudio {
            data: vec![100, 300, 200, 400],
            sample_rate: 48_000,
            channels: 2,
        })
        .expect("upload should encode");

        assert_eq!(upload.filename, "codex.wav");
        assert_eq!(upload.content_type, "audio/wav");
        assert_eq!(&upload.bytes[0..4], b"RIFF");
        assert_eq!(&upload.bytes[8..12], b"WAVE");
        assert_eq!(&upload.bytes[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([upload.bytes[20], upload.bytes[21]]), 1);
        assert_eq!(u16::from_le_bytes([upload.bytes[22], upload.bytes[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([
                upload.bytes[24],
                upload.bytes[25],
                upload.bytes[26],
                upload.bytes[27],
            ]),
            24_000
        );
        assert_eq!(&upload.bytes[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([
                upload.bytes[40],
                upload.bytes[41],
                upload.bytes[42],
                upload.bytes[43],
            ]),
            2
        );
        assert_eq!(
            i16::from_le_bytes([upload.bytes[44], upload.bytes[45]]),
            200
        );
    }

    #[test]
    fn build_base64_multipart_body_uses_file_field() {
        let upload = TranscriptionUpload {
            bytes: vec![1, 2, 3],
            filename: "codex.wav",
            content_type: "audio/wav",
        };

        let (content_type, body) = build_base64_multipart_body(upload);
        let body = base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("body should be base64 encoded");
        let body = String::from_utf8(body).expect("multipart body should be utf-8 for test data");

        assert!(content_type.starts_with("multipart/form-data; boundary=----codex-transcribe-"));
        assert!(
            body.contains(
                "Content-Disposition: form-data; name=\"file\"; filename=\"codex.wav\"\r\n"
            )
        );
        assert!(body.contains("Content-Type: audio/wav\r\n\r\n"));
        assert!(body.ends_with("--\r\n"));
    }
}
