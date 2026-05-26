use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_ulonglong, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use crate::oauth::OAUTH2_CLIENT_ID;
use libloading::Library;
use tokio::sync::mpsc;

const VIDEO_H264: u32 = 4;
const VIDEO_H265: u32 = 5;
const MIOT_CAMERA_CLIENT_ID: &str = OAUTH2_CLIENT_ID;
pub const DEFAULT_QUEUE_CAPACITY: usize = 8;

static RAW_FRAME_TX: OnceLock<Mutex<Option<mpsc::Sender<NativeFrame>>>> = OnceLock::new();
static STATUS_TX: OnceLock<Mutex<Option<mpsc::Sender<i32>>>> = OnceLock::new();

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NativeFrameHeader {
    pub codec_id: c_uint,
    pub length: c_uint,
    pub timestamp: c_ulonglong,
    pub sequence: c_uint,
    pub frame_type: c_uint,
    pub channel: c_uchar,
}

#[repr(C)]
struct NativeCameraInfo {
    did: *const c_char,
    model: *const c_char,
    channel_count: c_uchar,
}

#[repr(C)]
struct NativeCameraConfig {
    video_qualities: *const c_uchar,
    enable_audio: bool,
    pin_code: *const c_char,
}

type CameraHandle = *mut c_void;
type StatusCallback = extern "C" fn(c_int);
type RawDataCallback = extern "C" fn(*const NativeFrameHeader, *const c_uchar);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeFrame {
    pub codec_id: u32,
    pub codec: &'static str,
    pub data: Vec<u8>,
    pub timestamp: u64,
    pub sequence: u32,
    pub channel: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMiotConfig {
    pub lib_path: PathBuf,
    pub cloud_server: String,
    pub access_token: String,
    pub camera_id: String,
    pub camera_model: String,
    pub channel_count: u8,
    pub channel: u8,
    pub video_quality: u8,
    pub enable_audio: bool,
    pub pin_code: Option<String>,
    pub queue_capacity: usize,
}

impl NativeMiotConfig {
    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity.max(1)
    }
}

pub struct NativeMiotSource {
    lib: NativeMiotLibrary,
    handle: CameraHandle,
    _did: CString,
    _model: CString,
    pin_code: Option<CString>,
    qualities: Vec<u8>,
    channel: u8,
}

unsafe impl Send for NativeMiotSource {}

impl NativeMiotSource {
    pub fn start(config: &NativeMiotConfig) -> Result<(Self, mpsc::Receiver<NativeFrame>)> {
        if config.access_token.trim().is_empty() {
            return Err(anyhow!("MIOT_ACCESS_TOKEN is required in native mode"));
        }
        if config.camera_id.trim().is_empty() {
            return Err(anyhow!("CAMERA_ID is required in native mode"));
        }
        if config.camera_model.trim().is_empty() {
            return Err(anyhow!("MIOT_CAMERA_MODEL is required in native mode"));
        }
        if config.channel >= config.channel_count.max(1) {
            return Err(anyhow!(
                "STREAM_CHANNEL {} is outside MIOT_CHANNEL_COUNT {}",
                config.channel,
                config.channel_count
            ));
        }

        let lib = NativeMiotLibrary::load(&config.lib_path)?;
        let host = miot_host(&config.cloud_server)?;
        let client_id = CString::new(MIOT_CAMERA_CLIENT_ID)?;
        let access_token = CString::new(config.access_token.as_str())?;
        let did = CString::new(config.camera_id.as_str())?;
        let model = CString::new(config.camera_model.as_str())?;
        let pin_code = match &config.pin_code {
            Some(pin) if !pin.trim().is_empty() => Some(CString::new(pin.as_str())?),
            _ => None,
        };

        unsafe {
            (lib.miot_camera_init)(host.as_ptr(), client_id.as_ptr(), access_token.as_ptr());
        }

        let info = NativeCameraInfo {
            did: did.as_ptr(),
            model: model.as_ptr(),
            channel_count: config.channel_count.max(1),
        };
        let handle = unsafe { (lib.miot_camera_new)(&info) };
        if handle.is_null() {
            unsafe {
                (lib.miot_camera_deinit)();
            }
            return Err(anyhow!("miot_camera_new returned null"));
        }

        let (frame_tx, frame_rx) = mpsc::channel(config.queue_capacity());
        *RAW_FRAME_TX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("raw frame sender mutex poisoned") = Some(frame_tx);

        let (status_tx, _status_rx) = mpsc::channel(8);
        *STATUS_TX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("status sender mutex poisoned") = Some(status_tx);

        let status_result =
            unsafe { (lib.miot_camera_register_status_changed)(handle, status_callback) };
        if status_result != 0 {
            cleanup_sender_slots();
            unsafe {
                (lib.miot_camera_free)(handle);
                (lib.miot_camera_deinit)();
            }
            return Err(anyhow!(
                "miot_camera_register_status_changed failed: {status_result}"
            ));
        }

        let raw_result =
            unsafe { (lib.miot_camera_register_raw_data)(handle, raw_data_callback, config.channel) };
        if raw_result != 0 {
            cleanup_sender_slots();
            unsafe {
                (lib.miot_camera_unregister_status_changed)(handle);
                (lib.miot_camera_free)(handle);
                (lib.miot_camera_deinit)();
            }
            return Err(anyhow!("miot_camera_register_raw_data failed: {raw_result}"));
        }

        let mut qualities = vec![config.video_quality; config.channel_count.max(1) as usize];
        qualities.push(0);
        let native_config = NativeCameraConfig {
            video_qualities: qualities.as_ptr(),
            enable_audio: config.enable_audio,
            pin_code: pin_code.as_ref().map_or(ptr::null(), |pin| pin.as_ptr()),
        };
        let start_result = unsafe { (lib.miot_camera_start)(handle, &native_config) };
        if start_result != 0 {
            cleanup_sender_slots();
            unsafe {
                (lib.miot_camera_unregister_raw_data)(handle, config.channel);
                (lib.miot_camera_unregister_status_changed)(handle);
                (lib.miot_camera_free)(handle);
                (lib.miot_camera_deinit)();
            }
            return Err(anyhow!("miot_camera_start failed: {start_result}"));
        }

        Ok((
            Self {
                lib,
                handle,
                _did: did,
                _model: model,
                pin_code,
                qualities,
                channel: config.channel,
            },
            frame_rx,
        ))
    }

    pub fn version<P: AsRef<Path>>(lib_path: P) -> Result<String> {
        let lib = NativeMiotLibrary::load(lib_path.as_ref())?;
        let version = unsafe { (lib.miot_camera_version)() };
        if version.is_null() {
            return Err(anyhow!("miot_camera_version returned null"));
        }
        Ok(unsafe { CStr::from_ptr(version) }
            .to_string_lossy()
            .to_string())
    }
}

impl Drop for NativeMiotSource {
    fn drop(&mut self) {
        cleanup_sender_slots();
        unsafe {
            let _ = (self.lib.miot_camera_stop)(self.handle);
            let _ = (self.lib.miot_camera_unregister_raw_data)(self.handle, self.channel);
            let _ = (self.lib.miot_camera_unregister_status_changed)(self.handle);
            (self.lib.miot_camera_free)(self.handle);
            (self.lib.miot_camera_deinit)();
        }
        let _ = self.pin_code.take();
        self.qualities.clear();
    }
}

struct NativeMiotLibrary {
    _library: Library,
    miot_camera_init: unsafe extern "C" fn(*const c_char, *const c_char, *const c_char) -> c_int,
    miot_camera_deinit: unsafe extern "C" fn(),
    miot_camera_new: unsafe extern "C" fn(*const NativeCameraInfo) -> CameraHandle,
    miot_camera_free: unsafe extern "C" fn(CameraHandle),
    miot_camera_start: unsafe extern "C" fn(CameraHandle, *const NativeCameraConfig) -> c_int,
    miot_camera_stop: unsafe extern "C" fn(CameraHandle) -> c_int,
    miot_camera_version: unsafe extern "C" fn() -> *const c_char,
    miot_camera_register_status_changed:
        unsafe extern "C" fn(CameraHandle, StatusCallback) -> c_int,
    miot_camera_unregister_status_changed: unsafe extern "C" fn(CameraHandle) -> c_int,
    miot_camera_register_raw_data:
        unsafe extern "C" fn(CameraHandle, RawDataCallback, c_uchar) -> c_int,
    miot_camera_unregister_raw_data: unsafe extern "C" fn(CameraHandle, c_uchar) -> c_int,
}

impl NativeMiotLibrary {
    fn load(path: &Path) -> Result<Self> {
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load MIoT camera library: {}", path.display()))?;
        unsafe {
            let miot_camera_init = *library.get(b"miot_camera_init")?;
            let miot_camera_deinit = *library.get(b"miot_camera_deinit")?;
            let miot_camera_new = *library.get(b"miot_camera_new")?;
            let miot_camera_free = *library.get(b"miot_camera_free")?;
            let miot_camera_start = *library.get(b"miot_camera_start")?;
            let miot_camera_stop = *library.get(b"miot_camera_stop")?;
            let miot_camera_version = *library.get(b"miot_camera_version")?;
            let miot_camera_register_status_changed =
                *library.get(b"miot_camera_register_status_changed")?;
            let miot_camera_unregister_status_changed =
                *library.get(b"miot_camera_unregister_status_changed")?;
            let miot_camera_register_raw_data = *library.get(b"miot_camera_register_raw_data")?;
            let miot_camera_unregister_raw_data =
                *library.get(b"miot_camera_unregister_raw_data")?;

            Ok(Self {
                _library: library,
                miot_camera_init,
                miot_camera_deinit,
                miot_camera_new,
                miot_camera_free,
                miot_camera_start,
                miot_camera_stop,
                miot_camera_version,
                miot_camera_register_status_changed,
                miot_camera_unregister_status_changed,
                miot_camera_register_raw_data,
                miot_camera_unregister_raw_data,
            })
        }
    }
}

pub fn codec_name(codec_id: u32) -> Option<&'static str> {
    match codec_id {
        VIDEO_H264 => Some("h264"),
        VIDEO_H265 => Some("hevc"),
        _ => None,
    }
}

pub fn default_lib_path() -> PathBuf {
    PathBuf::from("/usr/local/lib/libmiot_camera_lite.so")
}

fn miot_host(cloud_server: &str) -> Result<CString> {
    let server = cloud_server.trim();
    let host = if server.is_empty() || server == "cn" {
        "oauth.api.io.mi.com".to_string()
    } else {
        format!("{server}.oauth.api.io.mi.com")
    };
    Ok(CString::new(host)?)
}

extern "C" fn status_callback(status: c_int) {
    if let Some(slot) = STATUS_TX.get() {
        if let Some(tx) = slot.lock().expect("status sender mutex poisoned").as_ref() {
            let _ = tx.try_send(status);
        }
    }
}

extern "C" fn raw_data_callback(header: *const NativeFrameHeader, data: *const c_uchar) {
    if header.is_null() || data.is_null() {
        return;
    }
    let header = unsafe { *header };
    let Some(codec) = codec_name(header.codec_id) else {
        return;
    };
    let payload = unsafe { std::slice::from_raw_parts(data, header.length as usize) }.to_vec();
    let frame = NativeFrame {
        codec_id: header.codec_id,
        codec,
        data: payload,
        timestamp: header.timestamp,
        sequence: header.sequence,
        channel: header.channel,
    };

    if let Some(slot) = RAW_FRAME_TX.get() {
        if let Some(tx) = slot.lock().expect("raw frame sender mutex poisoned").as_ref() {
            let _ = tx.try_send(frame);
        }
    }
}

fn cleanup_sender_slots() {
    if let Some(slot) = RAW_FRAME_TX.get() {
        *slot.lock().expect("raw frame sender mutex poisoned") = None;
    }
    if let Some(slot) = STATUS_TX.get() {
        *slot.lock().expect("status sender mutex poisoned") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_frame_header_matches_miloco_ctypes_layout() {
        assert_eq!(std::mem::size_of::<NativeFrameHeader>(), 32);
    }

    #[test]
    fn builds_miot_host_for_cn_and_global_regions() {
        assert_eq!(
            miot_host("cn").unwrap().to_str().unwrap(),
            "oauth.api.io.mi.com"
        );
        assert_eq!(
            miot_host("de").unwrap().to_str().unwrap(),
            "de.oauth.api.io.mi.com"
        );
    }

    #[test]
    fn enforces_non_zero_queue_capacity() {
        let config = NativeMiotConfig {
            lib_path: default_lib_path(),
            cloud_server: "cn".to_string(),
            access_token: "token".to_string(),
            camera_id: "did".to_string(),
            camera_model: "model".to_string(),
            channel_count: 1,
            channel: 0,
            video_quality: 2,
            enable_audio: false,
            pin_code: None,
            queue_capacity: 0,
        };

        assert_eq!(config.queue_capacity(), 1);
    }

    #[test]
    fn default_queue_capacity_is_small_for_low_latency() {
        assert_eq!(DEFAULT_QUEUE_CAPACITY, 8);
    }
}
