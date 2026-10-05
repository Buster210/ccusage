# Installation

ccusage ships as a standalone binary. Download the file for your platform
from [GitHub Releases](https://github.com/ccusage/ccusage/releases) and put
`ccusage` on your `PATH`. No Node.js, Bun, or npm required.

## Standalone Binary (Recommended)

Each release provides binaries for Linux (arm64/x64), macOS (arm64/x64), and
Windows (arm64/x64):

::: code-group

```bash [Linux / macOS]
# Download ccusage-<platform>-<arch> from GitHub Releases,
# then make it executable and move it onto your PATH
chmod +x ccusage-*
sudo mv ccusage-* /usr/local/bin/ccusage
```

```powershell [Windows]
# Download ccusage-win32-<arch>.exe from GitHub Releases,
# rename it and move it somewhere on your PATH
ccusage.exe daily
```

:::

## Nix (Alternative)

If you use Nix, run ccusage without downloading anything:

```bash
nix run github:ccusage/ccusage -- daily
```

## Development Installation

For development or contributing to ccusage:

```bash
# Clone the repository
git clone https://github.com/ccusage/ccusage.git
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

Download the binary for the new release and replace the one on your
`PATH`:

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
