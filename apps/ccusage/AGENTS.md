# AGENTS.md - ccusage Package

The local `ccusage` source package. The CLI itself is Rust, in `../../rust/crates/ccusage`; this
package carries the release version source (`package.json`), the `src/cli.js` dev launcher that
spawns the native binary, `config-schema.json`, and the staging and benchmark scripts in `scripts/`.

The package ships the canonical native CLI, so its public surface is the `ccusage` command, its
agent subcommands, and stable `--json` output - not library-style TypeScript exports. New runtime
behavior goes into the Rust crates unless the work is scoped to this local layer.
