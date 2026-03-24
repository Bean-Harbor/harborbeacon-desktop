//! HarborBeacon Desktop Agent
//!
//! Connects to Feishu via WebSocket long-connection, receives user messages,
//! maps simple commands to safe workspace actions, and prints results.
//! This is the P0 wiring that proves the Feishu → local workspace loop.

use clap::Parser;
use core_contracts::InboundMessage;
use feishu_provider::reply::ReplyClient;
use feishu_provider::ws::{FeishuWsConfig, self};
use std::sync::Arc;
use tracing::{info, error, warn};
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

    let reply_client = Arc::new(
        ReplyClient::new(&cli.app_id, &cli.app_secret, &cli.domain)
            .expect("failed to create reply client"),
    );

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
        let reply_text = dispatch(&bridge, &msg);
        info!(reply = %reply_text, "Action result");

        // Send reply back to Feishu
        if !msg.message_id.is_empty() {
            let client = Arc::clone(&reply_client);
            let mid = msg.message_id.clone();
            let rt = reply_text.clone();
            tokio::spawn(async move {
                if let Err(e) = client.reply_text(&mid, &rt).await {
                    warn!(error = %e, "Failed to send reply to Feishu");
                }
            });
        }
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
    } else if let Some(path) = text.strip_prefix("/diff") {
        let target = path.trim();
        let target = if target.is_empty() { "." } else { target };
        match actions::git_diff(bridge, target) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        }
    } else if let Some(patch) = text.strip_prefix("/patch ") {
        match actions::apply_patch(bridge, patch) {
            Ok(r) => {
                if r.success {
                    r.content
                } else {
                    format!("Patch rejected: {}", r.content)
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    } else if let Some(filter) = text.strip_prefix("/test") {
        let filter = filter.trim();
        match actions::run_tests(bridge, filter) {
            Ok(r) => {
                if r.success {
                    format!("Tests passed.\n{}", r.content)
                } else {
                    format!("Tests failed.\n{}", r.content)
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    } else {
        format!("Commands: /read <path> | /ls <path> | /search <path> <query> | /diff [path] | /patch <unified_patch> | /test [filter]")
    }
}
