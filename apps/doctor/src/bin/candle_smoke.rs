use candle_core::{DType, Device, Result, Tensor};
use candle_nn::ops::softmax;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "candle_smoke")]
#[command(about = "Run a local Candle inference smoke test")]
struct Cli {
    #[arg(long, default_value = "读取文件 README.md")]
    prompt: String,
}

fn main() {
    let cli = Cli::parse();
    match run_intent_inference(&cli.prompt) {
        Ok((label, score, probs)) => {
            println!("Candle inference OK");
            println!("prompt   : {}", cli.prompt);
            println!("intent   : {}", label);
            println!("score    : {:.4}", score);
            println!("probs    : {}", probs.join(", "));
        }
        Err(e) => {
            eprintln!("Candle inference failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_intent_inference(prompt: &str) -> Result<(String, f32, Vec<String>)> {
    let device = Device::Cpu;
    let features = extract_features(prompt);

    let x = Tensor::from_slice(&features, (1, features.len()), &device)?;

    // Tiny linear classifier weights: [feature_dim, class_dim]
    // classes: read, list, search, diff, test, other
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
    )?;

    let b = Tensor::from_slice(&[0.0f32, 0.0, 0.0, 0.0, 0.0, 0.2], 6, &device)?;

    let logits = x.matmul(&w)?.broadcast_add(&b)?;
    let probs = softmax(&logits, 1)?;
    let probs = probs.to_dtype(DType::F32)?;
    let p = probs.squeeze(0)?.to_vec1::<f32>()?;

    let labels = ["read", "list", "search", "diff", "test", "other"];
    let mut best_idx = 0usize;
    let mut best_score = f32::MIN;
    for (i, v) in p.iter().enumerate() {
        if *v > best_score {
            best_score = *v;
            best_idx = i;
        }
    }

    let pairs = labels
        .iter()
        .zip(p.iter())
        .map(|(k, v)| format!("{}={:.3}", k, v))
        .collect::<Vec<_>>();

    Ok((labels[best_idx].to_string(), best_score, pairs))
}

fn extract_features(prompt: &str) -> Vec<f32> {
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
