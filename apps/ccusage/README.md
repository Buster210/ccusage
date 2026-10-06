<div align="center">
    <img src="https://cdn.jsdelivr.net/gh/Buster210/ccusage@main/docs/public/logo.svg" alt="ccusage logo" width="256" height="256">
    <h1>ccusage</h1>
</div>

<div align="center">
    <img src="https://cdn.jsdelivr.net/gh/Buster210/ccusage@main/docs/public/screenshot.png" alt="ccusage terminal report screenshot">
</div>

> Analyze coding (agent) CLI token usage and costs from local data.

> Forked from [ccusage/ccusage](https://github.com/ccusage/ccusage) — maintained by [Buster210](https://github.com/Buster210).

## Quick Start

Run without installing:

```bash
# Linux/macOS direct binary in /tmp
OS=$(uname -s | tr A-Z a-z); ARCH=$(uname -m | sed -e 's/x86_64/x64/' -e 's/aarch64/arm64/'); curl -fsSL -o /tmp/ccusage "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-${OS}-${ARCH}" && chmod +x /tmp/ccusage && /tmp/ccusage daily
```

```powershell
# Windows direct binary in TEMP
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }; Invoke-WebRequest -Uri "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-win32-$arch.exe" -OutFile "$env:TEMP\ccusage.exe"; & "$env:TEMP\ccusage.exe" daily
```

See [Installation](#installation) for permanent installs and PATH setup.

## Supported Sources

ccusage reads local usage data from coding agent CLIs and turns it into daily, weekly, monthly, and session reports.

| Source             | Focused command example     |
| ------------------ | --------------------------- |
| Claude Code        | `ccusage claude daily`      |
| Codex              | `ccusage codex daily`       |
| OpenCode           | `ccusage opencode daily`    |
| Amp                | `ccusage amp daily`         |
| Droid              | `ccusage droid daily`       |
| Codebuff           | `ccusage codebuff daily`    |
| Hermes Agent       | `ccusage hermes daily`      |
| pi                 | `ccusage pi daily`          |
| Goose              | `ccusage goose daily`       |
| OpenClaw           | `ccusage openclaw daily`    |
| Kilo               | `ccusage kilo daily`        |
| Kimi               | `ccusage kimi daily`        |
| Qwen               | `ccusage qwen daily`        |
| GitHub Copilot CLI | `ccusage copilot daily`     |
| Gemini CLI         | `ccusage gemini daily`      |
| Antigravity        | `ccusage antigravity daily` |
| Grok Build CLI     | `ccusage grok daily`        |
| ZCode              | `ccusage zcode daily`       |

Use `ccusage daily`, `ccusage weekly`, `ccusage monthly`, or `ccusage session` to include every detected source in one report.

## Installation

### One-command install (Recommended)

Copy, paste, run. Downloads the latest release and puts `ccusage` on your `PATH`:

```bash
# Linux / macOS (asks for sudo to place the binary in /usr/local/bin)
OS=$(uname -s | tr A-Z a-z); ARCH=$(uname -m | sed -e 's/x86_64/x64/' -e 's/aarch64/arm64/'); curl -fsSL -o /tmp/ccusage "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-${OS}-${ARCH}" && chmod +x /tmp/ccusage && sudo mv /tmp/ccusage /usr/local/bin/ccusage && ccusage --version
```

```powershell
# Windows (no admin needed; installs to %LOCALAPPDATA%\ccusage and adds it to your user PATH)
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }; $dir = "$env:LOCALAPPDATA\ccusage"; New-Item -ItemType Directory -Force -Path $dir | Out-Null; Invoke-WebRequest -Uri "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-win32-$arch.exe" -OutFile "$dir\ccusage.exe"; $env:Path = "$env:Path;$dir"; $userPath = [Environment]::GetEnvironmentVariable('Path','User'); if ($userPath -notlike "*$dir*") { [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User') }; & "$dir\ccusage.exe" --version
```

Restart your terminal afterwards on Windows so the new `PATH` applies everywhere, then run `ccusage daily`.

### Manual install

Each release ships direct prebuilt binaries for Linux (arm64/x64), macOS (arm64/x64), and Windows (arm64/x64). Download the file matching your platform from
[GitHub Releases](https://github.com/Buster210/ccusage/releases), make it executable where applicable, and put `ccusage` on your `PATH` — no archive extraction and no Node.js, Bun, or npm required:

- Linux: `ccusage-linux-arm64`, `ccusage-linux-x64`
- macOS: `ccusage-darwin-arm64`, `ccusage-darwin-x64`
- Windows: `ccusage-win32-arm64.exe`, `ccusage-win32-x64.exe`

```bash
# Linux / macOS example
chmod +x ccusage-linux-x64
sudo mv ccusage-linux-x64 /usr/local/bin/ccusage
```

On Windows, download the matching `.exe` file and place it in a directory on your `PATH`.

### Let your AI agent install it

Rather run nothing yourself? Paste this prompt to your coding agent and let it do the install while showing its work:

```text
Install ccusage for me without me running install commands blindly.

Rules:
1. Only use releases from github.com/Buster210/ccusage (the maintained fork). Never use upstream ccusage/ccusage or any other source.
2. Detect my OS and CPU architecture, then download the matching asset from that repo's latest GitHub release: ccusage-linux-<arch>, ccusage-darwin-<arch>, or ccusage-win32-<arch>.exe.
3. Before installing, show me the exact download URL, confirm the file is a real executable binary (not an HTML error page), and show its --version output.
4. Install it to a user-writable location on my PATH (Unix: ~/.local/bin preferred; /usr/local/bin only if I approve sudo. Windows: %LOCALAPPDATA%\ccusage plus my user PATH). No sudo, no shell-startup edits, no system settings changes without asking first.
5. Verify with `ccusage --version` and `ccusage --help`, then report every step you took and how to undo it (which file to delete).
```

## Usage

```bash
# Basic usage
ccusage          # Show all detected sources by day (default)
ccusage daily    # All detected sources by day
ccusage weekly   # All detected sources by week
ccusage monthly  # All detected sources by month
ccusage session  # All detected sources by session
ccusage blocks   # Claude Code 5-hour billing windows
ccusage statusline  # Claude Code status line for hooks (Beta)
ccusage clear-cache      # Clear the on-disk cache

# Source-focused reports and options
ccusage claude daily --mode display
ccusage codex daily --speed fast
ccusage opencode weekly
ccusage amp session
ccusage droid daily
ccusage codebuff daily
ccusage hermes daily
ccusage goose daily
ccusage openclaw daily
ccusage kilo daily
ccusage kimi daily
ccusage qwen daily
ccusage copilot daily
ccusage gemini daily
ccusage antigravity daily
ccusage grok daily
ccusage zcode daily
ccusage pi daily --pi-path /path/to/sessions
ccusage pi daily --pi-path /path/to/sessions,/archive/pi/sessions

# Explicit unified report
ccusage daily --all
ccusage daily --sections daily,monthly,session --json
ccusage daily --by-agent --json

# Filters and options
ccusage daily --since 2026-04-25 --until 2026-05-16
ccusage daily --last 1  # Today
ccusage weekly --last 1  # This week
ccusage monthly --last 1  # This month
ccusage daily --json  # JSON output
ccusage daily --no-cost  # Hide cost columns and JSON cost fields
ccusage daily --timezone UTC  # Use UTC timezone

# Project analysis
ccusage claude daily --instances  # Group Claude Code by project/instance
ccusage claude daily --project myproject  # Filter to specific Claude project
ccusage claude daily --instances --project myproject --json  # Combined usage

# Compact mode for screenshots/sharing
ccusage --compact  # Force compact table mode
ccusage monthly --compact  # Compact monthly report
```

## Features

- 📊 **Daily Report**: View token usage and costs aggregated by date
- 📅 **Monthly Report**: View token usage and costs aggregated by month
- 💬 **Session Report**: View usage grouped by conversation sessions
- 🤖 **Unified CLI Reports**: View Claude Code, Codex, OpenCode, Amp, Droid, Codebuff, Hermes Agent, pi, Goose, OpenClaw, Kilo, Kimi, Qwen, GitHub Copilot CLI, Gemini CLI, Antigravity, Grok Build CLI, and ZCode usage from one CLI
- ⏰ **5-Hour Blocks Report**: Track usage within Claude's billing windows with active block monitoring
- 🚀 **Statusline Integration**: Compact usage display for Claude Code status bar hooks (Beta)
- 🤖 **Model Tracking**: See which models are used across supported sources
- 📊 **Model Breakdown**: View per-model cost breakdown with `--breakdown` flag
- 📅 **Date Filtering**: Filter reports by date range using `--since` and `--until`
- ⏱️ **Recent Periods**: Jump to today, this week, or this month with `--last 1` on any daily, weekly, or monthly report
- 📁 **Custom Paths**: Support for custom local data directory locations
- 🎨 **Beautiful Output**: Colorful table-formatted display with automatic responsive layout
- 📱 **Smart Tables**: Automatic compact mode for narrow terminals (< 100 characters) with essential columns
- 📸 **Compact Mode**: Use `--compact` flag to force compact table layout, perfect for screenshots and sharing
- 📋 **Enhanced Model Display**: Model names shown one per line for better readability
- 📄 **JSON Output**: Export data in structured JSON format with `--json`
- 💰 **Cost Tracking**: Shows costs in USD for each day/month/session
- 🔒 **Cost Hiding**: Remove cost columns and JSON cost fields with `--no-cost`
- 🔄 **Cache Token Support**: Tracks and displays cache creation and cache read tokens separately
- 🌐 **Offline Mode**: Use pre-cached pricing data without network connectivity with `--offline`
- 📊 **Live Only Mode**: Show only usage from log files still on disk; exclude spend retained from deleted source logs with `--live-only`
- 🧩 **Custom Pricing Overrides**: Override token pricing per raw model name in `ccusage.json` without rebuilding
- 🏗️ **Claude Instance Support**: Group Claude Code usage by project with `--instances` and filter by specific projects
- 🌍 **Timezone Support**: Configure timezone for date grouping with `--timezone` option
- ⚙️ **Configuration Files**: Set defaults with JSON configuration files, complete with IDE autocomplete and validation
- 🧹 **Cache Management**: Clear the on-disk cache with `clear-cache` for a clean rebuild

## Development

<details>
<summary>Contributor setup</summary>

Contributor setup uses the Nix flake development environment with [nix-direnv](https://github.com/nix-community/nix-direnv) for pinned tools, and `just` for everyday development tasks. Install [Nix](https://nixos.org/) with the `nix-command` and `flakes` experimental features enabled, then let nix-direnv load the dev shell automatically when you enter the directory:

```sh
# Clone the repository
git clone https://github.com/Buster210/ccusage.git
cd ccusage

# Allow direnv to load the Nix dev shell
direnv allow
```

The dev shell provides the pinned `pnpm`, Rust toolchain, GitHub CLI, git hooks, generated local agent skills, package tooling, and project utilities from `flake.nix`. Run `pnpm install --frozen-lockfile` only when a task needs workspace `node_modules`.

Run project tasks with `just` from inside the Nix environment (`just --list` shows every recipe):

```sh
just fmt
just test
just check
```

### Nix Package

The flake exposes `ccusage` as the default package and app:

```sh
nix run github:Buster210/ccusage
nix run github:Buster210/ccusage -- codex daily --offline
nix build github:Buster210/ccusage
```

Nix builds embed the LiteLLM pricing file from the locked `litellm` flake input, so sandboxed builds do not fetch pricing at build time. To update the locked pricing snapshot:

Non-Nix Cargo builds read the same locked LiteLLM revision from `flake.lock` and fetch the pricing file from that revision at build time.

```bash
just update-litellm-pricing
```

The scheduled `update pricing` workflow runs the same update and validation, then opens a PR when the pricing snapshot changes.

</details>

## License

[MIT](LICENSE). Fork maintained by [Buster210](https://github.com/Buster210). (this fork)
