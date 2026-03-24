# Repository Bootstrap

This directory is ready to be split into its own standalone repository.

## Recommended Repository Name

- GitHub repository: `harborbeacon-desktop`
- Local directory: `harborbeacon-desktop`

If you want a broader branding later, alternatives include:

- `harborclaw-desktop`
- `harborbeacon-agent-desktop`

## Recommended Scope

Move only the new desktop-host workspace into the standalone repository:

- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `.gitignore`
- `REPO_BOOTSTRAP.md`
- `apps/`
- `crates/`

Do not move HarborOS-specific code from the parent repository unless it is explicitly being promoted into a shared Rust crate.

## Local Git Initialization

From this directory:

```powershell
git init -b main
git add .
git commit -m "Initial HarborBeacon Desktop workspace"
```

## Create the GitHub Repository

Create a new empty GitHub repository named `harborbeacon-desktop`.

Do not initialize it with a README, `.gitignore`, or license if you want a clean first push from this local repository.

## First Push

After creating the remote repository:

```powershell
git remote add origin <YOUR_GITHUB_REPO_URL>
git push -u origin main
```

Example remote URLs:

- `https://github.com/<owner>/harborbeacon-desktop.git`
- `git@github.com:<owner>/harborbeacon-desktop.git`

## Suggested Next Technical Milestones

1. Extend `doctor` with proxy diagnostics and richer endpoint validation.
2. Implement Feishu long-connection receive loop in `crates/feishu-provider`.
3. Add a minimal local MCP server in `apps/mcp-server`.
4. Add a first safe VS Code bridge action set in `crates/vscode-bridge`.

## Migration Note

Until the split is pushed to its own remote, this folder still lives inside the parent repository. That is acceptable for local development, but long-term ownership, issue tracking, and release automation should move to the standalone repository.