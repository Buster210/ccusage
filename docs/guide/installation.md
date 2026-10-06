# Installation

## Quick run without installing

```bash
# Linux/macOS direct binary in /tmp
OS=$(uname -s | tr A-Z a-z); ARCH=$(uname -m | sed -e 's/x86_64/x64/' -e 's/aarch64/arm64/'); curl -fsSL -o /tmp/ccusage "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-${OS}-${ARCH}" && chmod +x /tmp/ccusage && /tmp/ccusage daily
```

```powershell
# Windows direct binary in TEMP
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }; Invoke-WebRequest -Uri "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-win32-$arch.exe" -OutFile "$env:TEMP\ccusage.exe"; & "$env:TEMP\ccusage.exe" daily
```

## One-command install (Recommended)

Copy, paste, run. Downloads the latest release and puts `ccusage` on your `PATH`:

::: code-group

```bash [Linux / macOS]
# Asks for sudo to place the binary in /usr/local/bin
OS=$(uname -s | tr A-Z a-z); ARCH=$(uname -m | sed -e 's/x86_64/x64/' -e 's/aarch64/arm64/'); curl -fsSL -o /tmp/ccusage "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-${OS}-${ARCH}" && chmod +x /tmp/ccusage && sudo mv /tmp/ccusage /usr/local/bin/ccusage && ccusage --version
```

```powershell [Windows]
# No admin needed; installs to %LOCALAPPDATA%\ccusage and adds it to your user PATH.
# Restart your terminal afterwards so the new PATH applies everywhere.
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }; $dir = "$env:LOCALAPPDATA\ccusage"; New-Item -ItemType Directory -Force -Path $dir | Out-Null; Invoke-WebRequest -Uri "https://github.com/Buster210/ccusage/releases/latest/download/ccusage-win32-$arch.exe" -OutFile "$dir\ccusage.exe"; $env:Path = "$env:Path;$dir"; $userPath = [Environment]::GetEnvironmentVariable('Path','User'); if ($userPath -notlike "*$dir*") { [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User') }; & "$dir\ccusage.exe" --version
```

:::

## Manual install

Each release provides binaries for Linux (arm64/x64), macOS (arm64/x64), and
Windows (arm64/x64). For a permanent install, download the file for your platform
from [GitHub Releases](https://github.com/Buster210/ccusage/releases) and put
`ccusage` on your `PATH`. No Node.js, Bun, or npm required.

::: code-group

```bash [Linux / macOS]
# Download ccusage-<platform>-<arch> from GitHub Releases,
# then make it executable and move it onto your PATH
chmod +x ccusage-linux-x64
sudo mv ccusage-linux-x64 /usr/local/bin/ccusage
```

```powershell [Windows]
# Download ccusage-win32-<arch>.exe from GitHub Releases,
# rename it, move it to a directory on your PATH, then verify it
Rename-Item .\ccusage-win32-x64.exe ccusage.exe
ccusage.exe --version
```

:::

## Let your AI agent install it

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

## Development Installation

For development or contributing to ccusage:

```bash
# Clone the repository
git clone https://github.com/Buster210/ccusage.git
cd ccusage

# Allow direnv to load the Nix dev shell
direnv allow
```

The Nix dev shell provides the pinned `pnpm`, Rust toolchain, GitHub CLI, git hooks, package tooling, and project utilities. Run project tasks with `just`:

```bash
# Format the tree
just fmt

# Run tests
just test

# Run static checks
just check

# Build distribution
just build
```

You can also build the binary from source with Cargo:

```bash
cargo build --manifest-path rust/Cargo.toml --release
```

## Verification

After installation, verify ccusage is working:

```bash
# Check version
ccusage --version

# Run help command
ccusage --help

# Test with daily report
ccusage daily
```

## Updating

Re-run the one-command installer above — it replaces the binary with the
latest release. Or download the new binary manually and swap it onto your
`PATH`, then confirm:

```bash
ccusage --version
```

## Uninstalling

```bash
# Remove the binary from your PATH
sudo rm /usr/local/bin/ccusage

# Or remove the cloned repository for development installs
rm -rf ccusage/
```

## Next Steps

After installation, check out:

- [Getting Started Guide](/guide/getting-started) - Your first usage report
- [Configuration](/guide/configuration) - Customize ccusage behavior
- [Daily Usage](/guide/daily-reports) - Understand daily usage patterns
