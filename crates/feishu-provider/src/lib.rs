use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

pub mod reply;
pub mod ws;

#[derive(Debug, Error)]
pub enum FeishuError {
    #[error("http client init failed: {0}")]
    ClientInit(String),
    #[error("request failed: {0}")]
    Request(String),
    #[error("json parse failed: {0}")]
    Json(String),
    #[error("feishu api returned code {code}: {message}")]
    Api { code: i64, message: String },
    #[error("missing field in response: {0}")]
    MissingField(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectivityReport {
    pub ok: bool,
    pub domain: String,
    pub token_ok: bool,
    pub bot_info_ok: bool,
    pub ws_endpoint_ok: bool,
    pub app_name: Option<String>,
    pub bot_name: Option<String>,
    pub ws_endpoint: Option<String>,
    pub ws_service_id: Option<String>,
    pub warnings: Vec<String>,
}

pub fn check_connectivity(
    app_id: &str,
    app_secret: &str,
    domain: &str,
) -> Result<ConnectivityReport, FeishuError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| FeishuError::ClientInit(error.to_string()))?;

    let tenant_token = fetch_tenant_token(&client, app_id, app_secret, domain)?;
    let mut report = ConnectivityReport {
        ok: true,
        domain: domain.trim_end_matches('/').to_string(),
        token_ok: true,
        bot_info_ok: false,
        ws_endpoint_ok: false,
        app_name: None,
        bot_name: None,
        ws_endpoint: None,
        ws_service_id: None,
        warnings: Vec::new(),
    };

    match fetch_bot_info(&client, &tenant_token, domain) {
        Ok((app_name, bot_name)) => {
            report.bot_info_ok = true;
            report.app_name = app_name;
            report.bot_name = bot_name;
        }
        Err(error) => {
            report.ok = false;
            report.warnings.push(format!("bot info check failed: {error}"));
        }
    }

    match fetch_ws_endpoint(&client, app_id, app_secret, domain) {
        Ok((endpoint, service_id)) => {
            report.ws_endpoint_ok = true;
            report.ws_endpoint = Some(endpoint);
            report.ws_service_id = Some(service_id);
        }
        Err(error) => {
            report.ok = false;
            report.warnings.push(format!("ws endpoint check failed: {error}"));
        }
    }

    Ok(report)
}

fn fetch_tenant_token(
    client: &Client,
    app_id: &str,
    app_secret: &str,
    domain: &str,
) -> Result<String, FeishuError> {
    let url = format!(
        "{}/open-apis/auth/v3/tenant_access_token/internal",
        domain.trim_end_matches('/')
    );
    let body = json!({
        "app_id": app_id,
        "app_secret": app_secret,
    });
    let payload = post_json(client, &url, &body)?;
    parse_success(&payload)?;
    payload
        .get("tenant_access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(FeishuError::MissingField("tenant_access_token"))
}

fn fetch_bot_info(
    client: &Client,
    tenant_token: &str,
    domain: &str,
) -> Result<(Option<String>, Option<String>), FeishuError> {
    let url = format!("{}/open-apis/bot/v3/info", domain.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {tenant_token}"))
        .send()
        .map_err(|error| FeishuError::Request(error.to_string()))?;
    let payload: Value = response
        .json()
        .map_err(|error| FeishuError::Json(error.to_string()))?;
    parse_success(&payload)?;

    let bot = payload.get("bot").cloned().unwrap_or(Value::Null);
    let app_name = bot.get("app_name").and_then(Value::as_str).map(str::to_string);
    let bot_name = bot.get("bot_name").and_then(Value::as_str).map(str::to_string);
    Ok((app_name, bot_name))
}

fn fetch_ws_endpoint(
    client: &Client,
    app_id: &str,
    app_secret: &str,
    domain: &str,
) -> Result<(String, String), FeishuError> {
    let url = format!("{}/open-apis/callback/ws/endpoint", domain.trim_end_matches('/'));
    let body = json!({
        "app_id": app_id,
        "app_secret": app_secret,
    });
    let payload = post_json(client, &url, &body)?;
    parse_success(&payload)?;

    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let endpoint = data
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(FeishuError::MissingField("data.endpoint"))?;
    let service_id = data
        .get("service_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(FeishuError::MissingField("data.service_id"))?;

    Ok((endpoint, service_id))
}

fn post_json(client: &Client, url: &str, body: &Value) -> Result<Value, FeishuError> {
    let response = client
        .post(url)
        .json(body)
        .send()
        .map_err(|error| FeishuError::Request(error.to_string()))?;
    response
        .json()
        .map_err(|error| FeishuError::Json(error.to_string()))
}

fn parse_success(payload: &Value) -> Result<(), FeishuError> {
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = payload
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Err(FeishuError::Api { code, message });
    }
    Ok(())
}
