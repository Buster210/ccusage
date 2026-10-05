# Installation

ccusage ships as a standalone binary. Download the archive for your platform
from [GitHub Releases](https://github.com/ccusage/ccusage/releases), extract
it, and put `ccusage` on your `PATH`. No Node.js, Bun, or npm required.

## Standalone Binary (Recommended)

Each release provides archives for Linux (arm64/x64), macOS (arm64/x64), and
Windows (arm64/x64):

::: code-group

```bash [Linux / macOS]
# Download ccusage-<version>-<platform>-<arch>.tar.gz from GitHub Releases,
# then extract it and move the binary onto your PATH
tar -xzf ccusage-*.tar.gz
chmod +x ccusage
sudo mv ccusage /usr/local/bin/
```

```powershell [Windows]
# Download ccusage-<version>-win32-<arch>.zip from GitHub Releases,
# extract it, and move ccusage.exe somewhere on your PATH
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

Download the archive for the new release and replace the binary on your
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
