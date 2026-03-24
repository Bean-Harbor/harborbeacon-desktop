# HarborBeacon Desktop

HarborBeacon Desktop is the standalone desktop-host project for remote assistant workflows on Windows and macOS.

Recommended standalone repository name: `harborbeacon-desktop`

This workspace currently provides:

- a Rust workspace skeleton for the desktop-host architecture
- a `doctor` CLI for Feishu credential and connectivity diagnostics
- foundational crates for future Feishu, runtime, and VS Code bridge work

## Layout

- `apps/desktop`: future desktop host entrypoint
- `apps/doctor`: Feishu diagnostics CLI
- `apps/mcp-server`: future local MCP server entrypoint
- `crates/core-contracts`: shared types and contracts
- `crates/feishu-provider`: Feishu API and transport helpers
- `crates/router-runtime`: future planning and routing runtime
- `crates/vscode-bridge`: future workspace bridge layer

## Quick Start

Run the doctor CLI from this workspace root:

```powershell
cargo run -p harborbeacon-desktop-doctor -- --app-id <APP_ID> --app-secret <APP_SECRET>
```

Use `--json` for machine-readable output.

## Standalone Repository

This folder is intended to become its own Git repository and GitHub project.

See `REPO_BOOTSTRAP.md` for the suggested repository split, remote naming, and first-push steps.
