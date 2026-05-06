# Micam Rust Bridge

High-performance Rust implementation of the Xiaomi Miloco WebSocket to RTSP bridge.

This implementation keeps the current Micam stream flow:

- Log in to Miloco.
- Check `/api/miot/login_status`.
- Connect to `/api/miot/ws/video_stream`.
- Wait for the first H.264/H.265 keyframe.
- Pipe the stream into FFmpeg.
- Publish RTSP over TCP.
- Encode to HomeKit-friendly H.264/AAC by default using CUDA/NVENC.

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
  -e VIDEO_CODEC=hevc \
  -e STREAM_CHANNEL=0 \
  -e FFMPEG_HWACCEL=cuda \
  -e FFMPEG_VIDEO_ENCODER=h264_nvenc \
  micam-rs:local
```

## GPU Encoding

By default the bridge transcodes to HomeKit-friendly H.264/AAC using NVIDIA
CUDA/NVENC:

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
  -e FFMPEG_VIDEO_ENCODER=h264_qsv \
  micam-rs:local

docker run --rm --device /dev/dri \
  -e FFMPEG_HWACCEL=vaapi \
  -e FFMPEG_VIDEO_ENCODER=hevc_vaapi \
  micam-rs:local
```

The container must include an FFmpeg build that supports the selected encoder,
and Docker must expose the GPU device required by that encoder.

## Configuration

| Environment | Default | Description |
| --- | --- | --- |
| `MILOCO_BASE_URL` | `https://miloco:8000` | Miloco base URL |
| `MILOCO_USERNAME` | `admin` | Miloco username |
| `MILOCO_PASSWORD` | required | Miloco password MD5 |
| `CAMERA_ID` | required | Camera DID |
| `RTSP_URL` | `rtsp://0.0.0.0:8554/live` | Target RTSP URL |
| `VIDEO_CODEC` | `hevc` | Input stream codec, usually `hevc` or `h264` |
| `STREAM_CHANNEL` | `0` | Miloco stream channel |
| `FFMPEG_VIDEO_ENCODER` | `h264_nvenc` | FFmpeg video encoder for HomeKit H.264 output |
| `FFMPEG_HWACCEL` | `cuda` | FFmpeg `-hwaccel` value |
