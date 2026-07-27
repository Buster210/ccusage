use std::path::Path;

use crate::{cli::SharedArgs, debug_log};

/// Runs `parse` and returns its entries, or logs `"Failed to read {kind} {file}: {error}"`
/// and returns an empty `Vec` on failure. Shared by the JSONL adapters whose
/// per-file parse step differs only in `kind` and the parse call itself.
pub fn parse_or_log<T>(
    file: &Path,
    shared: &SharedArgs,
    kind: &str,
    parse: impl FnOnce() -> crate::Result<Vec<T>>,
) -> Vec<T> {
    parse().unwrap_or_else(|error| {
        debug_log(
            shared,
            format!("Failed to read {kind} {}: {error}", file.display()),
        );
        Vec::new()
    })
}
