use anyhow::{anyhow, Result};
use clap::Parser;
use micam_rs::{BridgeConfig, RtspBridge};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "micam-rs",
    version,
    about = "Bridge Xiaomi Miloco WebSocket video streams to RTSP"
)]
struct Cli {
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

    #[arg(long, env = "VIDEO_CODEC", default_value = "hevc")]
    video_codec: String,

    #[arg(long, env = "RTSP_URL", default_value = "rtsp://0.0.0.0:8554/live")]
    rtsp_url: String,

    #[arg(long, env = "FFMPEG_VIDEO_ENCODER", default_value = "h264_nvenc")]
    ffmpeg_video_encoder: Option<String>,

    #[arg(long, env = "FFMPEG_HWACCEL", default_value = "cuda")]
    ffmpeg_hwaccel: Option<String>,
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
        if self.password.trim().is_empty() {
            return Err(anyhow!("MILOCO_PASSWORD is required"));
        }

        if self.camera_id.trim().is_empty() {
            return Err(anyhow!("CAMERA_ID is required"));
        }

        Ok(BridgeConfig {
            base_url: self.base_url,
            username: self.username,
            password: self.password,
            camera_id: self.camera_id,
            channel: self.channel,
            video_codec: self.video_codec,
            rtsp_url: self.rtsp_url,
            ffmpeg_video_encoder: self.ffmpeg_video_encoder,
            ffmpeg_hwaccel: self.ffmpeg_hwaccel,
        })
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
