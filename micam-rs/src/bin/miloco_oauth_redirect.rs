use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use micam_rs::oauth;
use url::Url;

#[derive(Debug, Clone)]
struct OAuthServerConfig {
    bind: String,
    public_base_url: String,
    callback_path: String,
    cloud_server: String,
    redirect_uri: String,
    uuid: String,
    token_file: Option<PathBuf>,
    skip_confirm: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = OAuthServerConfig::from_env();
    let listener = TcpListener::bind(&config.bind)?;
    let auth_url = oauth::build_auth_url(&config.redirect_uri, &config.uuid, config.skip_confirm)?;
    let callback_url = format!(
        "{}{}",
        config.public_base_url.trim_end_matches('/'),
        config.callback_path
    );

    eprintln!("MIoT OAuth helper listening on http://{}/", config.bind);
    eprintln!("Open local page: {}", config.public_base_url);
    eprintln!("Xiaomi OAuth URL: {auth_url}");
    eprintln!("Local callback URL: {callback_url}");

    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut request = [0_u8; 8192];
        let size = stream.read(&mut request)?;
        let first_line = String::from_utf8_lossy(&request[..size])
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let response = handle_request(path, &config, &auth_url, &callback_url).await;
        stream.write_all(response.as_bytes())?;
    }

    Ok(())
}

impl OAuthServerConfig {
    fn from_env() -> Self {
        Self {
            bind: env::var("MIOT_OAUTH_BIND").unwrap_or_else(|_| "0.0.0.0:18080".to_string()),
            public_base_url: env::var("MIOT_OAUTH_PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:18080".to_string()),
            callback_path: env::var("MIOT_OAUTH_CALLBACK_PATH")
                .unwrap_or_else(|_| "/api/miot/xiaomi_home_callback".to_string()),
            cloud_server: env::var("MIOT_CLOUD_SERVER").unwrap_or_else(|_| "cn".to_string()),
            redirect_uri: env::var("MIOT_OAUTH_REDIRECT_URI")
                .unwrap_or_else(|_| oauth::default_redirect_uri().to_string()),
            uuid: env::var("MIOT_OAUTH_UUID").unwrap_or_else(|_| "micam-rs".to_string()),
            token_file: env::var_os("MIOT_TOKEN_FILE")
                .map(PathBuf::from)
                .or_else(|| Some(oauth::default_token_file())),
            skip_confirm: env_bool("MIOT_OAUTH_SKIP_CONFIRM", false),
        }
    }
}

async fn handle_request(
    path: &str,
    config: &OAuthServerConfig,
    auth_url: &str,
    callback_url: &str,
) -> String {
    let path_url = match Url::parse(&format!("http://localhost{path}")) {
        Ok(value) => value,
        Err(error) => {
            return html_response(
                400,
                &render_error("请求无法解析", &format!("invalid request path: {error}")),
            );
        }
    };

    if path_url.path() == config.callback_path {
        return handle_callback(path_url, config).await;
    }

    html_response(
        200,
        &render_index(auth_url, callback_url, &config.redirect_uri, &config.uuid),
    )
}

async fn handle_callback(path_url: Url, config: &OAuthServerConfig) -> String {
    let query: std::collections::HashMap<_, _> = path_url.query_pairs().into_owned().collect();
    let Some(code) = query.get("code").filter(|value| !value.trim().is_empty()) else {
        return html_response(400, &render_error("授权失败", "OAuth callback missing code"));
    };
    let Some(state) = query.get("state").filter(|value| !value.trim().is_empty()) else {
        return html_response(400, &render_error("授权失败", "OAuth callback missing state"));
    };
    let expected_state = oauth::oauth_state(&config.uuid);
    if state != &expected_state {
        return html_response(
            400,
            &render_error(
                "授权失败",
                &format!("state mismatch; expected {expected_state}, got {state}"),
            ),
        );
    }

    match oauth::exchange_code_for_token(
        &config.cloud_server,
        &config.redirect_uri,
        &config.uuid,
        code,
    )
    .await
    {
        Ok(token) => match oauth::save_token_file(&config.token_file, &token) {
            Ok(()) => html_response(200, &render_success(&config.token_file, token.expires_ts)),
            Err(error) => html_response(
                500,
                &render_error("Token 已获取但保存失败", &format!("{error:#}")),
            ),
        },
        Err(error) => html_response(502, &render_error("Token 获取失败", &format!("{error:#}"))),
    }
}

fn html_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn render_index(auth_url: &str, callback_url: &str, redirect_uri: &str, uuid: &str) -> String {
    let auth_url = escape_html(auth_url);
    let callback_url = escape_html(callback_url);
    let redirect_uri = escape_html(redirect_uri);
    let uuid = escape_html(uuid);

    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>MIoT OAuth</title>
  <style>
    body {{ margin: 0; font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #f6f7f9; color: #17202a; }}
    main {{ min-height: 100vh; display: grid; place-items: center; padding: 24px; box-sizing: border-box; }}
    section {{ width: min(760px, 100%); }}
    h1 {{ margin: 0 0 14px; font-size: 28px; line-height: 1.2; }}
    p {{ margin: 0 0 14px; line-height: 1.6; }}
    a.button {{ display: inline-flex; align-items: center; justify-content: center; min-height: 42px; padding: 0 16px; border-radius: 6px; background: #1f6feb; color: #fff; text-decoration: none; font-weight: 650; }}
    dl {{ display: grid; grid-template-columns: 150px 1fr; gap: 8px 12px; margin: 22px 0 0; }}
    dt {{ color: #5f6b7a; }}
    dd {{ margin: 0; overflow-wrap: anywhere; }}
    code {{ padding: 2px 5px; background: #ffffff; border: 1px solid #d8dee4; border-radius: 5px; }}
  </style>
</head>
<body>
  <main>
    <section>
      <h1>MIoT OAuth 首次授权</h1>
      <p>点击登录后完成小米账号授权。若跳转到 Miloco 官方 redirect 页面，把本地回调地址填进去并继续跳转。</p>
      <p><a class="button" href="{auth_url}">打开小米 OAuth 登录</a></p>
      <dl>
        <dt>本地回调</dt><dd><code>{callback_url}</code></dd>
        <dt>OAuth redirect_uri</dt><dd><code>{redirect_uri}</code></dd>
        <dt>OAuth uuid</dt><dd><code>{uuid}</code></dd>
      </dl>
    </section>
  </main>
</body>
</html>"#
    )
}

fn render_success(token_file: &Option<PathBuf>, expires_ts: i64) -> String {
    let token_file = token_file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not saved".to_string());
    let token_file = escape_html(&token_file);

    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>MIoT OAuth 成功</title></head>
<body style="margin:0;font-family:system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#f6f7f9;color:#17202a">
  <main style="min-height:100vh;display:grid;place-items:center;padding:24px;box-sizing:border-box">
    <section style="width:min(680px,100%)">
      <h1 style="margin:0 0 14px;font-size:28px">授权完成</h1>
      <p style="line-height:1.6">Token 已写入 <code>{token_file}</code>，后续 micam-rs 会自动 refresh。</p>
      <p style="line-height:1.6">expires_ts: <code>{expires_ts}</code></p>
    </section>
  </main>
</body>
</html>"#
    )
}

fn render_error(title: &str, message: &str) -> String {
    let title = escape_html(title);
    let message = escape_html(message);
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title></head>
<body style="margin:0;font-family:system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#fff7f7;color:#2d1f1f">
  <main style="min-height:100vh;display:grid;place-items:center;padding:24px;box-sizing:border-box">
    <section style="width:min(760px,100%)">
      <h1 style="margin:0 0 14px;font-size:28px">{title}</h1>
      <p style="line-height:1.6;overflow-wrap:anywhere"><code>{message}</code></p>
    </section>
  </main>
</body>
</html>"#
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(default)
}
