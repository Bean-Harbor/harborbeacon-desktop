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
    // dev-loop intents (edit/build/restart from Feishu)
    WriteFile { path: String, content: String },
    ReplaceInFile { path: String, old_text: String, new_text: String },
    Build,
    SelfRestart,
    ViewLogs,
    TerminalRun(String),
    TerminalConfirm,
    TerminalCancel,
    GitCommit(String),
    GitPush,
    // meta intents
    Continue,
    ContinueAll,
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
    AgentFix(String),
    SystemInstallHelp(String),
    Clear,
    Help,
}

/// Context passed through dispatch for build/restart operations.
struct AgentContext {
    desktop_root: String,
    app_id: String,
    app_secret: String,
    domain: String,
    workspace: String,
}

/// Dispatch result; if `restart` is Some, the main loop spawns the new binary and exits.
struct DispatchOutcome {
    reply: String,
    restart: Option<RestartInfo>,
}

struct RestartInfo {
    exe_path: String,
    args: Vec<String>,
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

    let agent_ctx = AgentContext {
        desktop_root: std::path::Path::new(&cli.workspace)
            .join("harborbeacon-desktop")
            .to_string_lossy()
            .to_string(),
        app_id: cli.app_id.clone(),
        app_secret: cli.app_secret.clone(),
        domain: cli.domain.clone(),
        workspace: cli.workspace.clone(),
    };

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
        let outcome = dispatch_with_session(&bridge, &store, &msg, &http, cli.github_token.as_deref(), &agent_ctx).await;
        info!(reply = %outcome.reply, "Action result");

        if !msg.message_id.is_empty() {
            let client = Arc::clone(&reply_client);
            let mid = msg.message_id.clone();
            let rt = outcome.reply.clone();
            // Await reply delivery before potential restart.
            let _ = client.reply_text(&mid, &rt).await;
        }

        // If a restart was requested, spawn the new binary and exit.
        if let Some(restart) = outcome.restart {
            info!(exe = %restart.exe_path, "Spawning new process and exiting for restart");
            match std::process::Command::new(&restart.exe_path)
                .args(&restart.args)
                .spawn()
            {
                Ok(child) => info!(pid = child.id(), "New process spawned"),
                Err(e) => error!(error = %e, "Failed to spawn new process"),
            }
            std::process::exit(0);
        }
    }

    info!("Message channel closed, exiting.");
}

// ---- session-aware dispatch -------------------------------------------- 

async fn dispatch_with_session(bridge: &BridgeBinding, store: &SessionStore, msg: &InboundMessage, http: &HttpClient, github_token: Option<&str>, ctx: &AgentContext) -> DispatchOutcome {
    let text = msg.text.trim();
    let mut session = store.load(&msg.sender_id);
    let mut restart_info: Option<RestartInfo> = None;

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
                            format!("还剩 {rem} 步，发送「继续」或「执行全部」")
                        }
                    )
                }
            }
            Intent::ContinueAll => {
                if session.pending_steps.is_empty() {
                    "无待执行步骤。\n发送 `plan <任务>` 设置计划，或直接执行命令。".to_string()
                } else {
                    let mut outputs = Vec::new();
                    while !session.pending_steps.is_empty() {
                        let step = session.pending_steps.remove(0);
                        let result = execute_plan_step(bridge, &step);
                        session.last_result = Some(result.clone());
                        outputs.push(format!("▸ 执行步骤: {step}\n{result}"));
                        // Stop early if a step failed
                        if result.contains('❌') {
                            let rem = session.pending_steps.len();
                            if rem > 0 {
                                outputs.push(format!("\n⚠️ 步骤失败，已暂停。还剩 {rem} 步，发送「继续」或「执行全部」恢复。"));
                            }
                            break;
                        }
                    }
                    session.updated_at = now_secs();
                    if session.pending_steps.is_empty() {
                        let _ = store.save_snapshot(&session, Some("auto-completed"));
                        outputs.push("\n✅ 所有步骤已执行完毕".to_string());
                    }
                    outputs.join("\n\n")
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
                store.save(&session).ok();
                return DispatchOutcome { reply: "🗑 会话已清除（已自动归档）".to_string(), restart: None };
            }
            // ---------- dev-loop ----------
            Intent::WriteFile { path, content } => {
                match actions::write_file(bridge, &path, &content) {
                    Ok(r) => format!("✅ {}", r.content),
                    Err(e) => format!("❌ 写入失败: {e}"),
                }
            }
            Intent::ReplaceInFile { path, old_text, new_text } => {
                do_replace_in_file(bridge, &path, &old_text, &new_text)
            }
            Intent::Build => do_build(&ctx.desktop_root),
            Intent::SelfRestart => {
                match do_build_and_restart(ctx) {
                    Ok((reply_msg, info)) => {
                        restart_info = Some(info);
                        reply_msg
                    }
                    Err(e) => e,
                }
            }
            Intent::ViewLogs => do_view_logs(&ctx.desktop_root),
            Intent::TerminalRun(command) => {
                if is_high_risk_terminal_command(&command) {
                    session.pending_terminal_command = Some(command.clone());
                    session.updated_at = now_secs();
                    format!(
                        "⚠️ 检测到高风险命令，需二次确认后执行:\n{}\n\n发送“确认执行”继续，或发送“取消执行”放弃。",
                        command
                    )
                } else {
                    let result = do_terminal_command(&ctx.workspace, &command);
                    session.last_action = Some(format!("/terminal {command}"));
                    session.last_result = Some(result.clone());
                    session.updated_at = now_secs();
                    result
                }
            }
            Intent::TerminalConfirm => {
                match session.pending_terminal_command.clone() {
                    Some(command) => {
                        session.pending_terminal_command = None;
                        let result = do_terminal_command(&ctx.workspace, &command);
                        session.last_action = Some(format!("/terminal {command}"));
                        session.last_result = Some(result.clone());
                        session.updated_at = now_secs();
                        result
                    }
                    None => "当前没有待确认的高风险终端命令。".to_string(),
                }
            }
            Intent::TerminalCancel => {
                if session.pending_terminal_command.take().is_some() {
                    session.updated_at = now_secs();
                    "✅ 已取消待确认的高风险终端命令。".to_string()
                } else {
                    "当前没有待确认的高风险终端命令。".to_string()
                }
            }
            Intent::GitCommit(message) => do_git_commit(&ctx.workspace, &message),
            Intent::GitPush => do_git_push(&ctx.workspace),
            // ---------- copilot bridge ----------
            Intent::AgentFix(user_desc) => {
                // Build context from session's last error/result + last action
                let last_action = session.last_action.as_deref().unwrap_or("(unknown)");
                let last_result = session.last_result.as_deref().unwrap_or("(no previous result)");
                let desc_part = if user_desc.is_empty() {
                    String::new()
                } else {
                    format!("\nUser note: {user_desc}")
                };
                let prompt = format!(
                    "The user ran a command/operation via Feishu remote agent and it failed. \
                     Please analyze the error and suggest a concrete fix.\n\
                     \n## Last command\n```\n{}\n```\n\
                     \n## Error output\n```\n{}\n```{}\
                     \n\nProvide a short analysis and the exact corrected command or code fix. \
                     Reply in the user's language (Chinese).",
                    last_action, last_result, desc_part
                );
                match copilot_ask(http, github_token, &prompt).await {
                    Ok(ans) => format!("🔍 分析结果:\n\n{ans}"),
                    Err(e) => {
                        // Fallback: provide local analysis
                        format!(
                            "⚠️ Copilot 不可用 ({e})，以下是上次错误上下文:\n\
                             命令: {last_action}\n\
                             输出:\n{last_result}\n\n\
                             提示: 检查命令拼写是否正确，或使用 /terminal <完整命令> 重试。"
                        )
                    }
                }
            }
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
            Intent::SystemInstallHelp(task) => system_install_help_text(&task),
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
    DispatchOutcome { reply, restart: restart_info }
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

    // Handle /terminal steps directly in plans
    if let Some(cmd) = step.strip_prefix("/terminal ") {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            return do_terminal_command(&bridge.workspace.path, cmd);
        }
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

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() < prefix.len() {
        return None;
    }
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

fn parse_intent(text: &str) -> Option<Intent> {
    if text.is_empty() { return Some(Intent::Help); }
    let lower = text.to_lowercase();

    // meta: continue
    if matches!(lower.as_str(), "继续" | "continue" | "继续执行" | "下一步") {
        return Some(Intent::Continue);
    }
    // meta: continue all
    if matches!(lower.as_str(), "执行全部" | "全部执行" | "run all" | "执行所有步骤" | "全部继续" | "一键执行") {
        return Some(Intent::ContinueAll);
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
    // meta: copilot-style /agent command
    if let Some(rest) = strip_prefix_ci(text, "/agent") {
        let arg_raw = rest.trim();
        let arg = arg_raw.to_lowercase();
        // /agent fix ... — debug and fix the last error
        if arg == "fix" || arg.starts_with("fix ") || arg.starts_with("fix:") {
            let desc = arg.strip_prefix("fix").unwrap_or("").trim_start_matches(':').trim().to_string();
            return Some(Intent::AgentFix(desc));
        }
        return Some(match arg.as_str() {
            "" | "status" => Intent::Status,
            "coding" | "code" => Intent::SwitchMode(SessionMode::Coding),
            "planning" | "plan" => Intent::SwitchMode(SessionMode::Planning),
            "help" => Intent::Help,
            _ => {
                // Try to parse as a known command first
                if let Some(intent) = parse_intent(arg_raw) {
                    intent
                } else {
                    // Unknown task: send to Copilot for intelligent analysis
                    Intent::CopilotAsk(format!(
                        "用户通过飞书远程代理请求执行以下任务，工作区为本地 Rust 项目。\n\
                         请给出可直接执行的具体步骤（优先用命令行）。\n\
                         用户请求: {}",
                        arg_raw
                    ))
                }
            }
        });
    }
    // standalone: fix / 修复 (without /agent prefix)
    for prefix in &["/fix ", "/fix:", "fix ", "修复 "] {
        if let Some(rest) = strip_prefix_ci(text, prefix) {
            return Some(Intent::AgentFix(rest.trim().to_string()));
        }
    }
    if matches!(lower.as_str(), "/fix" | "fix" | "修复" | "修复这个") {
        return Some(Intent::AgentFix(String::new()));
    }
    // meta: set plan (single-line prefix)
    for prefix in &["/plan ", "plan ", "计划 ", "规划 ", "/计划 ", "/规划 ", "做计划 ", "做规划 "] {
        if let Some(rest) = strip_prefix_ci(text, prefix) {
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

    // ---- dev-loop commands ----
    // build
    if matches!(lower.as_str(), "build" | "/build" | "编译" | "构建") {
        return Some(Intent::Build);
    }
    // restart
    if matches!(lower.as_str(), "restart" | "/restart" | "重启" | "重启服务" | "rebuild" | "/rebuild") {
        return Some(Intent::SelfRestart);
    }
    // logs
    if matches!(lower.as_str(), "logs" | "/logs" | "日志" | "查看日志" | "最新日志" | "错误日志") {
        return Some(Intent::ViewLogs);
    }
    // terminal
    for prefix in &["/terminal ", "terminal "] {
        if let Some(rest) = strip_prefix_ci(text, prefix) {
            let cmd = rest.trim();
            if !cmd.is_empty() {
                return Some(Intent::TerminalRun(cmd.to_string()));
            }
        }
    }
    if matches!(lower.as_str(), "确认执行" | "confirm" | "/confirm" | "yes") {
        return Some(Intent::TerminalConfirm);
    }
    if matches!(lower.as_str(), "取消执行" | "cancel" | "/cancel" | "no") {
        return Some(Intent::TerminalCancel);
    }
    // git commit
    for prefix in &["/commit ", "commit ", "提交 ", "git commit "] {
        if let Some(rest) = strip_prefix_ci(text, prefix) {
            let msg = rest.trim();
            let msg = if msg.is_empty() { "feishu: auto-commit" } else { msg };
            return Some(Intent::GitCommit(msg.to_string()));
        }
    }
    // git push
    if matches!(lower.as_str(), "push" | "/push" | "推送" | "git push") {
        return Some(Intent::GitPush);
    }
    // write file: /write <path>\n<content>
    if let Some(rest) = strip_prefix_ci(text, "/write ") {
        if let Some(nl) = rest.find('\n') {
            let path = rest[..nl].trim().to_string();
            let content = rest[nl + 1..].to_string();
            return Some(Intent::WriteFile { path, content });
        }
    }
    // replace in file: /replace <path>\n<<<\n<old>\n===\n<new>\n>>>
    if let Some(rest) = strip_prefix_ci(text, "/replace ") {
        if let Some(intent) = parse_replace_command(rest) {
            return Some(intent);
        }
    }

    if let Some(action) = parse_action_intent(text) {
        return Some(action);
    }
    // Copilot Chat bridge
    for prefix in &["/ask ", "ask "] {
        if let Some(q) = strip_prefix_ci(text, prefix) {
            return Some(Intent::CopilotAsk(q.trim().to_string()));
        }
    }
    for prefix in &["问:", "ai:", "copilot:", "问copilot:", "ask:"] {
        if let Some(q) = strip_prefix_ci(text, prefix) {
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
    if looks_like_system_install_request(text) {
        return Some(Intent::SystemInstallHelp(text.trim().to_string()));
    }
    // Recognize common actionable natural-language requests
    if let Some(intent) = match_natural_task(&lower, text) {
        return Some(intent);
    }
    if let Some((description, steps)) = auto_plan_from_request(text) {
        return Some(Intent::SetPlan { description, steps });
    }
    None
}

fn looks_like_system_install_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    let has_install = lower.contains("install")
        || lower.contains("安装")
        || lower.contains("setup")
        || lower.contains("winget")
        || lower.contains("choco");
    let has_system_target = lower.contains("powershell")
        || lower.contains("软件")
        || lower.contains("系统")
        || lower.contains("aka.ms/")
        || lower.contains("http://")
        || lower.contains("https://");
    has_install && has_system_target
}

fn system_install_help_text(task: &str) -> String {
    format!(
        "⚠️ 当前飞书代理不支持直接执行系统级安装（为避免误操作，执行器仅开放工作区读写/搜索/diff/patch/test）。\n\n请求: {task}\n\n可选方案:\n1. 在电脑本机执行安装命令（推荐）\n   winget install --id Microsoft.PowerShell --source winget\n2. 安装后回到飞书发送“继续”，我会继续代码任务。\n\n如果你希望我远程触发系统安装，我可以下一步为你加“需确认后执行”的受控命令通道。"
    )
}

/// Match common natural-language task requests to direct intents.
fn match_natural_task(lower: &str, _text: &str) -> Option<Intent> {
    // Git sync / push to remote
    let is_git_sync = lower.contains("同步") && (lower.contains("github") || lower.contains("git") || lower.contains("远程") || lower.contains("代码"))
        || (lower.contains("push") && (lower.contains("代码") || lower.contains("github") || lower.contains("code")))
        || lower.contains("推到github") || lower.contains("推送代码") || lower.contains("上传代码");
    if is_git_sync {
        // The workspace has two git repos:
        //   parent: HarborBeacon-LocalAgent-Project-git (workspace root)
        //   child:  harborbeacon-desktop/ (nested repo with actual code)
        // We must commit+push in BOTH, child first.
        return Some(Intent::SetPlan {
            description: "同步代码到 GitHub（子仓库 + 父仓库）".to_string(),
            steps: vec![
                "/terminal Write-Host '=== harborbeacon-desktop (子仓库) ==='; git -C harborbeacon-desktop status".to_string(),
                "/terminal git -C harborbeacon-desktop add -A".to_string(),
                "/terminal git -C harborbeacon-desktop diff --cached --quiet; if ($LASTEXITCODE -ne 0) { git -C harborbeacon-desktop commit -m 'sync: update from feishu agent' } else { Write-Host 'harborbeacon-desktop: nothing to commit' }".to_string(),
                "/terminal git -C harborbeacon-desktop push".to_string(),
                "/terminal Write-Host '=== parent repo ==='; git status".to_string(),
                "/terminal git add -A; git diff --cached --quiet; if ($LASTEXITCODE -ne 0) { git commit -m 'chore: update desktop submodule pointer' } else { Write-Host 'parent: nothing to commit' }".to_string(),
                "/terminal git push".to_string(),
            ],
        });
    }
    // Build project
    let is_build = (lower.contains("编译") || lower.contains("构建") || lower.contains("build"))
        && (lower.contains("项目") || lower.contains("代码") || lower.contains("agent") || lower.contains("project"));
    if is_build {
        return Some(Intent::Build);
    }
    // Run tests
    let is_test = (lower.contains("测试") || lower.contains("test") || lower.contains("跑测试"))
        && !lower.contains("写");
    if is_test {
        return Some(Intent::Test(String::new()));
    }
    // Check git diff/changes
    let is_diff = lower.contains("查看变更") || lower.contains("看看改了什么") || lower.contains("有什么改动");
    if is_diff {
        return Some(Intent::Diff(String::new()));
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

    // For unrecognized natural-language tasks, don't create a useless generic plan.
    // Instead, return None and let it fall through to CopilotAsk in the /agent handler.
    None
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

// ---- dev-loop helper functions -----------------------------------------

fn parse_replace_command(rest: &str) -> Option<Intent> {
    // Format: path\n<<<\nold text\n===\nnew text\n>>>
    let nl = rest.find('\n')?;
    let path = rest[..nl].trim().to_string();
    let body = &rest[nl + 1..];
    let body = body.strip_prefix("<<<").unwrap_or(body).trim_start_matches('\n');
    let parts: Vec<&str> = body.splitn(2, "\n===\n").collect();
    if parts.len() != 2 {
        return None;
    }
    let old_text = parts[0].to_string();
    let new_text = parts[1].strip_suffix("\n>>>").unwrap_or(parts[1]).to_string();
    Some(Intent::ReplaceInFile { path, old_text, new_text })
}

fn do_replace_in_file(bridge: &BridgeBinding, path: &str, old_text: &str, new_text: &str) -> String {
    let abs = match bridge.resolve(path) {
        Ok(p) => p,
        Err(e) => return format!("❌ 路径错误: {e}"),
    };
    let content = match fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => return format!("❌ 读取失败: {e}"),
    };
    let count = content.matches(old_text).count();
    if count == 0 {
        return format!("❌ 未找到匹配文本:\n{old_text}");
    }
    let updated = content.replacen(old_text, new_text, 1);
    if let Err(e) = fs::write(&abs, &updated) {
        return format!("❌ 写入失败: {e}");
    }
    format!("✅ 已替换 {path} 中 {count} 处匹配（已替换第1处）\n变更前 {} 字节 → 变更后 {} 字节", content.len(), updated.len())
}

fn next_target_dir(desktop_root: &str) -> String {
    let root = std::path::Path::new(desktop_root);
    let mut max_n = 0u32;
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(suffix) = name.strip_prefix("target_feishu_") {
                if let Ok(n) = suffix.parse::<u32>() {
                    max_n = max_n.max(n);
                }
            }
        }
    }
    format!("target_feishu_{}", max_n + 1)
}

fn do_build(desktop_root: &str) -> String {
    let target_dir = next_target_dir(desktop_root);
    let output = std::process::Command::new("cargo")
        .current_dir(desktop_root)
        .args(["build", "--target-dir", &target_dir, "-p", "harborbeacon-desktop-app"])
        .output();
    match output {
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                format!("✅ 编译成功\ntarget: {target_dir}\n{}", last_n_lines(&stderr, 5))
            } else {
                format!("❌ 编译失败\n{}", last_n_lines(&stderr, 30))
            }
        }
        Err(e) => format!("❌ 无法启动 cargo: {e}"),
    }
}

fn do_build_and_restart(ctx: &AgentContext) -> Result<(String, RestartInfo), String> {
    let target_dir = next_target_dir(&ctx.desktop_root);
    let output = std::process::Command::new("cargo")
        .current_dir(&ctx.desktop_root)
        .args(["build", "--target-dir", &target_dir, "-p", "harborbeacon-desktop-app"])
        .output()
        .map_err(|e| format!("❌ 无法启动 cargo: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("❌ 编译失败，取消重启\n{}", last_n_lines(&stderr, 30)));
    }

    let exe_path = std::path::Path::new(&ctx.desktop_root)
        .join(&target_dir)
        .join("debug")
        .join("harborbeacon-desktop-app.exe");
    if !exe_path.exists() {
        return Err(format!("❌ 编译产物不存在: {}", exe_path.display()));
    }

    let args = vec![
        "--app-id".to_string(), ctx.app_id.clone(),
        "--app-secret".to_string(), ctx.app_secret.clone(),
        "--domain".to_string(), ctx.domain.clone(),
        "--workspace".to_string(), ctx.workspace.clone(),
    ];

    let reply = format!(
        "✅ 编译成功 ({})\n正在重启…飞书连接将短暂中断后自动恢复。",
        target_dir
    );
    Ok((reply, RestartInfo {
        exe_path: exe_path.to_string_lossy().to_string(),
        args,
    }))
}

fn do_view_logs(desktop_root: &str) -> String {
    let log_dir = std::path::Path::new(desktop_root).join("runlogs");
    if !log_dir.exists() {
        return "日志目录不存在。".to_string();
    }
    // Find latest err.log and stdout log
    let mut entries: Vec<_> = fs::read_dir(&log_dir)
        .ok()
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    let mut result = String::new();
    let mut found = 0;
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if (name.ends_with(".err.log") || name.ends_with(".log")) && name.starts_with("desktop-agent-") {
            if let Ok(content) = fs::read_to_string(entry.path()) {
                let tail = last_n_lines(&content, 20);
                if !tail.trim().is_empty() {
                    result.push_str(&format!("── {} ──\n{}\n", name, tail));
                    found += 1;
                }
            }
            if found >= 2 { break; }
        }
    }
    if result.is_empty() {
        "最新日志为空。".to_string()
    } else {
        result
    }
}

fn do_git_commit(workspace: &str, message: &str) -> String {
    // git add -A
    let add = std::process::Command::new("git")
        .current_dir(workspace)
        .args(["add", "-A"])
        .output();
    if let Ok(o) = &add {
        if !o.status.success() {
            return format!("❌ git add 失败: {}", String::from_utf8_lossy(&o.stderr));
        }
    }
    // git commit
    let commit = std::process::Command::new("git")
        .current_dir(workspace)
        .args(["commit", "-m", message])
        .output();
    match commit {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                format!("✅ 已提交\n{}", last_n_lines(&out, 5))
            } else {
                format!("⚠️ git commit:\n{}\n{}", out.trim(), err.trim())
            }
        }
        Err(e) => format!("❌ 无法执行 git: {e}"),
    }
}

fn do_terminal_command(workspace: &str, command: &str) -> String {
    // Force UTF-8 output to avoid garbled Chinese characters on Windows
    let wrapped = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; {command}"
    );
    let output = std::process::Command::new("powershell")
        .current_dir(workspace)
        .args(["-NoProfile", "-Command", &wrapped])
        .output();

    match output {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            let merged = if err.trim().is_empty() {
                out.to_string()
            } else if out.trim().is_empty() {
                err.to_string()
            } else {
                format!("{out}\n{err}")
            };
            let tail = truncate_chars(&merged, 6000);
            if o.status.success() {
                format!("✅ /terminal 执行成功\n{}", tail.trim())
            } else {
                format!(
                    "❌ /terminal 执行失败 (exit={})\n{}",
                    o.status.code().unwrap_or(-1),
                    tail.trim()
                )
            }
        }
        Err(e) => format!("❌ /terminal 无法启动 shell: {e}"),
    }
}

fn is_high_risk_terminal_command(command: &str) -> bool {
    let c = command.to_lowercase();
    let risk_tokens = [
        "rm ",
        "rm -",
        "del ",
        "erase ",
        "rmdir ",
        "format ",
        "mkfs",
        "shutdown",
        "reboot",
        "stop-process",
        "taskkill",
        "sc delete",
        "reg delete",
        "git reset --hard",
        "git clean -fd",
        "remove-item",
    ];
    risk_tokens.iter().any(|token| c.contains(token))
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars().take(max_chars) {
        out.push(ch);
    }
    out.push_str("\n...(output truncated)");
    out
}

fn do_git_push(workspace: &str) -> String {
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(["push"])
        .output();
    match output {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                format!("✅ 已推送\n{}{}", out.trim(), err.trim())
            } else {
                format!("❌ git push 失败:\n{}", err.trim())
            }
        }
        Err(e) => format!("❌ 无法执行 git: {e}"),
    }
}

fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---- session formatting ------------------------------------------------

fn has_meaningful_session(session: &session_store::UserSession) -> bool {
    session.current_task.is_some()
        || !session.pending_steps.is_empty()
        || session.last_action.is_some()
        || session.last_result.is_some()
    || session.pending_terminal_command.is_some()
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
    let pending_terminal = session
        .pending_terminal_command
        .as_deref()
        .map(|s| format!("\n待确认终端命令: {s}"))
        .unwrap_or_default();
    format!("📋 状态\n模式: {mode_label}\n当前任务: {task}\n{steps_part}\n上次操作: {last}{pending_terminal}")
}

fn fmt_plan_saved(description: &str, steps: &[String]) -> String {
    if steps.is_empty() {
        format!("✅ 计划已记录\n任务: {description}\n\n发送「继续」逐步执行，或「执行全部」一键执行。")
    } else {
        let list: Vec<String> = steps.iter().enumerate()
            .map(|(i, s)| format!("  {}. {s}", i + 1))
            .collect();
        format!(
            "✅ 计划已记录（共 {} 步）\n任务: {description}\n\n{}\n\n发送「继续」逐步执行，或「执行全部」一键执行。",
            steps.len(),
            list.join("\n")
        )
    }
}

fn help_text() -> String {
    "HarborBeacon 可用命令如下:\n\
\n【🤖 Copilot 对话】\n\
/ask <问题>         直接向 Copilot 提问\n\
ask <问题>          等价于 /ask\n\
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
\n【🔧 飞书调试 (Dev-Loop)】\n\
/replace <path>      替换文件内容（多行格式见下）\n\
/write <path>        写入新文件（第二行起为内容）\n\
/terminal <cmd>      在项目目录执行终端命令\n\
/agent fix [描述]    分析上次错误并给出修复建议\n\
fix [描述]           等价于 /agent fix\n\
确认执行             执行待确认的高风险命令\n\
取消执行             取消待确认的高风险命令\n\
build | /build       编译 desktop agent\n\
restart | /restart   编译并重启当前 agent\n\
logs | /logs         查看最新运行日志\n\
commit <msg>         git add -A && commit\n\
push | /push         git push\n\
\n  /replace 格式:\n\
  /replace path/to/file\n\
  <<<\n\
  旧代码\n\
  ===\n\
  新代码\n\
  >>>\n\
\n【会话命令】\n\
/plan <任务>         记录任务（换行写步骤）\n\
/agent status        查看当前任务进展\n\
/agent <任务>        交给助手解析与执行\n\
继续                 执行下一步\n\
执行全部              一键执行所有步骤\n\
重试                 重新执行上次操作\n\
状态                 查看当前任务进展\n\
清除                 清除会话（自动归档）"
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

    #[test]
    fn parse_system_install_request_intent() {
        match parse_intent("帮我 install powershell 6+: https://aka.ms/powershell") {
            Some(Intent::SystemInstallHelp(_)) => {}
            _ => panic!("expected system install help intent"),
        }
    }
}
