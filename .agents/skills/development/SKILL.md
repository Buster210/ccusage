---
name: development
description: Guides ccusage monorepo development. Use when editing workspace packages, the npm launcher or native packaging, dependencies, or shared configuration, and when running the `just` build, typecheck, test, format, or check recipes.
---

# ccusage Development

Root `AGENTS.md` holds the repository shape and the standing policies. This
skill is about working in the tree: the npm packaging seam, the recipes, and
validation.

## The Release Staging Seam

`apps/ccusage` is the local source package around the Rust binary — the
release version in `package.json`, `config-schema.json`, and the staging and
benchmark scripts. Distribution is GitHub Releases only: CI builds the Rust
binary per platform, `stage-native-package.nu` fills a per-platform staging
directory, and `release.yaml` collects the raw binaries from the staging
tarballs. There is no npm publishing surface.

The Nushell scripts in `apps/ccusage/scripts/` own the binary side and share
`native-binary.nu`: `stage-native-package.nu` fills one platform staging
directory for CI. A staging change usually touches that script, the build
actions under `.github/actions`, and `release.yaml` together.

## Gotchas

- `.claude/skills` is generated from `.agents/skills` by `nix/agent-skills.nix`.
  Edit the source tree; leave the generated one uncommitted.
- `LOG_LEVEL` gates runtime noise (`rust/crates/ccusage-core/src/logger.rs`):
  `0` suppresses progress and box titles, `>= 4` logs pricing refresh detail.
  Use `LOG_LEVEL=0` whenever output is captured or compared.

`references/commands.md` covers `just`, where a new dependency or tool belongs,
validation, and releases.
