use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use micam_rs::native_miot::{
    default_lib_path, NativeMiotConfig, NativeMiotSource, DEFAULT_QUEUE_CAPACITY,
};
use micam_rs::oauth::{self, TokenResolverConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let lib_path = env::var_os("MIOT_LIB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(default_lib_path);
    eprintln!("MIoT native library: {}", lib_path.display());
    match NativeMiotSource::version(&lib_path) {
        Ok(version) => eprintln!("MIoT native library version: {version}"),
        Err(error) => eprintln!("MIoT native library version unavailable: {error}"),
    }

    let token = oauth::resolve_access_token(&TokenResolverConfig {
        cloud_server: env::var("MIOT_CLOUD_SERVER").unwrap_or_else(|_| "cn".to_string()),
        redirect_uri: env::var("MIOT_OAUTH_REDIRECT_URI")
            .unwrap_or_else(|_| oauth::default_redirect_uri().to_string()),
        token_file: env::var_os("MIOT_TOKEN_FILE")
            .map(PathBuf::from)
            .or_else(|| Some(oauth::default_token_file())),
        access_token: env::var("MIOT_ACCESS_TOKEN").ok(),
        refresh_token: env::var("MIOT_REFRESH_TOKEN").ok(),
        refresh_margin_seconds: env::var("MIOT_REFRESH_MARGIN_SECONDS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or_else(oauth::default_refresh_margin_seconds),
    })
    .await?;

    let config = NativeMiotConfig {
        lib_path,
        cloud_server: env::var("MIOT_CLOUD_SERVER").unwrap_or_else(|_| "cn".to_string()),
        access_token: token.access_token,
        camera_id: required_env("CAMERA_ID")?,
        camera_model: required_env("MIOT_CAMERA_MODEL")?,
        channel_count: env::var("MIOT_CHANNEL_COUNT")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(1),
        channel: env::var("STREAM_CHANNEL")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(0),
        video_quality: env::var("MIOT_VIDEO_QUALITY")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(2),
        enable_audio: env::var("MIOT_ENABLE_AUDIO")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes")),
        pin_code: env::var("MIOT_PIN_CODE").ok(),
        queue_capacity: env::var("MIOT_QUEUE_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_QUEUE_CAPACITY),
    };

    let (_source, mut frames) = NativeMiotSource::start(&config)?;
    let started = Instant::now();
    let mut last = started;
    let mut frames_seen = 0_u64;
    let mut bytes_seen = 0_u64;

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(60), frames.recv())
            .await?
            .ok_or("native MIoT frame channel closed")?;
        frames_seen += 1;
        bytes_seen += frame.data.len() as u64;

        if frames_seen <= 10 || last.elapsed() >= Duration::from_secs(5) {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            eprintln!(
                "frame #{frames_seen}: codec={} bytes={} channel={} avg_fps={:.2} avg_mbps={:.2}",
                frame.codec,
                frame.data.len(),
                frame.channel,
                frames_seen as f64 / elapsed,
                (bytes_seen as f64 * 8.0) / elapsed / 1_000_000.0
            );
            last = Instant::now();
        }
    }
}

fn required_env(name: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}
