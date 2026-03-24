use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Channel & autonomy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Channel {
    Feishu,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AutonomyLevel {
    ReadOnly,
    Supervised,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTarget {
    pub path: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Chat & message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatType {
    P2p,
    Group,
    Unknown,
}

impl Default for ChatType {
    fn default() -> Self {
        Self::Unknown
    }
}

/// A message received from an IM channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: Channel,
    pub sender_id: String,
    pub text: String,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub chat_type: ChatType,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub mentions: Vec<String>,
}

/// A message to send back to an IM channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: Channel,
    pub recipient_id: String,
    pub text: String,
    #[serde(default)]
    pub reply_to_message_id: String,
}

// ---------------------------------------------------------------------------
// Connection state (for transports)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Stopped,
}
