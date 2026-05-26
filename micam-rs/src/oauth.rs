#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_miloco_token_json() {
        let token = MiotToken::from_json(
            r#"{"access_token":"access","refresh_token":"refresh","expires_ts":2000}"#,
        )
        .unwrap();

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token, "refresh");
        assert_eq!(token.expires_ts, 2000);
    }

    #[test]
    fn refreshes_when_token_is_inside_margin() {
        let token = MiotToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_ts: 1_900,
        };

        assert!(token.needs_refresh(200, 1_800));
        assert!(!token.needs_refresh(99, 1_800));
    }

    #[test]
    fn builds_cn_and_global_oauth_hosts() {
        assert_eq!(oauth_host("cn"), "mico.api.mijia.tech");
        assert_eq!(oauth_host("de"), "de.mico.api.mijia.tech");
    }

    #[test]
    fn builds_refresh_payload_without_device_id() {
        let payload = refresh_payload("redirect", "refresh");

        assert_eq!(payload["client_id"], OAUTH2_CLIENT_ID);
        assert_eq!(payload["redirect_uri"], "redirect");
        assert_eq!(payload["refresh_token"], "refresh");
        assert!(payload.get("device_id").is_none());
    }

    #[test]
    fn builds_miloco_compatible_device_id_and_state() {
        assert_eq!(device_id("abc123"), "mico.abc123");
        assert_eq!(
            oauth_state("abc123"),
            "9bad2f560cfe536050e4eb68dbd157036ac74cde"
        );
    }

    #[test]
    fn builds_miloco_compatible_auth_url() {
        let url = build_auth_url(OAUTH2_OFFICIAL_REDIRECT_URI, "abc123", false).unwrap();
        let parsed = Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.as_str().split('?').next(), Some(OAUTH2_AUTH_URL));
        assert_eq!(pairs.get("redirect_uri"), Some(&OAUTH2_OFFICIAL_REDIRECT_URI.to_string()));
        assert_eq!(pairs.get("client_id"), Some(&OAUTH2_CLIENT_ID.to_string()));
        assert_eq!(pairs.get("response_type"), Some(&"code".to_string()));
        assert_eq!(pairs.get("device_id"), Some(&"mico.abc123".to_string()));
        assert_eq!(pairs.get("state"), Some(&oauth_state("abc123")));
        assert_eq!(pairs.get("skip_confirm"), Some(&"false".to_string()));
    }

    #[test]
    fn builds_code_exchange_payload_with_device_id() {
        let payload = access_token_payload("redirect", "abc123", "code");

        assert_eq!(payload["client_id"], OAUTH2_CLIENT_ID);
        assert_eq!(payload["redirect_uri"], "redirect");
        assert_eq!(payload["code"], "code");
        assert_eq!(payload["device_id"], "mico.abc123");
        assert!(payload.get("refresh_token").is_none());
    }
}
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use url::Url;

pub const OAUTH2_CLIENT_ID: &str = "2882303761520431603";
pub const OAUTH2_AUTH_URL: &str = "https://account.xiaomi.com/oauth2/authorize";
pub const OAUTH2_OFFICIAL_REDIRECT_URI: &str = "https://mico.api.mijia.tech/login_redirect";
const PROJECT_CODE: &str = "mico";
const TOKEN_EXPIRES_TS_RATIO: f64 = 0.7;
const DEFAULT_REFRESH_MARGIN_SECONDS: i64 = 1_800;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiotToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_ts: i64,
}

impl MiotToken {
    pub fn from_json(value: &str) -> Result<Self> {
        Ok(serde_json::from_str(value)?)
    }

    pub fn needs_refresh(&self, now_ts: i64, margin_seconds: i64) -> bool {
        self.expires_ts - now_ts <= margin_seconds
    }

    pub fn is_usable(&self) -> bool {
        !self.access_token.trim().is_empty() && !self.refresh_token.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResolverConfig {
    pub cloud_server: String,
    pub redirect_uri: String,
    pub token_file: Option<PathBuf>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub refresh_margin_seconds: i64,
}

pub async fn resolve_access_token(config: &TokenResolverConfig) -> Result<MiotToken> {
    let now = unix_timestamp();
    let mut token = load_token(&config.token_file)?;

    if token.is_none() {
        if let (Some(access_token), Some(refresh_token)) =
            (non_empty(&config.access_token), non_empty(&config.refresh_token))
        {
            token = Some(MiotToken {
                access_token: access_token.to_string(),
                refresh_token: refresh_token.to_string(),
                expires_ts: now + config.refresh_margin_seconds.max(1) + 1,
            });
        } else if let Some(refresh_token) = non_empty(&config.refresh_token) {
            token = Some(MiotToken {
                access_token: String::new(),
                refresh_token: refresh_token.to_string(),
                expires_ts: 0,
            });
        }
    }

    let Some(token) = token else {
        return Err(anyhow!(
            "MIOT_ACCESS_TOKEN or MIOT_REFRESH_TOKEN or MIOT_TOKEN_FILE is required in native mode"
        ));
    };

    if token.is_usable() && !token.needs_refresh(now, config.refresh_margin_seconds) {
        return Ok(token);
    }

    let refreshed = refresh_access_token(
        &config.cloud_server,
        &config.redirect_uri,
        &token.refresh_token,
    )
    .await?;
    save_token(&config.token_file, &refreshed)?;
    Ok(refreshed)
}

pub async fn refresh_access_token(
    cloud_server: &str,
    redirect_uri: &str,
    refresh_token: &str,
) -> Result<MiotToken> {
    if refresh_token.trim().is_empty() {
        return Err(anyhow!("MIOT_REFRESH_TOKEN is required to refresh OAuth token"));
    }
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let url = format!(
        "https://{}/app/v2/{PROJECT_CODE}/oauth/get_token",
        oauth_host(cloud_server)
    );
    let payload = refresh_payload(redirect_uri, refresh_token);
    let response = client
        .get(url)
        .query(&[("data", payload.to_string())])
        .header("content-type", "application/x-www-form-urlencoded")
        .send()
        .await?
        .error_for_status()?;
    let text = response.text().await?;
    parse_refresh_response(&text, unix_timestamp())
}

pub async fn exchange_code_for_token(
    cloud_server: &str,
    redirect_uri: &str,
    uuid: &str,
    code: &str,
) -> Result<MiotToken> {
    if code.trim().is_empty() {
        return Err(anyhow!("OAuth callback missing code"));
    }
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let url = format!(
        "https://{}/app/v2/{PROJECT_CODE}/oauth/get_token",
        oauth_host(cloud_server)
    );
    let payload = access_token_payload(redirect_uri, uuid, code);
    let response = client
        .get(url)
        .query(&[("data", payload.to_string())])
        .header("content-type", "application/x-www-form-urlencoded")
        .send()
        .await?
        .error_for_status()?;
    let text = response.text().await?;
    parse_token_response(&text, unix_timestamp())
}

pub fn save_token_file(path: &Option<PathBuf>, token: &MiotToken) -> Result<()> {
    save_token(path, token)
}

pub fn default_token_file() -> PathBuf {
    PathBuf::from("/config/miot_token.json")
}

pub fn default_redirect_uri() -> &'static str {
    OAUTH2_OFFICIAL_REDIRECT_URI
}

pub fn default_refresh_margin_seconds() -> i64 {
    DEFAULT_REFRESH_MARGIN_SECONDS
}

pub fn oauth_host(cloud_server: &str) -> String {
    let cloud_server = cloud_server.trim();
    if cloud_server.is_empty() || cloud_server == "cn" {
        "mico.api.mijia.tech".to_string()
    } else {
        format!("{cloud_server}.mico.api.mijia.tech")
    }
}

pub fn device_id(uuid: &str) -> String {
    format!("{PROJECT_CODE}.{}", uuid.trim())
}

pub fn oauth_state(uuid: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("d={}", device_id(uuid)).as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn build_auth_url(redirect_uri: &str, uuid: &str, skip_confirm: bool) -> Result<String> {
    let mut url = Url::parse(OAUTH2_AUTH_URL)?;
    url.query_pairs_mut()
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", OAUTH2_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("device_id", &device_id(uuid))
        .append_pair("state", &oauth_state(uuid))
        .append_pair("skip_confirm", if skip_confirm { "true" } else { "false" });
    Ok(url.to_string())
}

pub fn access_token_payload(redirect_uri: &str, uuid: &str, code: &str) -> Value {
    json!({
        "client_id": OAUTH2_CLIENT_ID,
        "redirect_uri": redirect_uri,
        "code": code,
        "device_id": device_id(uuid),
    })
}

pub fn refresh_payload(redirect_uri: &str, refresh_token: &str) -> Value {
    json!({
        "client_id": OAUTH2_CLIENT_ID,
        "redirect_uri": redirect_uri,
        "refresh_token": refresh_token,
    })
}

fn parse_token_response(text: &str, now_ts: i64) -> Result<MiotToken> {
    let response: Value = serde_json::from_str(text)
        .with_context(|| format!("invalid MIoT OAuth response: {text}"))?;
    if response.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(anyhow!("MIoT OAuth token request failed: {text}"));
    }
    let result = response
        .get("result")
        .ok_or_else(|| anyhow!("MIoT OAuth response missing result: {text}"))?;
    let access_token = result
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("MIoT OAuth response missing access_token"))?;
    let refresh_token = result
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("MIoT OAuth response missing refresh_token"))?;
    let expires_in = result
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("MIoT OAuth response missing expires_in"))?;

    Ok(MiotToken {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_ts: now_ts + (expires_in as f64 * TOKEN_EXPIRES_TS_RATIO) as i64,
    })
}

fn parse_refresh_response(text: &str, now_ts: i64) -> Result<MiotToken> {
    parse_token_response(text, now_ts)
}

fn load_token(path: &Option<PathBuf>) -> Result<Option<MiotToken>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read MIoT token file: {}", path.display()))?;
    Ok(Some(MiotToken::from_json(&contents)?))
}

fn save_token(path: &Option<PathBuf>, token: &MiotToken) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create token directory: {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(token)?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write MIoT token file: {}", path.display()))?;
    Ok(())
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
