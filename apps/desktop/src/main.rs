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
                    let result = execute_plan_step(bridge, &step);
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
（也支持自然语言：如“读取文件 README.md”）\n\
\n【会话命令】\n\
plan <任务>          记录任务（换行写步骤）\n\
继续                 执行下一步\n\
重试                 重新执行上次操作\n\
状态                 查看当前任务进展\n\
清除                 清除会话\n\
切换编码/切换规划     切换工作模式"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{auto_plan_from_request, infer_action_intent_candle, Intent};

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
}
