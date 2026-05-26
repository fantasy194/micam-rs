# Micam Rust Bridge

High-performance Rust implementation of the Xiaomi Miloco WebSocket to RTSP bridge.

This implementation keeps the current Micam stream flow:

- Native mode: call Xiaomi's `libmiot_camera_lite.so` directly and receive raw
  camera frames through the C callback.
- Remote mode fallback: log in to Miloco and connect to `/api/miot/ws/video_stream`.
- Wait for the first H.264/H.265 keyframe.
- Pipe the stream into FFmpeg.
- Publish RTSP over TCP.
- Copy H.264 by default for the lowest CPU and latency path.

## Build

```shell
docker build -t micam-rs:local -f micam-rs/Dockerfile micam-rs
```

## Run

Copy the example environment file first:

```shell
cp micam-rs/.env.example micam-rs/.env
```

Edit `micam-rs/.env`, then run:

```shell
docker compose -f micam-rs/docker-compose.yml up -d --build
```

You can also run the image directly:

```shell
docker run --rm \
  --gpus all \
  --network host \
  -e MILOCO_BASE_URL=https://127.0.0.1:8000 \
  -e MILOCO_PASSWORD=your_miloco_password_md5 \
  -e CAMERA_ID=1234567890 \
  -e RTSP_URL=rtsp://127.0.0.1:8554/your_homekit_stream \
  -e VIDEO_CODEC=h264 \
  -e STREAM_CHANNEL=0 \
  -e FFMPEG_GPU_ENABLED=false \
  micam-rs:local
```

## Native MIoT Mode

Native mode is the default. It removes the Miloco service from the runtime path
and uses the same Xiaomi camera C library directly from Rust:

```shell
MILOCO_MODE=native
MIOT_REFRESH_TOKEN=your_xiaomi_home_refresh_token
MIOT_TOKEN_FILE=/config/miot_token.json
MIOT_OAUTH_REDIRECT_URI=https://mico.api.mijia.tech/login_redirect
MIOT_CAMERA_MODEL=your_camera_model
MIOT_CLOUD_SERVER=cn
MIOT_CHANNEL_COUNT=1
MIOT_VIDEO_QUALITY=2
MIOT_ENABLE_AUDIO=false
VIDEO_CODEC=h264
FFMPEG_GPU_ENABLED=false
FFMPEG_LOW_LATENCY=true
```

For N150-class Intel hosts, the lowest CPU path is native H.264 stream copy. If
HomeKit or the RTSP receiver rejects the copied stream, enable QSV hardware
transcoding:

```shell
FFMPEG_GPU_ENABLED=true
FFMPEG_HWACCEL=qsv
FFMPEG_VIDEO_ENCODER=h264_qsv
```

Probe the native camera path without FFmpeg:

```shell
docker run --rm --network host \
  -e MIOT_ACCESS_TOKEN=your_xiaomi_home_access_token \
  -e MIOT_REFRESH_TOKEN=your_xiaomi_home_refresh_token \
  -e CAMERA_ID=1234567890 \
  -e MIOT_CAMERA_MODEL=your_camera_model \
  -v ./config:/config \
  micam-rs:local miot_native_probe
```

If `MIOT_TOKEN_FILE` exists and its token is still valid, native mode uses it
without a network refresh. If it is missing or near expiry, the bridge uses
`MIOT_REFRESH_TOKEN` or the cached refresh token to call Xiaomi's MIoT OAuth
refresh endpoint, then writes the new access and refresh tokens back to
`MIOT_TOKEN_FILE`. This is required because Xiaomi refresh tokens rotate.

Set `MILOCO_MODE=remote` to use the previous Miloco WebSocket path.

## First OAuth Login

For the first token, run the local OAuth helper and open its page:

```shell
docker compose --profile oauth up miot-oauth
```

Then open:

```text
http://127.0.0.1:18080
```

The page generates a Xiaomi OAuth URL with the Miloco-compatible parameters:

- `redirect_uri=https://mico.api.mijia.tech/login_redirect`
- `device_id=mico.${MIOT_OAUTH_UUID}`
- `state=sha1("d=mico.${MIOT_OAUTH_UUID}")`

After Xiaomi login, the official redirect page may ask for a redirect address.
Use:

```text
http://127.0.0.1:18080
```

It will jump to `/api/miot/xiaomi_home_callback`, where the helper exchanges the
OAuth `code` for access and refresh tokens and writes them to
`/config/miot_token.json`. The normal `micam-rs` service then reuses that file
and keeps it refreshed automatically.

## GPU Encoding

The default runtime path does not transcode. Set `FFMPEG_GPU_ENABLED=true` when
the target requires a HomeKit-compatible transcode.

NVIDIA CUDA/NVENC:

```shell
FFMPEG_HWACCEL=cuda
FFMPEG_VIDEO_ENCODER=h264_nvenc
```

NVIDIA NVENC example:

```shell
docker run --rm --gpus all \
  -e FFMPEG_HWACCEL=cuda \
  -e FFMPEG_VIDEO_ENCODER=hevc_nvenc \
  micam-rs:local
```

Intel QSV or VAAPI examples:

```shell
docker run --rm --device /dev/dri \
  -e FFMPEG_HWACCEL=qsv \
  -e FFMPEG_VIDEO_ENCODER=h264_qsv \
  micam-rs:local

docker run --rm --device /dev/dri \
  -e FFMPEG_HWACCEL=vaapi \
  -e FFMPEG_VIDEO_ENCODER=h264_vaapi \
  -e FFMPEG_VAAPI_DEVICE=/dev/dri/renderD128 \
  micam-rs:local
```

When VAAPI is enabled through `FFMPEG_HWACCEL=vaapi` or an encoder ending in
`_vaapi`, the bridge automatically injects:

```shell
-vaapi_device ${FFMPEG_VAAPI_DEVICE:-/dev/dri/renderD128}
-vf format=nv12,hwupload
```

Set `FFMPEG_GPU_ENABLED=false` to bypass hardware transcoding and use
`-c:v copy -c:a copy`. Use `FFMPEG_EXTRA_ARGS` for final output options that
must be appended before the RTSP URL, for example `-b:v 2500k -maxrate 3000k`.

The container must include an FFmpeg build that supports the selected encoder,
and Docker must expose the GPU device required by that encoder.

## Reusing the Miloco Client

The crate now exposes the Miloco login and raw camera WebSocket logic through
`micam_rs::miloco::MilocoClient`:

```rust
use micam_rs::miloco::MilocoClient;

let mut client = MilocoClient::new("https://miloco:8000")?;
client.login("admin", "your_miloco_password_md5").await?;

client
    .stream_raw_video("1190866232", 0, |frame| async move {
        // frame is the raw camera video payload from Miloco.
        Ok(())
    })
    .await?;
```

Useful endpoints wrapped by the client:

- `POST /api/auth/login`
- `GET /api/auth/register-status`
- `GET /api/miot/camera_list`
- `GET /api/miot/xiaomi_home_callback`
- `WS /api/miot/ws/video_stream`

The image also includes two helper binaries:

```shell
miloco_probe
miloco_oauth_redirect
```

`miloco_probe` logs in, prints camera metadata, and receives raw video frames
without starting FFmpeg. `miloco_oauth_redirect` is a small local HTML callback
helper for cases where you need to inspect an OAuth redirect path before passing
it to Miloco's `/api/miot/xiaomi_home_callback`.

## Configuration

| Environment | Default | Description |
| --- | --- | --- |
| `MILOCO_MODE` | `native` | `native` uses `libmiot_camera_lite.so`; `remote` uses Miloco WebSocket |
| `MILOCO_BASE_URL` | `https://miloco:8000` | Miloco base URL |
| `MILOCO_USERNAME` | `admin` | Miloco username |
| `MILOCO_PASSWORD` | required for remote | Miloco password MD5 |
| `CAMERA_ID` | required | Camera DID |
| `RTSP_URL` | `rtsp://0.0.0.0:8554/live` | Target RTSP URL |
| `VIDEO_CODEC` | `h264` | Input stream codec, usually `h264` for Xiaomi H.264 streams |
| `STREAM_CHANNEL` | `0` | Miloco stream channel |
| `FFMPEG_GPU_ENABLED` | `false` | Enable hardware/HomeKit transcode path; false uses stream copy |
| `FFMPEG_VIDEO_ENCODER` | `h264_qsv` | FFmpeg video encoder for HomeKit H.264 output |
| `FFMPEG_HWACCEL` | `qsv` | FFmpeg `-hwaccel` value |
| `FFMPEG_VAAPI_DEVICE` | `/dev/dri/renderD128` | VAAPI render device when VAAPI mode is used |
| `FFMPEG_EXTRA_ARGS` | empty | Extra FFmpeg output args appended before the RTSP URL |
| `FFMPEG_LOW_LATENCY` | `true` | Add low-latency FFmpeg muxing flags |
| `MIOT_ACCESS_TOKEN` | optional | Xiaomi Home OAuth access token; used only when token cache is absent |
| `MIOT_REFRESH_TOKEN` | required for auto refresh | Xiaomi Home OAuth refresh token |
| `MIOT_TOKEN_FILE` | `/config/miot_token.json` | Persistent token cache path |
| `MIOT_OAUTH_REDIRECT_URI` | `https://mico.api.mijia.tech/login_redirect` | Redirect URI used by Xiaomi OAuth code exchange and refresh |
| `MIOT_REFRESH_MARGIN_SECONDS` | `1800` | Refresh when token expires within this many seconds |
| `MIOT_OAUTH_BIND` | `0.0.0.0:18080` | Bind address for the first-login OAuth helper |
| `MIOT_OAUTH_PUBLIC_BASE_URL` | `http://127.0.0.1:18080` | Browser-facing base URL for the OAuth helper |
| `MIOT_OAUTH_CALLBACK_PATH` | `/api/miot/xiaomi_home_callback` | Local callback path used by the OAuth helper |
| `MIOT_OAUTH_UUID` | `micam-rs` | Stable UUID portion used for `device_id=mico.<uuid>` |
| `MIOT_OAUTH_SKIP_CONFIRM` | `false` | Xiaomi OAuth `skip_confirm` flag for the first-login helper |
| `MIOT_CLOUD_SERVER` | `cn` | Xiaomi cloud region |
| `MIOT_CAMERA_MODEL` | required for native | Xiaomi camera model |
| `MIOT_CHANNEL_COUNT` | `1` | Number of camera channels |
| `MIOT_VIDEO_QUALITY` | `2` | Xiaomi native stream quality |
| `MIOT_ENABLE_AUDIO` | `false` | Request audio from native camera source |
| `MIOT_LIB_PATH` | `/usr/local/lib/libmiot_camera_lite.so` | Path to Xiaomi native camera library |
| `MIOT_QUEUE_CAPACITY` | `8` | Small frame queue for low latency |
