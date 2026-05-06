use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::cookie::{CookieStore, Jar};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};
use tracing::{debug, error, info};
use url::Url;

const DEFAULT_FFMPEG_HWACCEL: &str = "cuda";
const DEFAULT_FFMPEG_VIDEO_ENCODER: &str = "h264_nvenc";
const DEFAULT_FFMPEG_AUDIO_ENCODER: &str = "aac";
const HOMEKIT_PIXEL_FORMAT: &str = "yuv420p";
const HOMEKIT_AUDIO_RATE: &str = "48000";
const HOMEKIT_AUDIO_CHANNELS: &str = "2";

pub fn is_keyframe(video_codec: &str, data: &[u8]) -> bool {
    match video_codec {
        "h264" => find_annex_b_nal_units(data).any(|nal_start| (data[nal_start] & 0x1f) == 5),
        "hevc" => find_annex_b_nal_units(data).any(|nal_start| {
            let nal_unit_type = (data[nal_start] >> 1) & 0x3f;
            (16..=20).contains(&nal_unit_type)
        }),
        _ => true,
    }
}

fn find_annex_b_nal_units(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 4 <= data.len() {
            let nal_start = if data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x01 {
                Some(i + 3)
            } else if i + 5 <= data.len()
                && data[i] == 0x00
                && data[i + 1] == 0x00
                && data[i + 2] == 0x00
                && data[i + 3] == 0x01
            {
                Some(i + 4)
            } else {
                None
            };

            i += 1;

            if let Some(nal_start) = nal_start {
                if nal_start < data.len() {
                    return Some(nal_start);
                }
            }
        }

        None
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegOptions {
    pub video_codec: String,
    pub rtsp_url: String,
    pub video_encoder: Option<String>,
    pub hwaccel: Option<String>,
}

pub fn build_ffmpeg_args(options: &FfmpegOptions) -> Vec<String> {
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-hide_banner".to_string(),
        "-use_wallclock_as_timestamps".to_string(),
        "1".to_string(),
        "-analyzeduration".to_string(),
        "20000000".to_string(),
        "-probesize".to_string(),
        "20000000".to_string(),
    ];

    args.push("-hwaccel".to_string());
    args.push(
        option_value(&options.hwaccel)
            .unwrap_or(DEFAULT_FFMPEG_HWACCEL)
            .to_string(),
    );

    args.extend([
        "-f".to_string(),
        options.video_codec.clone(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-c:v".to_string(),
        option_value(&options.video_encoder)
            .unwrap_or(DEFAULT_FFMPEG_VIDEO_ENCODER)
            .to_string(),
        "-pix_fmt".to_string(),
        HOMEKIT_PIXEL_FORMAT.to_string(),
        "-c:a".to_string(),
        DEFAULT_FFMPEG_AUDIO_ENCODER.to_string(),
        "-ar".to_string(),
        HOMEKIT_AUDIO_RATE.to_string(),
        "-ac".to_string(),
        HOMEKIT_AUDIO_CHANNELS.to_string(),
        "-f".to_string(),
        "rtsp".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        options.rtsp_url.clone(),
    ]);

    args
}

fn option_value(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub camera_id: String,
    pub channel: String,
    pub video_codec: String,
    pub rtsp_url: String,
    pub ffmpeg_video_encoder: Option<String>,
    pub ffmpeg_hwaccel: Option<String>,
}

impl BridgeConfig {
    pub fn ffmpeg_options(&self) -> FfmpegOptions {
        FfmpegOptions {
            video_codec: self.video_codec.clone(),
            rtsp_url: self.rtsp_url.clone(),
            video_encoder: self.ffmpeg_video_encoder.clone(),
            hwaccel: self.ffmpeg_hwaccel.clone(),
        }
    }
}

pub struct RtspBridge {
    config: BridgeConfig,
    cookie_jar: Arc<Jar>,
    client: reqwest::Client,
    process: Option<FfmpegProcess>,
    waiting_for_keyframe: bool,
}

impl RtspBridge {
    pub fn new(config: BridgeConfig) -> Result<Self> {
        let cookie_jar = Arc::new(Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(cookie_jar.clone())
            .danger_accept_invalid_certs(true)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            config,
            cookie_jar,
            client,
            process: None,
            waiting_for_keyframe: true,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        self.start_ffmpeg().await?;
        let result = self.run_stream().await;
        self.stop_ffmpeg().await;
        result
    }

    async fn run_stream(&mut self) -> Result<()> {
        self.login().await?;

        let ws_url = self.ws_url()?;
        info!("Connecting to WebSocket: {ws_url}");

        let mut request = ws_url
            .as_str()
            .into_client_request()
            .context("failed to build WebSocket request")?;

        if let Some(cookie_header) = self.cookie_header_for_ws(&ws_url) {
            request.headers_mut().insert("cookie", cookie_header);
        }

        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("failed to build WebSocket TLS connector")?;
        let (mut ws, _) =
            connect_async_tls_with_config(request, None, false, Some(Connector::NativeTls(tls)))
                .await
                .context("failed to connect WebSocket")?;

        info!("WebSocket connected. Streaming data...");

        loop {
            let msg = match tokio::time::timeout(Duration::from_secs(60), ws.next()).await {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(error))) => return Err(error).context("WebSocket receive error"),
                Ok(None) => {
                    info!("WebSocket closed");
                    return Ok(());
                }
                Err(_) => return Err(anyhow!("data receive timeout")),
            };

            match msg {
                Message::Binary(data) => {
                    if data.len() >= 100 {
                        debug!("Received binary data: {}", data.len());
                    }

                    if self.waiting_for_keyframe {
                        if is_keyframe(&self.config.video_codec, &data) {
                            info!("Keyframe detected. Starting stream...");
                            self.waiting_for_keyframe = false;
                        } else {
                            debug!("Skipping non-keyframe data...");
                            continue;
                        }
                    }

                    self.write_to_ffmpeg(&data).await?;
                }
                Message::Close(frame) => {
                    info!("WebSocket close: {frame:?}");
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_) => {}
            }
        }
    }

    async fn login(&self) -> Result<()> {
        let login_url = format!(
            "{}/api/auth/login",
            self.config.base_url.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "username": self.config.username,
            "password": self.config.password,
        });

        let response = self
            .client
            .post(&login_url)
            .json(&payload)
            .send()
            .await
            .context("login request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("login failed: {status} - {body}"));
        }

        let data: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        info!("Login successful: {data}");

        let status_url = format!(
            "{}/api/miot/login_status",
            self.config.base_url.trim_end_matches('/')
        );
        let status = self
            .client
            .get(&status_url)
            .send()
            .await
            .context("login status request failed")?;

        if !status.status().is_success() {
            return Err(anyhow!("login status check failed: {}", status.status()));
        }

        Ok(())
    }

    fn ws_url(&self) -> Result<Url> {
        let base = Url::parse(&self.config.base_url).context("invalid Miloco base URL")?;
        let protocol = match base.scheme() {
            "https" => "wss",
            "http" => "ws",
            scheme => return Err(anyhow!("unsupported Miloco URL scheme: {scheme}")),
        };
        let host = base
            .host_str()
            .ok_or_else(|| anyhow!("Miloco base URL has no host"))?;
        let host_port = match base.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };

        let path = format!(
            "{protocol}://{host_port}/api/miot/ws/video_stream?camera_id={}&channel={}",
            self.config.camera_id, self.config.channel
        );

        Url::parse(&path).context("failed to build WebSocket URL")
    }

    fn cookie_header_for_ws(&self, ws_url: &Url) -> Option<http::HeaderValue> {
        let mut cookie_url = ws_url.clone();
        let scheme = if ws_url.scheme() == "wss" {
            "https"
        } else {
            "http"
        };
        cookie_url.set_scheme(scheme).ok()?;
        self.cookie_jar.cookies(&cookie_url)
    }

    async fn start_ffmpeg(&mut self) -> Result<()> {
        let args = build_ffmpeg_args(&self.config.ffmpeg_options());
        info!("Starting FFmpeg: ffmpeg {}", args.join(" "));

        let mut child = Command::new("ffmpeg")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start FFmpeg")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("FFmpeg process has no stdin"))?;
        let stderr = child.stderr.take();

        self.process = Some(FfmpegProcess {
            child,
            stdin,
            stderr,
        });

        Ok(())
    }

    async fn write_to_ffmpeg(&mut self, data: &[u8]) -> Result<()> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| anyhow!("FFmpeg process not started"))?;

        tokio::time::timeout(Duration::from_secs(30), process.stdin.write_all(data))
            .await
            .map_err(|_| anyhow!("write data to FFmpeg timeout"))?
            .context("failed to write data to FFmpeg")?;

        Ok(())
    }

    async fn stop_ffmpeg(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };

        let _ = process.stdin.shutdown().await;

        match tokio::time::timeout(Duration::from_secs(5), process.child.wait()).await {
            Ok(Ok(status)) => {
                if !status.success() {
                    if let Some(stderr) = process.read_stderr().await {
                        error!("FFmpeg exited with {status}. stderr: {stderr}");
                    } else {
                        error!("FFmpeg exited with {status}");
                    }
                }
            }
            Ok(Err(error)) => error!("failed to wait for FFmpeg: {error}"),
            Err(_) => {
                let _ = process.child.kill().await;
            }
        }
    }
}

struct FfmpegProcess {
    child: Child,
    stdin: ChildStdin,
    stderr: Option<ChildStderr>,
}

impl FfmpegProcess {
    async fn read_stderr(&mut self) -> Option<String> {
        let mut stderr = self.stderr.take()?;
        let mut output = String::new();
        match tokio::time::timeout(Duration::from_secs(2), stderr.read_to_string(&mut output)).await
        {
            Ok(Ok(_)) if !output.trim().is_empty() => Some(output),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_h264_idr_keyframe() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];

        assert!(is_keyframe("h264", &data));
    }

    #[test]
    fn rejects_h264_non_idr_frame() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x22];

        assert!(!is_keyframe("h264", &data));
    }

    #[test]
    fn detects_hevc_irap_keyframe() {
        let nal_type_19 = 19u8 << 1;
        let data = [0x00, 0x00, 0x00, 0x01, nal_type_19, 0x01, 0x60];

        assert!(is_keyframe("hevc", &data));
    }

    #[test]
    fn builds_homekit_gpu_ffmpeg_args_by_default() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "hevc".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            video_encoder: None,
            hwaccel: None,
        });

        assert!(has_pair(&args, "-hwaccel", "cuda"));
        assert!(has_pair(&args, "-c:v", "h264_nvenc"));
        assert!(has_pair(&args, "-pix_fmt", "yuv420p"));
        assert!(has_pair(&args, "-c:a", "aac"));
        assert!(has_pair(&args, "-ar", "48000"));
        assert!(has_pair(&args, "-ac", "2"));
    }

    #[test]
    fn uses_configured_video_encoder_and_hwaccel() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "hevc".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            video_encoder: Some("h264_qsv".to_string()),
            hwaccel: Some("qsv".to_string()),
        });

        assert!(has_pair(&args, "-hwaccel", "qsv"));
        assert!(has_pair(&args, "-c:v", "h264_qsv"));
        assert!(has_pair(&args, "-c:a", "aac"));
    }

    fn has_pair(args: &[String], key: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == key && pair[1] == value)
    }
}
