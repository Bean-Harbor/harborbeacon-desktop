//! HarborBeacon Desktop Agent
//!
//! Connects to Feishu via WebSocket long-connection, receives user messages,
//! maps simple commands to safe workspace actions, and prints results.
//! This is the P0 wiring that proves the Feishu → local workspace loop.

use clap::Parser;
use core_contracts::InboundMessage;
use feishu_provider::ws::{FeishuWsConfig, self};
use tracing::{info, error};
use vscode_bridge::{actions, BridgeBinding};

#[derive(Parser)]
#[command(name = "harborbeacon-desktop", about = "HarborBeacon Desktop Agent")]
struct Cli {
    /// Feishu app_id
    #[arg(long, env = "FEISHU_APP_ID")]
    app_id: String,

    /// Feishu app_secret
    #[arg(long, env = "FEISHU_APP_SECRET")]
    app_secret: String,

    /// Feishu API domain
    #[arg(long, default_value = "https://open.feishu.cn")]
    domain: String,

    /// Local workspace path to expose
    #[arg(long)]
    workspace: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = FeishuWsConfig::new(&cli.app_id, &cli.app_secret)
        .with_domain(&cli.domain);

    let bridge = BridgeBinding::new(&cli.workspace, "desktop-workspace");

    info!(workspace = %cli.workspace, "Starting HarborBeacon Desktop Agent");

    let mut handle = match ws::start(config).await {
        Ok(h) => h,
        Err(e) => {
            error!(error = %e, "Failed to start Feishu WS");
            std::process::exit(1);
        }
    };

    info!("Listening for Feishu messages …");

    while let Some(msg) = handle.message_rx.recv().await {
        info!(sender = %msg.sender_id, text = %msg.text, "Received message");
        let reply = dispatch(&bridge, &msg);
        // In the future this sends back via Feishu API; for now just log.
        info!(reply = %reply, "Action result");
    }

    info!("Message channel closed, exiting.");
}

/// Map an inbound text command to a workspace action.
fn dispatch(bridge: &BridgeBinding, msg: &InboundMessage) -> String {
    let text = msg.text.trim();

    if let Some(path) = text.strip_prefix("/read ") {
        match actions::read_file(bridge, path.trim()) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        }
    } else if let Some(path) = text.strip_prefix("/ls ") {
        match actions::list_directory(bridge, path.trim()) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        }
    } else if let Some(rest) = text.strip_prefix("/search ") {
        let mut parts = rest.splitn(2, ' ');
        let path = parts.next().unwrap_or(".");
        let query = parts.next().unwrap_or("");
        match actions::search_text(bridge, path, query) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        }
    } else {
        format!("Commands: /read <path> | /ls <path> | /search <path> <query>")
    }
}
