//! HarborBeacon Desktop Agent — conversational Feishu interface with session state.
//!
//! The agent maintains per-user session state so that "继续", "重试", "状态",
//! and "plan" workflows persist across messages and restarts.
//!
//! Interaction modes:
//!   Coding   — default; execute actions directly and report results.
//!   Planning — record a task + step list; use "继续" to step through them.

use clap::Parser;
use candle_core::{Device, Tensor};
use candle_nn::ops::softmax;
use core_contracts::InboundMessage;
use feishu_provider::reply::ReplyClient;
use feishu_provider::ws::{self, FeishuWsConfig};
use reqwest::Client as HttpClient;
use rusqlite::Connection as SqliteConn;
use session_store::{default_session_dir, now_secs, SessionMode, SessionStore};
use std::fs;
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
    SessionList,
    SessionSave(Option<String>),
    SessionLoad(String),
    // Copilot Chat bridge
    CopilotAsk(String),
    CopilotSessions,
    CopilotHistory(usize),
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
    /// GitHub personal access token for Copilot Chat API (or set HARBOR_GITHUB_TOKEN)
    #[arg(long, env = "HARBOR_GITHUB_TOKEN")]
    github_token: Option<String>,
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

    let http = HttpClient::new();

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
        let reply_text = dispatch_with_session(&bridge, &store, &msg, &http, cli.github_token.as_deref()).await;
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

async fn dispatch_with_session(bridge: &BridgeBinding, store: &SessionStore, msg: &InboundMessage, http: &HttpClient, github_token: Option<&str>) -> String {
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
                    let result = execute_plan_step(bridge, &step);
                    session.last_result = Some(result.clone());
                    session.updated_at = now_secs();
                    let rem = session.pending_steps.len();
                    if rem == 0 {
                        let _ = store.save_snapshot(&session, Some("auto-completed"));
                    }
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
                if has_meaningful_session(&session) {
                    let _ = store.save_snapshot(&session, Some("auto-before-new-plan"));
                }
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
            Intent::SessionList => match store.list_snapshots(&msg.sender_id) {
                Ok(items) => fmt_snapshot_list(&items),
                Err(e) => format!("读取会话历史失败: {e}"),
            },
            Intent::SessionSave(label) => {
                session.updated_at = now_secs();
                match store.save_snapshot(&session, label.as_deref()) {
                    Ok(id) => format!("✅ 会话已保存: {id}\n发送“会话列表”可查看历史。"),
                    Err(e) => format!("保存会话失败: {e}"),
                }
            }
            Intent::SessionLoad(id) => match store.load_snapshot(&msg.sender_id, &id) {
                Ok(mut loaded) => {
                    loaded.updated_at = now_secs();
                    session = loaded;
                    format!("✅ 已载入会话: {id}\n{}", fmt_session_status(&session))
                }
                Err(e) => format!("载入会话失败: {e}"),
            },
            Intent::Clear => {
                if has_meaningful_session(&session) {
                    let _ = store.save_snapshot(&session, Some("auto-before-clear"));
                }
                store.clear(&msg.sender_id);
                return "🗑 会话已清除（已自动归档）".to_string();
            }
            // ---------- copilot bridge ----------
            Intent::CopilotAsk(question) => match copilot_ask(http, github_token, &question).await {
                Ok(ans) => ans,
                Err(e) => format!("❌ Copilot 请求失败: {e}"),
            },
            Intent::CopilotSessions => {
                let sessions = read_vscode_chat_sessions();
                fmt_vscode_sessions(&sessions)
            }
            Intent::CopilotHistory(n) => {
                let sessions = read_vscode_chat_sessions();
                if n == 0 {
                    let msgs = read_vscode_recent_messages();
                    fmt_recent_messages(&msgs)
                } else if let Some(s) = sessions.get(n.saturating_sub(1)) {
                    let msgs = read_vscode_recent_messages();
                    format!("📝 会话: {}\n\n{}", s.title, fmt_recent_messages(&msgs))
                } else {
                    format!("未找到会话 #{n}，发送\"chat 历史\"查看完整列表。")
                }
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

fn execute_plan_step(bridge: &BridgeBinding, step: &str) -> String {
    if let Some(recipe_id) = step.strip_prefix("@recipe:") {
        return execute_recipe(bridge, recipe_id.trim());
    }

    if let Some(action) = parse_action_intent(step) {
        return execute_action(bridge, action);
    }

    format!("▸ {step}（需要人工操作）")
}

fn execute_recipe(bridge: &BridgeBinding, recipe_id: &str) -> String {
    match recipe_id {
        "snake_python_bootstrap" => {
            let root = std::path::Path::new(&bridge.workspace.path);
            let project_dir = root.join("examples").join("snake-game");

            if let Err(e) = fs::create_dir_all(&project_dir) {
                return format!("Recipe failed: cannot create directory: {e}");
            }

            let readme = r#"# Snake Game (pygame)

## Run

1. Create venv (optional)
2. Install dependencies

```bash
pip install -r requirements.txt
python main.py
```

## Controls

- Arrow keys: move
- R: restart after game over
- Q: quit

This is a lightweight starter project generated by HarborBeacon recipe.
"#;

            let requirements = "pygame>=2.5.0\n";

            let main_py = r#"import random
import sys

import pygame


CELL = 20
GRID_W = 30
GRID_H = 20
WIDTH = GRID_W * CELL
HEIGHT = GRID_H * CELL
FPS = 12

BLACK = (20, 20, 20)
GREEN = (60, 180, 75)
RED = (230, 80, 80)
WHITE = (245, 245, 245)


def random_food(snake):
    while True:
        p = (random.randint(0, GRID_W - 1), random.randint(0, GRID_H - 1))
        if p not in snake:
            return p


def draw_cell(surface, pos, color):
    x, y = pos
    rect = pygame.Rect(x * CELL, y * CELL, CELL - 1, CELL - 1)
    pygame.draw.rect(surface, color, rect)


def run():
    pygame.init()
    screen = pygame.display.set_mode((WIDTH, HEIGHT))
    pygame.display.set_caption("Snake")
    clock = pygame.time.Clock()
    font = pygame.font.SysFont(None, 28)

    snake = [(GRID_W // 2, GRID_H // 2)]
    direction = (1, 0)
    pending_dir = direction
    food = random_food(snake)
    score = 0
    game_over = False

    while True:
        for event in pygame.event.get():
            if event.type == pygame.QUIT:
                pygame.quit()
                sys.exit(0)
            if event.type == pygame.KEYDOWN:
                if event.key == pygame.K_q:
                    pygame.quit()
                    sys.exit(0)
                if game_over and event.key == pygame.K_r:
                    snake = [(GRID_W // 2, GRID_H // 2)]
                    direction = (1, 0)
                    pending_dir = direction
                    food = random_food(snake)
                    score = 0
                    game_over = False
                if event.key == pygame.K_UP and direction != (0, 1):
                    pending_dir = (0, -1)
                elif event.key == pygame.K_DOWN and direction != (0, -1):
                    pending_dir = (0, 1)
                elif event.key == pygame.K_LEFT and direction != (1, 0):
                    pending_dir = (-1, 0)
                elif event.key == pygame.K_RIGHT and direction != (-1, 0):
                    pending_dir = (1, 0)

        if not game_over:
            direction = pending_dir
            hx, hy = snake[0]
            nx, ny = hx + direction[0], hy + direction[1]

            # Wall or self collision ends game.
            if nx < 0 or nx >= GRID_W or ny < 0 or ny >= GRID_H or (nx, ny) in snake:
                game_over = True
            else:
                snake.insert(0, (nx, ny))
                if (nx, ny) == food:
                    score += 1
                    food = random_food(snake)
                else:
                    snake.pop()

        screen.fill(BLACK)
        for s in snake:
            draw_cell(screen, s, GREEN)
        draw_cell(screen, food, RED)

        score_text = font.render(f"Score: {score}", True, WHITE)
        screen.blit(score_text, (8, 8))

        if game_over:
            msg = font.render("Game Over - Press R to restart, Q to quit", True, WHITE)
            rect = msg.get_rect(center=(WIDTH // 2, HEIGHT // 2))
            screen.blit(msg, rect)

        pygame.display.flip()
        clock.tick(FPS)


if __name__ == "__main__":
    run()
"#;

            let write_result = (|| -> Result<(), String> {
                fs::write(project_dir.join("README.md"), readme).map_err(|e| e.to_string())?;
                fs::write(project_dir.join("requirements.txt"), requirements).map_err(|e| e.to_string())?;
                fs::write(project_dir.join("main.py"), main_py).map_err(|e| e.to_string())?;
                Ok(())
            })();

            match write_result {
                Ok(()) => "✅ 已生成示例项目：examples/snake-game\n- main.py\n- requirements.txt\n- README.md".to_string(),
                Err(e) => format!("Recipe failed: {e}"),
            }
        }
        _ => format!("Unknown recipe: {recipe_id}"),
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
    // meta: session history
    if matches!(lower.as_str(), "会话列表" | "session list" | "sessions" | "历史会话" | "查看会话") {
        return Some(Intent::SessionList);
    }
    for prefix in ["载入会话 ", "恢复会话 ", "session load ", "/session load ", "切换会话 "] {
        if let Some(id) = text.strip_prefix(prefix) {
            return Some(Intent::SessionLoad(id.trim().to_string()));
        }
    }
    if matches!(lower.as_str(), "保存会话" | "session save" | "/session save") {
        return Some(Intent::SessionSave(None));
    }
    for prefix in ["保存会话 ", "session save ", "/session save "] {
        if let Some(label) = text.strip_prefix(prefix) {
            return Some(Intent::SessionSave(Some(label.trim().to_string())));
        }
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

    if let Some(action) = parse_action_intent(text) {
        return Some(action);
    }
    // Copilot Chat bridge
    for prefix in &["问:", "ai:", "copilot:", "问copilot:", "ask:"] {
        if let Some(q) = text.strip_prefix(prefix) {
            return Some(Intent::CopilotAsk(q.trim().to_string()));
        }
    }
    if matches!(lower.as_str(), "chat 历史" | "ai 历史" | "copilot 历史" | "聊天历史" | "chat history" | "copilot sessions") {
        return Some(Intent::CopilotSessions);
    }
    for prefix in &["chat 历史 ", "copilot 历史 ", "chat history "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return Some(Intent::CopilotHistory(n));
            }
        }
    }
    if matches!(lower.as_str(), "最近对话" | "最近消息" | "recent chat" | "查看最近对话") {
        return Some(Intent::CopilotHistory(0));
    }
    if let Some((description, steps)) = auto_plan_from_request(text) {
        return Some(Intent::SetPlan { description, steps });
    }
    None
}

fn auto_plan_from_request(text: &str) -> Option<(String, Vec<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    let looks_like_task = ["帮我", "请", "做", "开发", "实现", "创建", "写一个", "build", "implement"]
        .iter()
        .any(|k| lower.contains(k));

    if !looks_like_task || trimmed.len() < 6 {
        return None;
    }

    if lower.contains("贪吃蛇") || (lower.contains("snake") && lower.contains("游戏")) {
        return Some((
            "贪吃蛇游戏项目".to_string(),
            vec![
                "@recipe:snake_python_bootstrap".to_string(),
                "读取文件 examples/snake-game/README.md".to_string(),
                "列出目录 examples/snake-game".to_string(),
            ],
        ));
    }

    Some((
        trimmed.to_string(),
        vec![
            "列出目录 .".to_string(),
            format!("搜索 {trimmed}"),
            "查看变更".to_string(),
        ],
    ))
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

    // Natural-language shortcuts (Chinese) so users do not need slash commands.
    if let Some(path) = lower
        .strip_prefix("读取文件 ")
        .or_else(|| lower.strip_prefix("读取 "))
        .or_else(|| lower.strip_prefix("打开文件 "))
        .or_else(|| lower.strip_prefix("查看文件 "))
    {
        return Some(Intent::Read(path.trim().to_string()));
    }
    if let Some(path) = lower
        .strip_prefix("列出目录 ")
        .or_else(|| lower.strip_prefix("查看目录 "))
        .or_else(|| lower.strip_prefix("看看目录 "))
    {
        return Some(Intent::List(path.trim().to_string()));
    }
    if lower.starts_with("查看变更") || lower.starts_with("查看diff") {
        let path = lower
            .strip_prefix("查看变更")
            .or_else(|| lower.strip_prefix("查看diff"))
            .unwrap_or("")
            .trim();
        return Some(Intent::Diff(path.to_string()));
    }
    if let Some(rest) = lower.strip_prefix("在 ") {
        if let Some((path, query)) = rest.split_once(" 中搜索 ") {
            return Some(Intent::Search {
                path: path.trim().to_string(),
                query: query.trim().to_string(),
            });
        }
    }
    if let Some(query) = lower.strip_prefix("搜索 ") {
        return Some(Intent::Search {
            path: ".".to_string(),
            query: query.trim().to_string(),
        });
    }
    if lower.starts_with("运行测试") || lower.starts_with("执行测试") {
        let filter = lower
            .strip_prefix("运行测试")
            .or_else(|| lower.strip_prefix("执行测试"))
            .unwrap_or("")
            .trim();
        return Some(Intent::Test(filter.to_string()));
    }

    if let Some(intent) = infer_action_intent_candle(text) {
        return Some(intent);
    }
    None
}

fn infer_action_intent_candle(text: &str) -> Option<Intent> {
    let (label, score) = candle_intent_label(text)?;
    if score < 0.55 {
        return None;
    }

    match label {
        "read" => {
            let path = extract_path_like_token(text).unwrap_or_else(|| "README.md".to_string());
            Some(Intent::Read(path))
        }
        "list" => {
            let path = extract_path_like_token(text).unwrap_or_else(|| ".".to_string());
            Some(Intent::List(path))
        }
        "search" => {
            let query = extract_search_query(text).unwrap_or_else(|| text.trim().to_string());
            Some(Intent::Search {
                path: ".".to_string(),
                query,
            })
        }
        "diff" => Some(Intent::Diff("".to_string())),
        "test" => Some(Intent::Test("".to_string())),
        _ => None,
    }
}

fn candle_intent_label(text: &str) -> Option<(&'static str, f32)> {
    let device = Device::Cpu;
    let features = candle_features(text);

    let x = Tensor::from_slice(&features, (1, features.len()), &device).ok()?;
    let w = Tensor::from_slice(
        &[
            // read_kw
            2.4f32, -0.2, -0.1, -0.1, -0.1, 0.0,
            // list_kw
            -0.1, 2.4, -0.1, -0.1, -0.1, 0.0,
            // search_kw
            -0.1, -0.1, 2.4, -0.1, -0.1, 0.0,
            // diff_kw
            -0.1, -0.1, -0.1, 2.4, -0.1, 0.0,
            // test_kw
            -0.1, -0.1, -0.1, -0.1, 2.4, 0.0,
            // has_path_like
            0.5, 0.5, 0.3, 0.2, 0.2, 0.0,
            // has_dot_or_slash
            0.4, 0.3, 0.2, 0.2, 0.1, 0.0,
            // is_question_like
            -0.2, -0.2, -0.1, -0.1, -0.1, 0.8,
            // has_plan_like
            -0.2, -0.2, -0.2, -0.2, -0.2, 1.0,
            // fallback_bias_feat
            0.0, 0.0, 0.0, 0.0, 0.0, 0.2,
        ],
        (features.len(), 6),
        &device,
    )
    .ok()?;
    let b = Tensor::from_slice(&[0.0f32, 0.0, 0.0, 0.0, 0.0, 0.2], 6, &device).ok()?;

    let logits = x.matmul(&w).ok()?.broadcast_add(&b).ok()?;
    let probs = softmax(&logits, 1).ok()?;
    let p = probs.squeeze(0).ok()?.to_vec1::<f32>().ok()?;

    let labels = ["read", "list", "search", "diff", "test", "other"];
    let mut best_idx = 0usize;
    let mut best_score = f32::MIN;
    for (i, v) in p.iter().enumerate() {
        if *v > best_score {
            best_idx = i;
            best_score = *v;
        }
    }
    Some((labels[best_idx], best_score))
}

fn candle_features(prompt: &str) -> Vec<f32> {
    let s = prompt.to_lowercase();
    let has = |tokens: &[&str]| tokens.iter().any(|t| s.contains(t));

    let read_kw = has(&["read", "读取", "打开", "查看文件", "/read"]);
    let list_kw = has(&["ls", "list", "列出", "目录", "文件夹", "/ls"]);
    let search_kw = has(&["search", "搜索", "查找", "grep", "/search"]);
    let diff_kw = has(&["diff", "变更", "改动", "对比", "/diff"]);
    let test_kw = has(&["test", "测试", "pytest", "cargo test", "/test"]);

    let has_path_like = has(&[".md", ".rs", ".py", "src/", "docs/", "readme", "."]);
    let has_dot_or_slash = s.contains('.') || s.contains('/') || s.contains('\\');
    let is_question_like = has(&["吗", "是不是", "能否", "是否", "?"]);
    let has_plan_like = has(&["plan", "计划", "任务", "步骤", "继续"]);

    vec![
        b2f(read_kw),
        b2f(list_kw),
        b2f(search_kw),
        b2f(diff_kw),
        b2f(test_kw),
        b2f(has_path_like),
        b2f(has_dot_or_slash),
        b2f(is_question_like),
        b2f(has_plan_like),
        1.0,
    ]
}

fn b2f(v: bool) -> f32 {
    if v { 1.0 } else { 0.0 }
}

fn extract_path_like_token(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|tok| {
        let cleaned = tok.trim_matches(|c: char| c == '"' || c == '\'' || c == '，' || c == ',' || c == '。');
        if cleaned.contains('/')
            || cleaned.contains('\\')
            || cleaned.ends_with(".md")
            || cleaned.ends_with(".rs")
            || cleaned.ends_with(".py")
            || cleaned == "."
        {
            Some(cleaned.to_string())
        } else {
            None
        }
    })
}

fn extract_search_query(text: &str) -> Option<String> {
    if let Some(rest) = text.split_once("搜索") {
        let q = rest.1.trim();
        if !q.is_empty() {
            return Some(q.to_string());
        }
    }
    None
}

// ---- plan body parsing -------------------------------------------------

fn parse_plan_body(text: &str) -> (String, Vec<String>) {
    let mut description = String::new();
    let mut steps: Vec<String> = Vec::new();
    let mut first_non_empty = true;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }

        // Try to extract list-prefix steps (1. 2. - * • etc)
        if let Some(step) = strip_list_prefix(line) {
            steps.push(step.to_string());
        } else if first_non_empty {
            // First non-empty line without prefix is the description
            description = line.to_string();
            first_non_empty = false;
        } else {
            // Subsequent lines without prefix: split by comma as step list.
            for item in line.split([',', '，', ';', '；']) {
                let item = item.trim();
                if !item.is_empty() {
                    steps.push(item.to_string());
                }
            }
        }
    }

    if description.is_empty() && !steps.is_empty() {
        description = "任务".to_string();
    } else if description.is_empty() {
        description = text.trim().to_string();
    }
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

fn has_meaningful_session(session: &session_store::UserSession) -> bool {
    session.current_task.is_some()
        || !session.pending_steps.is_empty()
        || session.last_action.is_some()
        || session.last_result.is_some()
}

fn fmt_snapshot_list(items: &[session_store::SessionSnapshotMeta]) -> String {
    if items.is_empty() {
        return "暂无历史会话。发送“保存会话 <名称>”可保存当前进度。".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("🗂 历史会话（{}）", items.len()));
    for (i, item) in items.iter().take(20).enumerate() {
        let task = item.current_task.as_deref().unwrap_or("（无任务）");
        let label = item.label.as_deref().unwrap_or("manual");
        lines.push(format!(
            "{}. {} | {} | 待办:{} | id:{}",
            i + 1,
            task,
            label,
            item.pending_steps,
            item.id
        ));
    }
    lines.push("发送“载入会话 <id>”恢复并继续。".to_string());
    lines.join("\n")
}

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
    "HarborBeacon 可用命令如下:\n\
\n【🤖 Copilot 对话】\n\
问: <问题>           直接向 Copilot 提问\n\
chat 历史            查看 VS Code Copilot 会话\n\
chat 历史 <n>        查看第 n 个会话的提问摘要\n\
最近对话             查看最近 10 条提问\n\
\n【代码操作】\n\
read <path>          读取文件\n\
ls <path>            列出目录\n\
search <path> <q>    搜索文本\n\
diff [path]          查看 git diff\n\
/patch <patch>       应用补丁\n\
test [filter]        运行测试\n\
（也支持自然语言：如\"读取文件 README.md\"）\n\
\n【会话命令】\n\
plan <任务>          记录任务（换行写步骤）\n\
继续                 执行下一步\n\
重试                 重新执行上次操作\n\
状态                 查看当前任务进展\n\
会话列表             查看历史会话\n\
保存会话 [名称]      保存当前会话快照\n\
载入会话 <id>        恢复历史会话并继续\n\
清除                 清除会话（自动归档）\n\
切换编码/切换规划     切换工作模式"
        .to_string()
}

// ---- Copilot Chat Bridge -----------------------------------------------

struct VscodeChatSession {
    id: String,
    title: String,
    last_message_date: u64,
}

/// Scan all VS Code workspaceStorage databases for Copilot Chat sessions.
fn read_vscode_chat_sessions() -> Vec<VscodeChatSession> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let ws_root = std::path::Path::new(&appdata)
        .join("Code")
        .join("User")
        .join("workspaceStorage");
    if !ws_root.exists() {
        return vec![];
    }
    let mut all: Vec<VscodeChatSession> = vec![];
    let Ok(entries) = std::fs::read_dir(&ws_root) else { return vec![] };
    for entry in entries.flatten() {
        let db_path = entry.path().join("state.vscdb");
        if !db_path.exists() { continue; }
        let Ok(conn) = SqliteConn::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { continue };
        let Ok(json_str): rusqlite::Result<String> = conn.query_row(
            "SELECT value FROM ItemTable WHERE key = 'chat.ChatSessionStore.index'",
            [], |row| row.get(0),
        ) else { continue };
        let Ok(data): Result<serde_json::Value, _> = serde_json::from_str(&json_str) else { continue };
        if let Some(map) = data["entries"].as_object() {
            for (id, meta) in map {
                let title = meta["title"].as_str().unwrap_or("(no title)").to_string();
                let ts = meta["lastMessageDate"].as_u64().unwrap_or(0);
                all.push(VscodeChatSession { id: id.clone(), title, last_message_date: ts });
            }
        }
    }
    all.sort_by(|a, b| b.last_message_date.cmp(&a.last_message_date));
    all
}

/// Read recent user-typed prompts from the most recent VS Code Copilot Chat interactive session.
fn read_vscode_recent_messages() -> Vec<String> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let ws_root = std::path::Path::new(&appdata)
        .join("Code")
        .join("User")
        .join("workspaceStorage");
    if !ws_root.exists() { return vec![]; }

    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = vec![];
    if let Ok(entries) = std::fs::read_dir(&ws_root) {
        for entry in entries.flatten() {
            let db_path = entry.path().join("state.vscdb");
            if let Ok(meta) = std::fs::metadata(&db_path) {
                if let Ok(mt) = meta.modified() {
                    candidates.push((mt, db_path));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, db_path) in candidates.iter().take(5) {
        let Ok(conn) = SqliteConn::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { continue };
        let Ok(json_str): rusqlite::Result<String> = conn.query_row(
            "SELECT value FROM ItemTable WHERE key = 'memento/interactive-session'",
            [], |row| row.get(0),
        ) else { continue };
        let Ok(data): Result<serde_json::Value, _> = serde_json::from_str(&json_str) else { continue };
        let msgs: Vec<String> = data["history"]["copilot"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|item| item["inputText"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if !msgs.is_empty() { return msgs; }
    }
    vec![]
}

/// Try to resolve a usable GitHub OAuth token (explicit arg → gh CLI).
fn resolve_github_token(explicit: Option<&str>) -> Result<String, String> {
    if let Some(t) = explicit {
        if !t.is_empty() { return Ok(t.to_string()); }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .map_err(|e| format!("gh CLI 未找到: {e}\n请设置 HARBOR_GITHUB_TOKEN 环境变量"))?;
    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() { return Ok(token); }
    }
    Err("无法获取 GitHub 令牌。\n请设置 HARBOR_GITHUB_TOKEN 环境变量或通过 gh auth login 登录。".to_string())
}

/// Exchange a GitHub OAuth token for a short-lived Copilot API token.
async fn get_copilot_access_token(http: &HttpClient, github_token: &str) -> Result<String, String> {
    // GitHub side can behave differently for PAT/OAuth tokens.
    // Try the common combinations before failing.
    let attempts = [
        ("https://api.github.com/copilot_internal/v2/token", format!("Bearer {github_token}")),
        ("https://api.github.com/copilot_internal/v2/token", format!("token {github_token}")),
        ("https://api.github.com/copilot_internal/token", format!("Bearer {github_token}")),
        ("https://api.github.com/copilot_internal/token", format!("token {github_token}")),
    ];

    let mut last_err = String::new();
    for (url, auth_value) in attempts {
        let resp = match http
            .get(url)
            .header("Authorization", auth_value)
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.15.0")
            .header("editor-version", "vscode/1.87.0")
            .header("editor-plugin-version", "copilot-chat/0.15.0")
            .send()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("令牌请求失败: {e}");
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            last_err = format!("令牌接口返回 {status}: {body}");
            continue;
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(token) = json["token"].as_str() {
            return Ok(token.to_string());
        }
        last_err = format!("令牌响应缺少 token 字段: {json}");
    }

    if last_err.contains("404") || last_err.contains("Not Found") {
        return Err(
            "GitHub Copilot token endpoint 返回 404。\n\
这通常表示当前账号/环境不支持通过 REST 路径获取 Copilot 会话令牌。\n\
建议：\n\
1) 在本机安装并登录 GitHub Copilot CLI，然后改用 CLI 方式提问；\n\
2) 或继续使用『chat 历史 / 最近对话』功能（不依赖该令牌接口）。"
                .to_string(),
        );
    }

    Err(last_err)
}

async fn copilot_ask_via_rest(http: &HttpClient, github_token: Option<&str>, question: &str) -> Result<String, String> {
    let gh_token = resolve_github_token(github_token)?;
    let copilot_token = get_copilot_access_token(http, &gh_token).await?;

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are GitHub Copilot, an AI programming assistant. Answer concisely and helpfully. Respond in the same language as the user's question."},
            {"role": "user", "content": question}
        ],
        "stream": false,
        "max_tokens": 2048
    });

    let resp = http
        .post("https://api.githubcopilot.com/chat/completions")
        .header("Authorization", format!("Bearer {copilot_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "HarborBeacon/0.1")
        .header("editor-version", "vscode/1.87.0")
        .header("editor-plugin-version", "copilot-chat/0.15.0")
        .header("Copilot-Integration-Id", "copilot-chat")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Completions 请求失败: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("Copilot API 返回 {status}: {body_text}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("响应格式异常: {json}"))
}

fn copilot_ask_via_gh_cli(question: &str, github_token: Option<&str>) -> Result<String, String> {
    let mut errors: Vec<String> = Vec::new();
    let mut direct_checked = false;

    // Fail fast if gh auth is unavailable to avoid interactive login flows.
    if let Ok(status) = std::process::Command::new("gh").args(["auth", "status"]).output() {
        if !status.status.success() {
            return Err("GitHub CLI 未登录，请先执行 gh auth login 后重试。".to_string());
        }
    }

    // 1) Prefer direct CLI binary if installed by MSI.
    if let Ok(localapp) = std::env::var("LOCALAPPDATA") {
        let direct = std::path::Path::new(&localapp)
            .join("GitHubCopilotCLI")
            .join("copilot.exe");
        if direct.exists() {
            direct_checked = true;
            let mut cmd = std::process::Command::new(&direct);
            cmd.args(["-p", question])
                .env("GH_PROMPT_DISABLED", "1")
                .env("COPILOT_CLI_TELEMETRY_OPTOUT", "1");
            if let Some(t) = github_token {
                cmd.env("GH_TOKEN", t).env("GITHUB_TOKEN", t);
            }

            match cmd.output() {
                Ok(output) => {
                    if output.status.success() {
                        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !text.is_empty() {
                            return Ok(text);
                        }
                        return Err("Copilot CLI 未返回内容，请检查 GitHub 账号登录状态。".to_string());
                    }
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let merged = if !stderr.is_empty() { stderr } else { stdout };
                    if merged.contains("auth login") || merged.contains("authenticate") || merged.contains("not logged") {
                        return Err("Copilot CLI 需要 GitHub 登录，请先执行 gh auth login 后重试。".to_string());
                    }
                    if !merged.is_empty() {
                        return Err(format!("Copilot CLI 调用失败: {merged}"));
                    }
                    return Err("Copilot CLI 调用失败（无错误输出）".to_string());
                }
                Err(e) => return Err(format!("无法调用 Copilot CLI: {e}")),
            }
        }
    }

    // If direct executable exists, never fall through to gh wrapper to avoid interactive hangs.
    if direct_checked {
        return Err("Copilot CLI 未能返回结果。".to_string());
    }

    // 2) Fallback to gh wrapper.
    let mut gh_cmd = std::process::Command::new("gh");
    gh_cmd
        .args(["copilot", "-p", question])
        .env("GH_PROMPT_DISABLED", "1")
        .env("COPILOT_CLI_TELEMETRY_OPTOUT", "1");
    if let Some(t) = github_token {
        gh_cmd.env("GH_TOKEN", t).env("GITHUB_TOKEN", t);
    }

    match gh_cmd.output() {
        Ok(output) => {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !text.is_empty() {
                    return Ok(text);
                }
            }

            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let merged = if !stderr.is_empty() { stderr } else { stdout };

            if merged.contains("Cannot find GitHub Copilot CLI") {
                return Err(
                    "REST 路径不可用，且本机未安装 GitHub Copilot CLI。\n\
请先安装 Copilot CLI（或执行 gh copilot 并选择安装），然后重试『问: ...』。"
                        .to_string(),
                );
            }

            if merged.contains("not logged") || merged.contains("auth") || merged.contains("login") {
                return Err("gh copilot 未登录，请先执行 gh auth login 后重试。".to_string());
            }

            if !merged.is_empty() {
                errors.push(format!("gh copilot: {merged}"));
            }
        }
        Err(e) => errors.push(format!("无法调用 gh copilot: {e}")),
    }

    if errors.is_empty() {
        Err("CLI 回退失败（无输出）".to_string())
    } else {
        Err(format!("CLI 回退失败: {}", errors.join(" | ")))
    }
}

/// Ask a question to GitHub Copilot and return the response text.
async fn copilot_ask(http: &HttpClient, github_token: Option<&str>, question: &str) -> Result<String, String> {
    let gh_token = resolve_github_token(github_token).ok();

    let rest_result = if let Some(ref t) = gh_token {
        copilot_ask_via_rest(http, Some(t.as_str()), question).await
    } else {
        Err("未能获取 GitHub token，跳过 REST 路径。".to_string())
    };

    match rest_result {
        Ok(ans) => Ok(ans),
        Err(rest_err) => {
            // Fallback to command-line Copilot when REST endpoint is unavailable.
            match copilot_ask_via_gh_cli(question, gh_token.as_deref()) {
                Ok(ans) => Ok(ans),
                Err(cli_err) => Err(format!("REST 失败: {rest_err}\n\nCLI 回退失败: {cli_err}")),
            }
        }
    }
}

fn fmt_vscode_sessions(sessions: &[VscodeChatSession]) -> String {
    if sessions.is_empty() {
        return "未找到 VS Code Copilot Chat 会话。\n请先在 VS Code 中进行一些对话。".to_string();
    }
    let mut lines = vec![format!("💬 VS Code Copilot 历史（{}）", sessions.len())];
    for (i, s) in sessions.iter().take(20).enumerate() {
        let dt = fmt_elapsed(s.last_message_date);
        lines.push(format!("{}. [{}] {}", i + 1, dt, s.title));
    }
    lines.push("".to_string());
    lines.push("发送「chat 历史 <编号>」查看该会话的提问摘要。".to_string());
    lines.push("发送「问: <问题>」直接向 Copilot 提问。".to_string());
    lines.join("\n")
}

fn fmt_recent_messages(msgs: &[String]) -> String {
    if msgs.is_empty() {
        return "暂无最近消息记录。".to_string();
    }
    let recent: Vec<&String> = msgs.iter().rev().take(10).collect();
    let mut lines = vec!["💬 最近 Copilot 对话（你的提问）:".to_string()];
    for (i, m) in recent.iter().rev().enumerate() {
        let preview = if m.chars().count() > 80 {
            let cut: String = m.chars().take(80).collect();
            format!("{cut}…")
        } else {
            m.to_string()
        };
        lines.push(format!("{}. {}", i + 1, preview));
    }
    lines.join("\n")
}

fn fmt_elapsed(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let now = now_secs();
    let elapsed = now.saturating_sub(secs);
    if elapsed < 86400 { "今天".to_string() }
    else if elapsed < 172800 { "昨天".to_string() }
    else { format!("{}天前", elapsed / 86400) }
}

#[cfg(test)]
mod tests {
    use super::{auto_plan_from_request, infer_action_intent_candle, parse_intent, Intent};

    #[test]
    fn candle_fallback_infers_read() {
        let intent = infer_action_intent_candle("帮我打开 README.md 的内容");
        match intent {
            Some(Intent::Read(path)) => assert!(path.to_lowercase().contains("readme")),
            _ => panic!("expected read intent from candle fallback"),
        }
    }

    #[test]
    fn candle_fallback_infers_search() {
        let intent = infer_action_intent_candle("请帮我查找 websocket 相关代码");
        match intent {
            Some(Intent::Search { path, query }) => {
                assert_eq!(path, ".");
                assert!(query.contains("websocket") || query.contains("查找"));
            }
            _ => panic!("expected search intent from candle fallback"),
        }
    }

    #[test]
    fn auto_plan_generates_snake_recipe() {
        let plan = auto_plan_from_request("帮我做一个贪吃蛇游戏项目");
        let (_, steps) = plan.expect("expected auto plan");
        assert!(steps.iter().any(|s| s == "@recipe:snake_python_bootstrap"));
    }

    #[test]
    fn auto_plan_returns_none_for_short_chat() {
        let plan = auto_plan_from_request("你好");
        assert!(plan.is_none());
    }

    #[test]
    fn parse_session_list_intent() {
        match parse_intent("会话列表") {
            Some(Intent::SessionList) => {}
            _ => panic!("expected session list intent"),
        }
    }

    #[test]
    fn parse_session_load_intent() {
        match parse_intent("载入会话 1774430905-trip") {
            Some(Intent::SessionLoad(id)) => assert_eq!(id, "1774430905-trip"),
            _ => panic!("expected session load intent"),
        }
    }
}
