use std::env;
use std::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};

use micam_rs::miloco::MilocoClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let base_url =
        env::var("MILOCO_BASE_URL").unwrap_or_else(|_| "https://miloco:8000".to_string());
    let username = env::var("MILOCO_USERNAME").unwrap_or_else(|_| "admin".to_string());
    let password = env::var("MILOCO_PASSWORD")?;
    let camera_id = env::var("CAMERA_ID")?;
    let channel = env::var("STREAM_CHANNEL")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);

    let mut client = MilocoClient::new(base_url)?;
    let login = client.login(&username, &password).await?;
    eprintln!("Miloco login ok: {}", login.username);
    eprintln!("Xiaomi OAuth callback URL: {}", client.oauth_callback_url());

    if let Ok(cameras) = client.camera_list().await {
        eprintln!("Miloco camera count: {}", cameras.len());
        for camera in cameras {
            eprintln!(
                "camera did={} name={:?} model={:?} channels={:?}",
                camera.did, camera.name, camera.model, camera.channel_count
            );
        }
    }

    let frame_count = AtomicU64::new(0);
    let byte_count = AtomicU64::new(0);
    client
        .stream_raw_video(&camera_id, channel, |frame| {
            let frames = frame_count.fetch_add(1, Ordering::Relaxed) + 1;
            let bytes =
                byte_count.fetch_add(frame.len() as u64, Ordering::Relaxed) + frame.len() as u64;
            async move {
                if frames <= 10 || frames % 100 == 0 {
                    eprintln!(
                        "raw video frame #{frames}: {} bytes, total {bytes} bytes",
                        frame.len()
                    );
                }
                Ok(())
            }
        })
        .await
}
