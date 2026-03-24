use clap::Parser;
use feishu_provider::{check_connectivity, ConnectivityReport};

#[derive(Debug, Parser)]
#[command(name = "harborbeacon-desktop-doctor")]
#[command(about = "Diagnose Feishu connectivity for HarborBeacon Desktop")]
struct Cli {
    #[arg(long, help = "Feishu app_id")]
    app_id: String,

    #[arg(long, help = "Feishu app_secret")]
    app_secret: String,

    #[arg(long, default_value = "https://open.feishu.cn", help = "Feishu Open API domain")]
    domain: String,

    #[arg(long, help = "Emit machine-readable JSON")]
    json: bool,
}

fn main() {
    let cli = Cli::parse();

    match check_connectivity(&cli.app_id, &cli.app_secret, &cli.domain) {
        Ok(report) => emit_report(&report, cli.json),
        Err(error) => {
            if cli.json {
                let payload = serde_json::json!({
                    "ok": false,
                    "error": error.to_string(),
                });
                println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{\"ok\":false}".to_string()));
            } else {
                eprintln!("doctor failed: {error}");
            }
            std::process::exit(1);
        }
    }
}

fn emit_report(report: &ConnectivityReport, as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .unwrap_or_else(|_| "{\"ok\":false}".to_string())
        );
        return;
    }

    println!("=== HarborBeacon Desktop Doctor ===");
    println!("Domain        : {}", report.domain);
    println!("Token         : {}", if report.token_ok { "OK" } else { "FAIL" });
    println!("Bot Info      : {}", if report.bot_info_ok { "OK" } else { "DEGRADED" });
    println!("WS Endpoint   : {}", if report.ws_endpoint_ok { "OK" } else { "DEGRADED" });
    if let Some(bot_name) = &report.bot_name {
        println!("Bot Name      : {bot_name}");
    }
    if let Some(app_name) = &report.app_name {
        println!("App Name      : {app_name}");
    }
    if let Some(service_id) = &report.ws_service_id {
        println!("WS Service ID : {service_id}");
    }
    if let Some(endpoint) = &report.ws_endpoint {
        println!("WS Endpoint   : {endpoint}");
    }
    if !report.warnings.is_empty() {
        println!("Warnings:");
        for warning in &report.warnings {
            println!("- {warning}");
        }
    }
    println!();
    println!("Overall       : {}", if report.ok { "PASS" } else { "FAIL" });
}
