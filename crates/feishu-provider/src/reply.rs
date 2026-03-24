//! Send reply messages back to Feishu via the REST API.
//!
//! Uses `POST /open-apis/im/v1/messages` to reply in the same chat,
//! or to a specific message via `reply_in_thread`.

use crate::FeishuError;
use serde_json::{json, Value};
use std::time::Duration;

/// A lightweight async Feishu reply client.
///
/// Caches a `tenant_access_token` and refreshes it transparently.
pub struct ReplyClient {
    app_id: String,
    app_secret: String,
    domain: String,
    http: reqwest::Client,
    token: tokio::sync::RwLock<String>,
}

impl ReplyClient {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>, domain: impl Into<String>) -> Result<Self, FeishuError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| FeishuError::ClientInit(e.to_string()))?;
        Ok(Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            domain: domain.into().trim_end_matches('/').to_string(),
            http,
            token: tokio::sync::RwLock::new(String::new()),
        })
    }

    /// Ensure we have a valid tenant token (simple: always refresh).
    async fn refresh_token(&self) -> Result<String, FeishuError> {
        let url = format!("{}/open-apis/auth/v3/tenant_access_token/internal", self.domain);
        let resp: Value = self
            .http
            .post(&url)
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .map_err(|e| FeishuError::Request(e.to_string()))?
            .json()
            .await
            .map_err(|e| FeishuError::Json(e.to_string()))?;

        let code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            let msg = resp.get("msg").and_then(Value::as_str).unwrap_or("unknown").to_string();
            return Err(FeishuError::Api { code, message: msg });
        }

        let token = resp
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .ok_or(FeishuError::MissingField("tenant_access_token"))?
            .to_string();

        *self.token.write().await = token.clone();
        Ok(token)
    }

    /// Get the current token or refresh.
    async fn get_token(&self) -> Result<String, FeishuError> {
        {
            let t = self.token.read().await;
            if !t.is_empty() {
                return Ok(t.clone());
            }
        }
        self.refresh_token().await
    }

    /// Reply to a message in the same chat.
    ///
    /// `message_id` is the Feishu message_id to reply to.
    /// `text` is the plain-text reply content.
    pub async fn reply_text(&self, message_id: &str, text: &str) -> Result<(), FeishuError> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/open-apis/im/v1/messages/{}/reply",
            self.domain, message_id
        );

        let body = json!({
            "content": json!({"text": text}).to_string(),
            "msg_type": "text",
        });

        let resp: Value = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| FeishuError::Request(e.to_string()))?
            .json()
            .await
            .map_err(|e| FeishuError::Json(e.to_string()))?;

        let code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            // Token might be expired; try once more with refresh.
            let token = self.refresh_token().await?;
            let resp2: Value = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await
                .map_err(|e| FeishuError::Request(e.to_string()))?
                .json()
                .await
                .map_err(|e| FeishuError::Json(e.to_string()))?;

            let code2 = resp2.get("code").and_then(Value::as_i64).unwrap_or(-1);
            if code2 != 0 {
                let msg = resp2.get("msg").and_then(Value::as_str).unwrap_or("unknown").to_string();
                return Err(FeishuError::Api { code: code2, message: msg });
            }
        }

        Ok(())
    }

    /// Send a new message to a chat (not a reply).
    ///
    /// `receive_id` is the chat_id or open_id.
    /// `receive_id_type` is "chat_id" or "open_id".
    pub async fn send_text(
        &self,
        receive_id: &str,
        receive_id_type: &str,
        text: &str,
    ) -> Result<(), FeishuError> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type={}",
            self.domain, receive_id_type
        );

        let body = json!({
            "receive_id": receive_id,
            "content": json!({"text": text}).to_string(),
            "msg_type": "text",
        });

        let resp: Value = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| FeishuError::Request(e.to_string()))?
            .json()
            .await
            .map_err(|e| FeishuError::Json(e.to_string()))?;

        let code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            let msg = resp.get("msg").and_then(Value::as_str).unwrap_or("unknown").to_string();
            return Err(FeishuError::Api { code, message: msg });
        }

        Ok(())
    }
}
