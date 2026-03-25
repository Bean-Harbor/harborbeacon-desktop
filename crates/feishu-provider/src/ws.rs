//! Feishu WebSocket long-connection client.
//!
//! Connects to the Feishu event gateway via an outbound WebSocket,
//! receives pushed events, and dispatches them as `InboundMessage`.
//! Auto-reconnects on disconnect with exponential back-off.

use core_contracts::{Channel, ChatType, ConnectionState, InboundMessage};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{error, info, warn};

use crate::FeishuError;

#[derive(Clone, PartialEq, ProstMessage)]
struct PbHeader {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
struct PbFrame {
    #[prost(uint64, tag = "1")]
    seq_id: u64,
    #[prost(uint64, tag = "2")]
    log_id: u64,
    #[prost(int32, tag = "3")]
    service: i32,
    #[prost(int32, tag = "4")]
    method: i32,
    #[prost(message, repeated, tag = "5")]
    headers: Vec<PbHeader>,
    #[prost(string, tag = "6")]
    payload_encoding: String,
    #[prost(string, tag = "7")]
    payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    payload: Vec<u8>,
    #[prost(string, tag = "9")]
    log_id_new: String,
}

impl PbFrame {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }
}

const METHOD_CONTROL: i32 = 0;
const METHOD_DATA: i32 = 1;

/// Configuration for the Feishu WebSocket long-connection.
#[derive(Debug, Clone)]
pub struct FeishuWsConfig {
    pub app_id: String,
    pub app_secret: String,
    pub domain: String,
}

impl FeishuWsConfig {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            domain: "https://open.feishu.cn".into(),
        }
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = domain.into();
        self
    }
}

/// A running long-connection handle.
///
/// Drop this to signal the background loop to stop.
pub struct FeishuWsHandle {
    _cancel: tokio::sync::watch::Sender<bool>,
    pub state_rx: watch::Receiver<ConnectionState>,
    pub message_rx: mpsc::Receiver<InboundMessage>,
}

/// Start the Feishu WebSocket long-connection loop.
///
/// Returns a handle that yields `InboundMessage` values via `message_rx`.
/// The connection auto-reconnects on failure.
pub async fn start(config: FeishuWsConfig) -> Result<FeishuWsHandle, FeishuError> {
    let (msg_tx, msg_rx) = mpsc::channel::<InboundMessage>(256);
    let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting);
    let (cancel_tx, cancel_rx) = watch::channel(false);

    let config = Arc::new(config);

    tokio::spawn(run_loop(config, msg_tx, state_tx, cancel_rx));

    Ok(FeishuWsHandle {
        _cancel: cancel_tx,
        state_rx,
        message_rx: msg_rx,
    })
}

/// Background reconnect loop.
async fn run_loop(
    config: Arc<FeishuWsConfig>,
    msg_tx: mpsc::Sender<InboundMessage>,
    state_tx: watch::Sender<ConnectionState>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        if *cancel_rx.borrow() {
            let _ = state_tx.send(ConnectionState::Stopped);
            break;
        }

        let _ = state_tx.send(ConnectionState::Connecting);
        info!("Fetching Feishu WS endpoint …");

        match obtain_ws_url(&config).await {
            Ok(ws_url) => {
                info!(url = %ws_url, "Connecting to Feishu WS …");
                match connect_and_receive(&ws_url, &msg_tx, &mut cancel_rx).await {
                    Ok(()) => {
                        info!("WS session ended normally");
                    }
                    Err(e) => {
                        warn!(error = %e, "WS session ended with error");
                    }
                }
                // Reset backoff after a successful connection.
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                error!(error = %e, "Failed to obtain WS endpoint");
            }
        }

        if *cancel_rx.borrow() {
            let _ = state_tx.send(ConnectionState::Stopped);
            break;
        }

        let _ = state_tx.send(ConnectionState::Reconnecting);
        info!(wait_secs = backoff.as_secs(), "Reconnecting after back-off …");

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {},
            _ = cancel_rx.changed() => { continue; }
        }

        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Obtain a WebSocket URL from the Feishu endpoint API.
async fn obtain_ws_url(config: &FeishuWsConfig) -> Result<String, FeishuError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| FeishuError::ClientInit(e.to_string()))?;

    let domain = config.domain.trim_end_matches('/');

    // Step 1: tenant token
    let token_url = format!("{domain}/open-apis/auth/v3/tenant_access_token/internal");
    let token_resp: Value = client
        .post(&token_url)
        .json(&serde_json::json!({
            "app_id": config.app_id,
            "app_secret": config.app_secret,
        }))
        .send()
        .await
        .map_err(|e| FeishuError::Request(e.to_string()))?
        .json()
        .await
        .map_err(|e| FeishuError::Json(e.to_string()))?;

    let code = token_resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let msg = token_resp
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        return Err(FeishuError::Api { code, message: msg });
    }

    // Step 2: WS endpoint
    let ws_url_api = format!("{domain}/callback/ws/endpoint");
    let ws_resp_text = client
        .post(&ws_url_api)
        .header("locale", "zh")
        .json(&serde_json::json!({
            "AppID": config.app_id,
            "AppSecret": config.app_secret,
        }))
        .send()
        .await
        .map_err(|e| FeishuError::Request(e.to_string()))?
        .text()
        .await
        .map_err(|e| FeishuError::Json(e.to_string()))?;
    let ws_resp: Value = serde_json::from_str(&ws_resp_text)
        .map_err(|e| FeishuError::Json(e.to_string()))?;

    let code = ws_resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let msg = ws_resp
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        return Err(FeishuError::Api { code, message: msg });
    }

    ws_resp
        .pointer("/data/URL")
        .or_else(|| ws_resp.pointer("/data/url"))
        .or_else(|| ws_resp.pointer("/data/endpoint"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(FeishuError::MissingField("data.URL"))
}

/// Connect to the WS URL, receive events, and dispatch them.
async fn connect_and_receive(
    ws_url: &str,
    msg_tx: &mpsc::Sender<InboundMessage>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(), FeishuError> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| FeishuError::Request(format!("WS connect failed: {e}")))?;

    info!("Connected to Feishu WS");

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Some(msg) = parse_event_json(&text) {
                            if msg_tx.send(msg).await.is_err() {
                                // Receiver dropped
                                break;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        match PbFrame::decode(data.as_ref()) {
                            Ok(frame) => {
                                let frame_type = frame.header("type").unwrap_or("");
                                if frame.method == METHOD_CONTROL && frame_type == "ping" {
                                    let mut pong = frame.clone();
                                    pong.headers = vec![PbHeader {
                                        key: "type".to_string(),
                                        value: "pong".to_string(),
                                    }];
                                    pong.payload.clear();
                                    let mut buf = Vec::new();
                                    if pong.encode(&mut buf).is_ok() {
                                        let _ = write.send(WsMessage::Binary(buf.into())).await;
                                    }
                                }

                                if frame.method == METHOD_DATA && frame_type == "event" {
                                    // ACK event frame so Feishu treats delivery as successful.
                                    let mut ack = frame.clone();
                                    ack.payload = br#"{"code":200}"#.to_vec();
                                    let mut ack_buf = Vec::new();
                                    if ack.encode(&mut ack_buf).is_ok() {
                                        let _ = write.send(WsMessage::Binary(ack_buf.into())).await;
                                    }

                                    if let Ok(payload_text) = String::from_utf8(frame.payload.clone()) {
                                        if let Some(msg) = parse_event_json(&payload_text) {
                                            if msg_tx.send(msg).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to decode protobuf WS frame");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        info!("WS closed by remote");
                        break;
                    }
                    Some(Err(e)) => {
                        return Err(FeishuError::Request(format!("WS read error: {e}")));
                    }
                    _ => {}
                }
            }
            _ = cancel_rx.changed() => {
                info!("Cancel signal received, closing WS");
                let _ = write.send(WsMessage::Close(None)).await;
                break;
            }
        }
    }

    Ok(())
}

/// Parse a raw Feishu WS event frame into an `InboundMessage`.
///
/// Feishu sends JSON frames with a `header` and `event` section.
/// We extract the user message from `im.message.receive_v1` events.
fn parse_event_json(raw: &str) -> Option<InboundMessage> {
    let v: Value = serde_json::from_str(raw).ok()?;

    // Feishu frames have a `header.event_type` field.
    let event_type = v
        .pointer("/header/event_type")
        .and_then(Value::as_str)
        .unwrap_or("");

    if event_type != "im.message.receive_v1" {
        return None;
    }

    let event = v.get("event")?;
    let message = event.get("message")?;
    let sender = event.pointer("/sender/sender_id/open_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let message_id = message
        .get("message_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let chat_id = message
        .get("chat_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let chat_type = match message.get("chat_type").and_then(Value::as_str) {
        Some("p2p") => ChatType::P2p,
        Some("group") => ChatType::Group,
        _ => ChatType::Unknown,
    };

    // Message content is a JSON string inside the "content" field.
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|c| c.get("text").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();

    let mentions = message
        .get("mentions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Some(InboundMessage {
        channel: Channel::Feishu,
        sender_id: sender,
        text,
        message_id,
        chat_type,
        chat_id,
        mentions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_event() {
        let frame = r#"{
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {
                    "sender_id": { "open_id": "ou_abc123" }
                },
                "message": {
                    "message_id": "msg_001",
                    "chat_id": "oc_xyz",
                    "chat_type": "p2p",
                    "content": "{\"text\":\"hello world\"}"
                }
            }
        }"#;

        let msg = parse_event_json(frame).expect("should parse");
        assert_eq!(msg.sender_id, "ou_abc123");
        assert_eq!(msg.text, "hello world");
        assert_eq!(msg.chat_type, ChatType::P2p);
        assert_eq!(msg.message_id, "msg_001");
    }

    #[test]
    fn ignore_non_message_event() {
        let frame = r#"{"header":{"event_type":"url_verification"}}"#;
        assert!(parse_event_json(frame).is_none());
    }
}
