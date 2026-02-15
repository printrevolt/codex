# Extension points (PrintRevolt fork)

This fork’s design goal is to keep upstream changes *small and explicit* by:

- Isolating PrintRevolt code in `codex-rs/crates/pr_*`
- Touching only a small set of upstream call-sites that invoke `codex-pr-runtime`
- Enforcing a CI “diff budget” so upstream sync remains tractable

## Lifecycle touchpoints (Milestone A)

The upstream runtime invokes PrintRevolt at these points:

- `on_session_start`: after `SessionConfigured` is emitted and after the file watcher is started
- `before_task`: immediately before spawning a new regular task for a user turn
- `before_finalize`: immediately before emitting `TurnComplete`
- `on_session_end`: during shutdown after unified-exec processes are terminated

The implementation entrypoint is `codex-pr-runtime` in `codex-rs/crates/pr_runtime`.

## Current upstream call-sites

As of this fork baseline:

- Session init: `codex-rs/core/src/codex.rs` (`Session::new`)
- Turn spawn: `codex-rs/core/src/codex.rs` (`handlers::user_input_or_turn`)
- Turn completion: `codex-rs/core/src/tasks/mod.rs` (`Session::on_task_finished`)
- Session shutdown: `codex-rs/core/src/codex.rs` (`handlers::shutdown`)

If you need a new extension point, prefer adding it as a new method on
`codex_pr_runtime::PrRuntime` and invoking it from the smallest possible
upstream call-site.

## Diff budget (contract)

CI enforces that changes under `codex-rs/` are mostly isolated to
`codex-rs/crates/pr_*`. The number of files touched outside that directory is
bounded (see `.github/workflows/printrevolt-diff-budget.yml`).

