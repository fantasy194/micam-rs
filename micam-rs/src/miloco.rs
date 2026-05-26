use std::error::Error;
use std::fmt;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{COOKIE, SET_COOKIE};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::COOKIE as WS_COOKIE;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

#[derive(Clone, Debug)]
pub struct MilocoClient {
    base_url: String,
    http: reqwest::Client,
    access_cookie: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NormalResponse<T> {
    pub code: i32,
    pub message: String,
    pub data: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct LoginData {
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterStatusData {
    pub is_registered: bool,
}

#[derive(Debug, Deserialize)]
pub struct CameraInfo {
    #[serde(default)]
    pub did: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub channel_count: Option<u8>,
}

#[derive(Debug)]
pub enum MilocoError {
    Api { code: i32, message: String },
    MissingAccessCookie,
    InvalidBaseUrl(String),
}

impl fmt::Display for MilocoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { code, message } => write!(f, "Miloco API error {code}: {message}"),
            Self::MissingAccessCookie => {
                write!(f, "Miloco login did not return access_token cookie")
            }
            Self::InvalidBaseUrl(url) => write!(f, "invalid Miloco base URL: {url}"),
        }
    }
}

impl Error for MilocoError {}

impl MilocoClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        Ok(Self {
            base_url: trim_slash(base_url.into()),
            http,
            access_cookie: None,
        })
    }

    pub fn oauth_callback_url(&self) -> String {
        format!("{}/api/miot/xiaomi_home_callback", self.base_url)
    }

    pub async fn register_status(
        &self,
    ) -> Result<RegisterStatusData, Box<dyn Error + Send + Sync>> {
        self.get_json("/api/auth/register-status").await
    }

    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<LoginData, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/api/auth/login", self.base_url);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await?
            .error_for_status()?;

        let cookie = extract_access_cookie(response.headers())?;
        let body: NormalResponse<LoginData> = response.json().await?;
        let data = into_data(body)?;
        self.access_cookie = Some(cookie);
        Ok(data)
    }

    pub async fn camera_list(&self) -> Result<Vec<CameraInfo>, Box<dyn Error + Send + Sync>> {
        self.get_json("/api/miot/camera_list").await
    }

    pub async fn stream_raw_video<F, Fut>(
        &self,
        camera_id: &str,
        channel: u8,
        mut on_frame: F,
    ) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        F: FnMut(Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = Result<(), Box<dyn Error + Send + Sync>>>,
    {
        let cookie = self
            .access_cookie
            .as_ref()
            .ok_or(MilocoError::MissingAccessCookie)?;
        let ws_url = self.websocket_url(camera_id, channel)?;
        let mut request = ws_url.into_client_request()?;
        request.headers_mut().insert(WS_COOKIE, cookie.parse()?);

        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        let (mut websocket, _) =
            connect_async_tls_with_config(request, None, false, Some(Connector::NativeTls(tls)))
                .await?;
        while let Some(message) = websocket.next().await {
            match message? {
                Message::Binary(data) => on_frame(data.to_vec()).await?,
                Message::Ping(payload) => websocket.send(Message::Pong(payload)).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }

        Ok(())
    }

    async fn get_json<T>(&self, path: &str) -> Result<T, Box<dyn Error + Send + Sync>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut request = self.http.get(format!("{}{}", self.base_url, path));
        if let Some(cookie) = &self.access_cookie {
            request = request.header(COOKIE, cookie);
        }
        let body: NormalResponse<T> = request.send().await?.error_for_status()?.json().await?;
        into_data(body)
    }

    fn websocket_url(&self, camera_id: &str, channel: u8) -> Result<String, MilocoError> {
        let scheme = if self.base_url.starts_with("https://") {
            "wss://"
        } else if self.base_url.starts_with("http://") {
            "ws://"
        } else {
            return Err(MilocoError::InvalidBaseUrl(self.base_url.clone()));
        };
        let without_scheme = self
            .base_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .ok_or_else(|| MilocoError::InvalidBaseUrl(self.base_url.clone()))?;
        Ok(format!(
            "{scheme}{without_scheme}/api/miot/ws/video_stream?camera_id={camera_id}&channel={channel}"
        ))
    }
}

fn into_data<T>(body: NormalResponse<T>) -> Result<T, Box<dyn Error + Send + Sync>> {
    if body.code != 0 {
        return Err(Box::new(MilocoError::Api {
            code: body.code,
            message: body.message,
        }));
    }
    body.data.ok_or_else(|| {
        Box::new(MilocoError::Api {
            code: body.code,
            message: "Miloco response data is empty".to_string(),
        }) as Box<dyn Error + Send + Sync>
    })
}

fn extract_access_cookie(
    headers: &reqwest::header::HeaderMap,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    for value in headers.get_all(SET_COOKIE) {
        let cookie = value.to_str()?;
        if let Some(token) = cookie.split(';').next() {
            if token.starts_with("access_token=") {
                return Ok(token.to_string());
            }
        }
    }
    Err(Box::new(MilocoError::MissingAccessCookie))
}

fn trim_slash(mut value: String) -> String {
    while value.ends_with('/') {
        value.pop();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_https_websocket_url() {
        let client = MilocoClient::new("https://miloco:8000/").unwrap();
        assert_eq!(
            client.websocket_url("1190866232", 0).unwrap(),
            "wss://miloco:8000/api/miot/ws/video_stream?camera_id=1190866232&channel=0"
        );
    }

    #[test]
    fn builds_http_websocket_url() {
        let client = MilocoClient::new("http://127.0.0.1:8000").unwrap();
        assert_eq!(
            client.websocket_url("cam", 1).unwrap(),
            "ws://127.0.0.1:8000/api/miot/ws/video_stream?camera_id=cam&channel=1"
        );
    }

    #[test]
    fn extracts_access_token_cookie_only() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            SET_COOKIE,
            "access_token=abc.def; HttpOnly; Path=/".parse().unwrap(),
        );
        assert_eq!(extract_access_cookie(&headers).unwrap(), "access_token=abc.def");
    }

    #[test]
    fn exposes_miloco_oauth_callback_url() {
        let client = MilocoClient::new("https://miloco:8000/").unwrap();

        assert_eq!(
            client.oauth_callback_url(),
            "https://miloco:8000/api/miot/xiaomi_home_callback"
        );
    }
}
