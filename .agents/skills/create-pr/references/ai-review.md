# AI Review

This fork has no review bots. The maintainer is the reviewer: re-read the
full diff before calling a PR ready, checking behavior, tests, docs impact,
and commit atomicity.

Reply to human reviewer threads with `gh` — the reply and thread-state calls
live in `references/gh-review.md`. For every actionable item, apply the
smallest fix that keeps repo conventions, run the relevant checks, commit and
push through the `commit` skill, then reply in that thread stating what
changed and which validation passed.

Poll comments, reviews, and inline threads before calling the PR ready — see
`gh-review.md`, including the GraphQL query for thread resolution state.
Classify each item as actionable, a question, a false positive, or
informational.
