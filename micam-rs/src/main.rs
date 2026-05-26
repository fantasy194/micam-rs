use anyhow::{anyhow, Result};
use clap::{ArgAction, Parser};
use micam_rs::{native_miot, oauth, BridgeConfig, BridgeMode, RtspBridge};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "micam-rs",
    version,
    about = "Bridge Xiaomi Miloco WebSocket video streams to RTSP"
)]
struct Cli {
    #[arg(long, env = "MILOCO_MODE", default_value = "native")]
    mode: String,

    #[arg(long, env = "MILOCO_BASE_URL", default_value = "https://miloco:8000")]
    base_url: String,

    #[arg(long, env = "MILOCO_USERNAME", default_value = "admin")]
    username: String,

    #[arg(long, env = "MILOCO_PASSWORD", default_value = "")]
    password: String,

    #[arg(long, env = "CAMERA_ID", default_value = "")]
    camera_id: String,

    #[arg(long, env = "STREAM_CHANNEL", default_value = "0")]
    channel: String,

    #[arg(long, env = "VIDEO_CODEC", default_value = "h264")]
    video_codec: String,

    #[arg(long, env = "RTSP_URL", default_value = "rtsp://0.0.0.0:8554/live")]
    rtsp_url: String,

    #[arg(long, env = "FFMPEG_GPU_ENABLED", default_value_t = false, action = ArgAction::Set)]
    ffmpeg_gpu_enabled: bool,

    #[arg(long, env = "FFMPEG_VIDEO_ENCODER", default_value = "h264_nvenc")]
    ffmpeg_video_encoder: Option<String>,

    #[arg(long, env = "FFMPEG_HWACCEL", default_value = "cuda")]
    ffmpeg_hwaccel: Option<String>,

    #[arg(long, env = "FFMPEG_VAAPI_DEVICE")]
    ffmpeg_vaapi_device: Option<String>,

    #[arg(long, env = "FFMPEG_EXTRA_ARGS", default_value = "")]
    ffmpeg_extra_args: String,

    #[arg(long, env = "FFMPEG_LOW_LATENCY", default_value_t = true, action = ArgAction::Set)]
    ffmpeg_low_latency: bool,

    #[arg(long, env = "MIOT_ACCESS_TOKEN")]
    miot_access_token: Option<String>,

    #[arg(long, env = "MIOT_REFRESH_TOKEN")]
    miot_refresh_token: Option<String>,

    #[arg(long, env = "MIOT_TOKEN_FILE")]
    miot_token_file: Option<std::path::PathBuf>,

    #[arg(long, env = "MIOT_OAUTH_REDIRECT_URI", default_value_t = oauth::default_redirect_uri().to_string())]
    miot_oauth_redirect_uri: String,

    #[arg(long, env = "MIOT_REFRESH_MARGIN_SECONDS", default_value_t = oauth::default_refresh_margin_seconds())]
    miot_refresh_margin_seconds: i64,

    #[arg(long, env = "MIOT_CLOUD_SERVER", default_value = "cn")]
    miot_cloud_server: String,

    #[arg(long, env = "MIOT_CAMERA_MODEL")]
    miot_camera_model: Option<String>,

    #[arg(long, env = "MIOT_CHANNEL_COUNT", default_value_t = 1)]
    miot_channel_count: u8,

    #[arg(long, env = "MIOT_VIDEO_QUALITY", default_value_t = 2)]
    miot_video_quality: u8,

    #[arg(long, env = "MIOT_ENABLE_AUDIO", default_value_t = false, action = ArgAction::Set)]
    miot_enable_audio: bool,

    #[arg(long, env = "MIOT_PIN_CODE")]
    miot_pin_code: Option<String>,

    #[arg(long, env = "MIOT_LIB_PATH")]
    miot_lib_path: Option<std::path::PathBuf>,

    #[arg(long, env = "MIOT_QUEUE_CAPACITY", default_value_t = native_miot::DEFAULT_QUEUE_CAPACITY)]
    miot_queue_capacity: usize,
}

#[tokio::main]
async fn main() {
    init_logging();

    if let Err(error) = run().await {
        error!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.into_config()?;
    let mut bridge = RtspBridge::new(config)?;

    bridge.run().await?;
    info!("Stream finished");
    Ok(())
}

impl Cli {
    fn into_config(self) -> Result<BridgeConfig> {
        let mode = parse_mode(&self.mode)?;

        if mode == BridgeMode::Remote && self.password.trim().is_empty() {
            return Err(anyhow!("MILOCO_PASSWORD is required"));
        }

        if self.camera_id.trim().is_empty() {
            return Err(anyhow!("CAMERA_ID is required"));
        }

        if mode == BridgeMode::Native {
            if self.miot_camera_model.as_deref().unwrap_or("").trim().is_empty() {
                return Err(anyhow!("MIOT_CAMERA_MODEL is required when MILOCO_MODE=native"));
            }
        }

        Ok(BridgeConfig {
            mode,
            base_url: self.base_url,
            username: self.username,
            password: self.password,
            camera_id: self.camera_id,
            channel: self.channel,
            video_codec: self.video_codec,
            rtsp_url: self.rtsp_url,
            ffmpeg_gpu_enabled: self.ffmpeg_gpu_enabled,
            ffmpeg_video_encoder: self.ffmpeg_video_encoder,
            ffmpeg_hwaccel: self.ffmpeg_hwaccel,
            ffmpeg_vaapi_device: self.ffmpeg_vaapi_device,
            ffmpeg_extra_args: parse_extra_args(&self.ffmpeg_extra_args)?,
            ffmpeg_low_latency: self.ffmpeg_low_latency,
            miot_access_token: self.miot_access_token,
            miot_refresh_token: self.miot_refresh_token,
            miot_token_file: self
                .miot_token_file
                .or_else(|| Some(oauth::default_token_file())),
            miot_oauth_redirect_uri: self.miot_oauth_redirect_uri,
            miot_refresh_margin_seconds: self.miot_refresh_margin_seconds,
            miot_cloud_server: self.miot_cloud_server,
            miot_camera_model: self.miot_camera_model,
            miot_channel_count: self.miot_channel_count,
            miot_video_quality: self.miot_video_quality,
            miot_enable_audio: self.miot_enable_audio,
            miot_pin_code: self.miot_pin_code,
            miot_lib_path: self
                .miot_lib_path
                .unwrap_or_else(native_miot::default_lib_path),
            miot_queue_capacity: self.miot_queue_capacity,
        })
    }
}

fn parse_mode(value: &str) -> Result<BridgeMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "remote" | "miloco" => Ok(BridgeMode::Remote),
        "native" | "miot" => Ok(BridgeMode::Native),
        other => Err(anyhow!("invalid MILOCO_MODE: {other}; expected native or remote")),
    }
}

fn parse_extra_args(value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }

    shell_words::split(value).map_err(|error| anyhow!("invalid FFMPEG_EXTRA_ARGS: {error}"))
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
