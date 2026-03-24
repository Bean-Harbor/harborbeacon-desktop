//! HarborBeacon Desktop Agent — conversational Feishu interface with session state.
//!
//! The agent maintains per-user session state so that "继续", "重试", "状态",
//! and "plan" workflows persist across messages and restarts.
//!
//! Interaction modes:
//!   Coding   — default; execute actions directly and report results.
//!   Planning — record a task + step list; use "继续" to step through them.

use clap::Parser;
use core_contracts::InboundMessage;
use feishu_provider::reply::ReplyClient;
use feishu_provider::ws::{self, FeishuWsConfig};
use session_store::{default_session_dir, now_secs, SessionMode, SessionStore};
use std::sync::Arc;
use tracing::{error, info, warn};
use vscode_bridge::{actions, BridgeBinding};

enum Intent {
    // action intents
    Read(String),
    List(String),
    Search { path: String, query: String },
    Diff(String),
    Patch(String),
    Test(String),
    // meta intents
    Continue,
    Retry,
    Status,
    SetPlan { description: String, steps: Vec<String> },
    SwitchMode(SessionMode),
    Clear,
    Help,
}

#[derive(Parser)]
#[command(name = "harborbeacon-desktop", about = "HarborBeacon Desktop Agent")]
struct Cli {
    #[arg(long, env = "FEISHU_APP_ID")]
    app_id: String,
    #[arg(long, env = "FEISHU_APP_SECRET")]
    app_secret: String,
    #[arg(long, default_value = "https://open.feishu.cn")]
    domain: String,
    #[arg(long)]
    workspace: String,
    /// Session storage directory (default: {workspace}/.harborbeacon/sessions)
    #[arg(long)]
    session_dir: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let session_dir = cli
        .session_dir
        .unwrap_or_else(|| default_session_dir(&cli.workspace));

    let config = FeishuWsConfig::new(&cli.app_id, &cli.app_secret).with_domain(&cli.domain);
    let bridge = BridgeBinding::new(&cli.workspace, "desktop-workspace");
    let store = SessionStore::new(&session_dir);

    let reply_client = Arc::new(
        ReplyClient::new(&cli.app_id, &cli.app_secret, &cli.domain)
            .expect("failed to create reply client"),
    );

    info!(workspace = %cli.workspace, session_dir = %session_dir, "Starting HarborBeacon Desktop Agent");

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
        let reply_text = dispatch_with_session(&bridge, &store, &msg);
        info!(reply = %reply_text, "Action result");

        if !msg.message_id.is_empty() {
            let client = Arc::clone(&reply_client);
            let mid = msg.message_id.clone();
            let rt = reply_text.clone();
            tokio::spawn(async move {
                if let Err(e) = client.reply_text(&mid, &rt).await {
                    warn!(error = %e, "Failed to send Feishu reply");
                }
            });
        }
    }

    info!("Message channel closed, exiting.");
}

// ---- session-aware dispatch -------------------------------------------- 

fn dispatch_with_session(bridge: &BridgeBinding, store: &SessionStore, msg: &InboundMessage) -> String {
    let text = msg.text.trim();
    let mut session = store.load(&msg.sender_id);

    let reply = match parse_intent(text) {
        None => help_text(),
        Some(intent) => match intent {
            // ---------- meta ----------
            Intent::Continue => {
                if session.pending_steps.is_empty() {
                    "无待执行步骤。\n发送 `plan <任务>` 设置计划，或直接执行命令。".to_string()
                } else {
                    let step = session.pending_steps.remove(0);
                    let result = if let Some(action) = parse_action_intent(&step) {
                        execute_action(bridge, action)
                    } else {
                        format!("▸ {step}（需要人工操作）")
                    };
                    session.last_result = Some(result.clone());
                    session.updated_at = now_secs();
                    let rem = session.pending_steps.len();
                    format!(
                        "▸ 执行步骤: {step}\n\n{result}\n\n{}",
                        if rem == 0 {
                            "✅ 所有步骤已执行完毕".to_string()
                        } else {
                            format!("还剩 {rem} 步，发送「继续」执行下一步")
                        }
                    )
                }
            }
            Intent::Retry => match session.last_action.clone() {
                None => "无可重试的操作。".to_string(),
                Some(last_text) => match parse_action_intent(&last_text) {
                    None => "上次操作无法自动重试，请重新发送命令。".to_string(),
                    Some(action) => {
                        let result = execute_action(bridge, action);
                        session.last_result = Some(result.clone());
                        session.updated_at = now_secs();
                        format!("↻ 重试: {last_text}\n\n{result}")
                    }
                },
            },
            Intent::Status => fmt_session_status(&session),
            Intent::SetPlan { description, steps } => {
                session.mode = SessionMode::Planning;
                session.current_task = Some(description.clone());
                session.pending_steps = steps.clone();
                session.updated_at = now_secs();
                fmt_plan_saved(&description, &steps)
            }
            Intent::SwitchMode(mode) => {
                let label = match &mode { SessionMode::Coding => "编码", SessionMode::Planning => "规划" };
                session.mode = mode;
                session.updated_at = now_secs();
                format!("✅ 已切换到{label}模式")
            }
            Intent::Clear => {
                store.clear(&msg.sender_id);
                return "🗑 会话已清除".to_string();
            }
            Intent::Help => help_text(),
            // ---------- action ----------
            action => {
                let result = execute_action(bridge, action);
                session.last_action = Some(text.to_string());
                session.last_result = Some(result.clone());
                session.mode = SessionMode::Coding;
                session.updated_at = now_secs();
                result
            }
        },
    };

    store.save(&session).ok();
    reply
}

// ---- execute action intent -------------------------------------------- 

fn execute_action(bridge: &BridgeBinding, intent: Intent) -> String {
    match intent {
        Intent::Read(path) => match actions::read_file(bridge, path.trim()) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        },
        Intent::List(path) => match actions::list_directory(bridge, path.trim()) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        },
        Intent::Search { path, query } => match actions::search_text(bridge, &path, &query) {
            Ok(r) => r.content,
            Err(e) => format!("Error: {e}"),
        },
        Intent::Diff(path) => {
            let target = if path.trim().is_empty() { "." } else { path.trim() };
            match actions::git_diff(bridge, target) {
                Ok(r) => r.content,
                Err(e) => format!("Error: {e}"),
            }
        }
        Intent::Patch(patch) => match actions::apply_patch(bridge, &patch) {
            Ok(r) => {
                if r.success { r.content } else { format!("Patch rejected: {}", r.content) }
            }
            Err(e) => format!("Error: {e}"),
        },
        Intent::Test(filter) => match actions::run_tests(bridge, &filter) {
            Ok(r) => {
                if r.success { format!("Tests passed.\n{}", r.content) }
                else { format!("Tests failed.\n{}", r.content) }
            }
            Err(e) => format!("Error: {e}"),
        },
        _ => "not an action intent".to_string(),
    }
}

// ---- intent parsing ----------------------------------------------------

fn parse_intent(text: &str) -> Option<Intent> {
    if text.is_empty() { return Some(Intent::Help); }
    let lower = text.to_lowercase();

    // meta: continue
    if matches!(lower.as_str(), "继续" | "continue" | "继续执行" | "下一步") {
        return Some(Intent::Continue);
    }
    // meta: retry
    if matches!(lower.as_str(), "重试" | "retry" | "再试一次" | "再来一次") {
        return Some(Intent::Retry);
    }
    // meta: status
    if matches!(lower.as_str(), "状态" | "status" | "进展" | "现在做什么" | "当前任务") {
        return Some(Intent::Status);
    }
    // meta: clear
    if matches!(lower.as_str(), "清除" | "clear" | "清除会话" | "reset" | "重置") {
        return Some(Intent::Clear);
    }
    // meta: switch mode
    if matches!(lower.as_str(), "编码模式" | "切换编码" | "switch coding" | "coding mode" | "coding") {
        return Some(Intent::SwitchMode(SessionMode::Coding));
    }
    if matches!(lower.as_str(), "规划模式" | "切换规划" | "switch planning" | "planning mode" | "planning") {
        return Some(Intent::SwitchMode(SessionMode::Planning));
    }
    // meta: help
    if text == "/help" || matches!(lower.as_str(), "help" | "帮助") {
        return Some(Intent::Help);
    }
    // meta: set plan (single-line prefix)
    for prefix in &["/plan ", "plan ", "计划 ", "规划 ", "/计划 ", "/规划 ", "做计划 ", "做规划 "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let (desc, steps) = parse_plan_body(rest);
            return Some(Intent::SetPlan { description: desc, steps });
        }
    }
    // meta: set plan (multi-line — trigger word on first line, steps on subsequent lines)
    let plan_triggers = ["plan", "计划", "规划", "/plan", "/计划", "/规划"];
    if let Some(nl) = text.find('\n') {
        let first = text[..nl].trim().to_lowercase();
        if plan_triggers.contains(&first.as_str()) {
            let rest = text[nl + 1..].trim();
            let (desc, steps) = parse_plan_body(rest);
            return Some(Intent::SetPlan { description: desc, steps });
        }
    }
    // bare plan word → show current status
    if plan_triggers.contains(&lower.trim()) {
        return Some(Intent::Status);
    }

    parse_action_intent(text)
}

/// Action-only parser; also used by Continue to auto-execute plan steps.
fn parse_action_intent(text: &str) -> Option<Intent> {
    let lower = text.to_lowercase();

    if let Some(path) = text.strip_prefix("/read ")
        .or_else(|| text.strip_prefix("read "))
        .or_else(|| text.strip_prefix("读 "))
    {
        return Some(Intent::Read(path.trim().to_string()));
    }
    if let Some(path) = text.strip_prefix("/ls ")
        .or_else(|| text.strip_prefix("ls "))
        .or_else(|| text.strip_prefix("list "))
        .or_else(|| text.strip_prefix("看 "))
    {
        return Some(Intent::List(path.trim().to_string()));
    }
    for prefix in &["/search ", "search ", "搜 "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let mut parts = rest.splitn(2, ' ');
            let path = parts.next().unwrap_or(".").trim().to_string();
            let query = parts.next().unwrap_or("").trim().to_string();
            return Some(Intent::Search { path, query });
        }
    }
    if let Some(path) = {
        let l = lower.as_str();
        l.strip_prefix("/diff").or_else(|| l.strip_prefix("diff")).map(str::trim).map(str::to_string)
    } {
        return Some(Intent::Diff(path));
    }
    if let Some(patch) = text.strip_prefix("/patch ") {
        return Some(Intent::Patch(patch.to_string()));
    }
    for prefix in &["/test", "test"] {
        if let Some(filter) = lower.strip_prefix(prefix) {
            return Some(Intent::Test(filter.trim().to_string()));
        }
    }
    if let Some(filter) = text.strip_prefix("跑测试") {
        return Some(Intent::Test(filter.trim().to_string()));
    }
    None
}

// ---- plan body parsing -------------------------------------------------

fn parse_plan_body(text: &str) -> (String, Vec<String>) {
    let mut description = String::new();
    let mut steps: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some(step) = strip_list_prefix(line) {
            steps.push(step.to_string());
        } else if description.is_empty() {
            description = line.to_string();
        }
    }
    if description.is_empty() && !steps.is_empty() { description = "任务".to_string(); }
    else if description.is_empty() { description = text.trim().to_string(); }
    (description, steps)
}

fn strip_list_prefix(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("• "))
    {
        return Some(rest.trim());
    }
    // numbered: "1. " / "1) " / "1、"
    let num_end = s.char_indices().take_while(|(_, c)| c.is_ascii_digit()).last().map(|(i, _)| i + 1);
    if let Some(end) = num_end {
        if end > 0 {
            let rest = &s[end..];
            if let Some(stripped) = rest.strip_prefix(". ")
                .or_else(|| rest.strip_prefix(") "))
                .or_else(|| rest.strip_prefix('、'))
            {
                return Some(stripped.trim());
            }
        }
    }
    None
}

// ---- session formatting ------------------------------------------------

fn fmt_session_status(session: &session_store::UserSession) -> String {
    let mode_label = match session.mode {
        SessionMode::Coding => "编码",
        SessionMode::Planning => "规划",
    };
    let task = session.current_task.as_deref().unwrap_or("（无当前任务）");
    let steps_part = if session.pending_steps.is_empty() {
        "无待执行步骤".to_string()
    } else {
        let list: Vec<String> = session.pending_steps.iter().enumerate()
            .map(|(i, s)| format!("  {}. {s}", i + 1))
            .collect();
        format!("待执行步骤（{}步）:\n{}", session.pending_steps.len(), list.join("\n"))
    };
    let last = session.last_action.as_deref().unwrap_or("（无）");
    format!("📋 状态\n模式: {mode_label}\n当前任务: {task}\n{steps_part}\n上次操作: {last}")
}

fn fmt_plan_saved(description: &str, steps: &[String]) -> String {
    if steps.is_empty() {
        format!("✅ 计划已记录\n任务: {description}\n\n发送「继续」执行，或补充步骤。")
    } else {
        let list: Vec<String> = steps.iter().enumerate()
            .map(|(i, s)| format!("  {}. {s}", i + 1))
            .collect();
        format!(
            "✅ 计划已记录（共 {} 步）\n任务: {description}\n\n{}\n\n发送「继续」开始执行。",
            steps.len(),
            list.join("\n")
        )
    }
}

fn help_text() -> String {
    "HarborBeacon 飞书远程编程指令:\n\
\n【执行命令】\n\
read <path>          读取文件\n\
ls <path>            列出目录\n\
search <path> <q>    搜索文本\n\
diff [path]          查看 git diff\n\
/patch <patch>       应用补丁\n\
test [filter]        运行测试\n\
\n【会话命令】\n\
plan <任务>          记录任务（换行写步骤）\n\
继续                 执行下一步\n\
重试                 重新执行上次操作\n\
状态                 查看当前任务进展\n\
清除                 清除会话\n\
切换编码/切换规划     切换工作模式"
        .to_string()
}
