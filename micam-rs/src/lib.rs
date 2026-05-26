use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::cookie::{CookieStore, Jar};
use std::path::PathBuf;
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

pub mod miloco;
pub mod native_miot;
pub mod oauth;

const DEFAULT_FFMPEG_HWACCEL: &str = "cuda";
const DEFAULT_FFMPEG_VIDEO_ENCODER: &str = "h264_nvenc";
const DEFAULT_FFMPEG_AUDIO_ENCODER: &str = "aac";
const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";
const HOMEKIT_PIXEL_FORMAT: &str = "yuv420p";
const VAAPI_UPLOAD_FILTER: &str = "format=nv12,hwupload";
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
    pub gpu_enabled: bool,
    pub video_encoder: Option<String>,
    pub hwaccel: Option<String>,
    pub vaapi_device: Option<String>,
    pub extra_args: Vec<String>,
    pub low_latency: bool,
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

    let hwaccel = option_value(&options.hwaccel).unwrap_or(DEFAULT_FFMPEG_HWACCEL);
    let video_encoder =
        option_value(&options.video_encoder).unwrap_or(DEFAULT_FFMPEG_VIDEO_ENCODER);
    let uses_vaapi = options.gpu_enabled
        && (hwaccel.eq_ignore_ascii_case("vaapi")
            || video_encoder.to_ascii_lowercase().ends_with("_vaapi"));

    if options.gpu_enabled {
        if uses_vaapi {
            args.push("-vaapi_device".to_string());
            args.push(
                option_value(&options.vaapi_device)
                    .unwrap_or(DEFAULT_VAAPI_DEVICE)
                    .to_string(),
            );
        }

        args.push("-hwaccel".to_string());
        args.push(hwaccel.to_string());
    }

    args.extend([
        "-f".to_string(),
        options.video_codec.clone(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-c:v".to_string(),
        if options.gpu_enabled {
            video_encoder.to_string()
        } else {
            "copy".to_string()
        },
    ]);

    if options.gpu_enabled {
        if uses_vaapi {
            args.push("-vf".to_string());
            args.push(VAAPI_UPLOAD_FILTER.to_string());
        } else {
            args.push("-pix_fmt".to_string());
            args.push(HOMEKIT_PIXEL_FORMAT.to_string());
        }

        args.extend([
            "-c:a".to_string(),
            DEFAULT_FFMPEG_AUDIO_ENCODER.to_string(),
            "-ar".to_string(),
            HOMEKIT_AUDIO_RATE.to_string(),
            "-ac".to_string(),
            HOMEKIT_AUDIO_CHANNELS.to_string(),
        ]);
    } else {
        args.extend(["-c:a".to_string(), "copy".to_string()]);
    }

    args.extend([
        "-f".to_string(),
        "rtsp".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
    ]);

    if options.low_latency {
        args.extend([
            "-fflags".to_string(),
            "nobuffer".to_string(),
            "-flags".to_string(),
            "low_delay".to_string(),
            "-flush_packets".to_string(),
            "1".to_string(),
            "-muxdelay".to_string(),
            "0".to_string(),
            "-muxpreload".to_string(),
            "0".to_string(),
        ]);
    }

    args.extend(options.extra_args.iter().cloned());
    args.push(options.rtsp_url.clone());

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
    pub mode: BridgeMode,
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub camera_id: String,
    pub channel: String,
    pub video_codec: String,
    pub rtsp_url: String,
    pub ffmpeg_gpu_enabled: bool,
    pub ffmpeg_video_encoder: Option<String>,
    pub ffmpeg_hwaccel: Option<String>,
    pub ffmpeg_vaapi_device: Option<String>,
    pub ffmpeg_extra_args: Vec<String>,
    pub ffmpeg_low_latency: bool,
    pub miot_access_token: Option<String>,
    pub miot_refresh_token: Option<String>,
    pub miot_token_file: Option<PathBuf>,
    pub miot_oauth_redirect_uri: String,
    pub miot_refresh_margin_seconds: i64,
    pub miot_cloud_server: String,
    pub miot_camera_model: Option<String>,
    pub miot_channel_count: u8,
    pub miot_video_quality: u8,
    pub miot_enable_audio: bool,
    pub miot_pin_code: Option<String>,
    pub miot_lib_path: PathBuf,
    pub miot_queue_capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeMode {
    Remote,
    Native,
}

impl BridgeConfig {
    pub fn ffmpeg_options(&self) -> FfmpegOptions {
        FfmpegOptions {
            video_codec: self.video_codec.clone(),
            rtsp_url: self.rtsp_url.clone(),
            gpu_enabled: self.ffmpeg_gpu_enabled,
            video_encoder: self.ffmpeg_video_encoder.clone(),
            hwaccel: self.ffmpeg_hwaccel.clone(),
            vaapi_device: self.ffmpeg_vaapi_device.clone(),
            extra_args: self.ffmpeg_extra_args.clone(),
            low_latency: self.ffmpeg_low_latency,
        }
    }

    async fn native_config(&self) -> Result<native_miot::NativeMiotConfig> {
        let channel = self
            .channel
            .parse::<u8>()
            .context("STREAM_CHANNEL must be an integer for native mode")?;
        let token = oauth::resolve_access_token(&oauth::TokenResolverConfig {
            cloud_server: self.miot_cloud_server.clone(),
            redirect_uri: self.miot_oauth_redirect_uri.clone(),
            token_file: self.miot_token_file.clone(),
            access_token: self.miot_access_token.clone(),
            refresh_token: self.miot_refresh_token.clone(),
            refresh_margin_seconds: self.miot_refresh_margin_seconds,
        })
        .await?;
        Ok(native_miot::NativeMiotConfig {
            lib_path: self.miot_lib_path.clone(),
            cloud_server: self.miot_cloud_server.clone(),
            access_token: token.access_token,
            camera_id: self.camera_id.clone(),
            camera_model: self.miot_camera_model.clone().unwrap_or_default(),
            channel_count: self.miot_channel_count,
            channel,
            video_quality: self.miot_video_quality,
            enable_audio: self.miot_enable_audio,
            pin_code: self.miot_pin_code.clone(),
            queue_capacity: self.miot_queue_capacity,
        })
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
        let result = match self.config.mode {
            BridgeMode::Remote => self.run_remote_stream().await,
            BridgeMode::Native => self.run_native_stream().await,
        };
        self.stop_ffmpeg().await;
        result
    }

    async fn run_remote_stream(&mut self) -> Result<()> {
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

                    self.ensure_ffmpeg_started().await?;
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

    async fn run_native_stream(&mut self) -> Result<()> {
        let native_config = self.config.native_config().await?;
        let (_source, mut frames) = native_miot::NativeMiotSource::start(&native_config)
            .context("failed to start native MIoT source")?;
        info!("Native MIoT source started");

        loop {
            let frame = tokio::time::timeout(Duration::from_secs(60), frames.recv())
                .await
                .map_err(|_| anyhow!("native MIoT data receive timeout"))?
                .ok_or_else(|| anyhow!("native MIoT frame channel closed"))?;

            if self.config.video_codec == "auto" {
                self.config.video_codec = frame.codec.to_string();
            }

            if self.waiting_for_keyframe {
                if is_keyframe(&self.config.video_codec, &frame.data) {
                    info!("Keyframe detected. Starting stream...");
                    self.waiting_for_keyframe = false;
                } else {
                    debug!("Skipping non-keyframe data...");
                    continue;
                }
            }

            self.ensure_ffmpeg_started().await?;
            self.write_to_ffmpeg(&frame.data).await?;
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

    async fn ensure_ffmpeg_started(&mut self) -> Result<()> {
        if self.process.is_none() {
            self.start_ffmpeg().await?;
        }
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
            gpu_enabled: true,
            video_encoder: None,
            hwaccel: None,
            vaapi_device: None,
            extra_args: Vec::new(),
            low_latency: false,
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
            gpu_enabled: true,
            video_encoder: Some("h264_qsv".to_string()),
            hwaccel: Some("qsv".to_string()),
            vaapi_device: None,
            extra_args: Vec::new(),
            low_latency: false,
        });

        assert!(has_pair(&args, "-hwaccel", "qsv"));
        assert!(has_pair(&args, "-c:v", "h264_qsv"));
        assert!(has_pair(&args, "-c:a", "aac"));
    }

    #[test]
    fn disables_gpu_and_copies_stream_when_requested() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "h264".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            gpu_enabled: false,
            video_encoder: Some("h264_vaapi".to_string()),
            hwaccel: Some("vaapi".to_string()),
            vaapi_device: Some("/dev/dri/renderD128".to_string()),
            extra_args: Vec::new(),
            low_latency: false,
        });

        assert!(!args.contains(&"-hwaccel".to_string()));
        assert!(!args.contains(&"-vaapi_device".to_string()));
        assert!(has_pair(&args, "-c:v", "copy"));
        assert!(has_pair(&args, "-c:a", "copy"));
    }

    #[test]
    fn adds_vaapi_device_and_upload_filter_for_vaapi_encoder() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "h264".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            gpu_enabled: true,
            video_encoder: Some("h264_vaapi".to_string()),
            hwaccel: Some("vaapi".to_string()),
            vaapi_device: Some("/dev/dri/renderD128".to_string()),
            extra_args: Vec::new(),
            low_latency: false,
        });

        assert!(has_pair(&args, "-vaapi_device", "/dev/dri/renderD128"));
        assert!(has_pair(&args, "-hwaccel", "vaapi"));
        assert!(has_pair(&args, "-vf", "format=nv12,hwupload"));
        assert!(has_pair(&args, "-c:v", "h264_vaapi"));
    }

    #[test]
    fn appends_extra_ffmpeg_args_before_output_url() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "h264".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            gpu_enabled: true,
            video_encoder: Some("h264_qsv".to_string()),
            hwaccel: Some("qsv".to_string()),
            vaapi_device: None,
            extra_args: vec!["-b:v".to_string(), "2500k".to_string()],
            low_latency: false,
        });
        let output_url_index = args
            .iter()
            .position(|arg| arg == "rtsp://127.0.0.1:8554/live")
            .expect("output url should be present");

        assert_eq!(args[output_url_index - 2], "-b:v");
        assert_eq!(args[output_url_index - 1], "2500k");
    }

    #[test]
    fn maps_native_codec_ids_to_ffmpeg_input_codecs() {
        assert_eq!(native_miot::codec_name(4), Some("h264"));
        assert_eq!(native_miot::codec_name(5), Some("hevc"));
        assert_eq!(native_miot::codec_name(99), None);
    }

    #[test]
    fn low_latency_ffmpeg_args_are_inserted_before_output_url() {
        let args = build_ffmpeg_args(&FfmpegOptions {
            video_codec: "h264".to_string(),
            rtsp_url: "rtsp://127.0.0.1:8554/live".to_string(),
            gpu_enabled: false,
            video_encoder: None,
            hwaccel: None,
            vaapi_device: None,
            extra_args: Vec::new(),
            low_latency: true,
        });

        let output_url_index = args
            .iter()
            .position(|arg| arg == "rtsp://127.0.0.1:8554/live")
            .expect("output url should be present");
        assert!(args[..output_url_index]
            .windows(2)
            .any(|pair| pair[0] == "-fflags" && pair[1] == "nobuffer"));
        assert!(args[..output_url_index]
            .windows(2)
            .any(|pair| pair[0] == "-flags" && pair[1] == "low_delay"));
        assert!(args[..output_url_index]
            .windows(2)
            .any(|pair| pair[0] == "-flush_packets" && pair[1] == "1"));
    }

    fn has_pair(args: &[String], key: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == key && pair[1] == value)
    }
}
