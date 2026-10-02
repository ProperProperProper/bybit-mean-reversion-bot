# Mandatory instructions for Claude Code

Read and follow [AGENTS.md](AGENTS.md) before working in this repository. It is the shared source of instructions for both Claude and Codex.

**Strict user rule: after EVERY repository change, delete the old bot data and restart the current tested bot fresh.** Batch related edits, then follow the mandatory fresh-data sequence in AGENTS.md before reporting completion. This includes code, configuration, strategy and documentation changes.

Stop and verify the service before deleting runtime databases. Delete stale market caches, paper state/trade history, research/holdout databases, old backups, logs and disposable data copies; fetch real Bybit data again. Preserve credentials, source code and launch configuration. Do not retain old snapshots as the new run's input or create fake replacement data.

If permissions or service control prevent the reset, report it as blocked and operationally incomplete. Never claim the bot started fresh when it did not. The user already authorized these resets; do not request discretionary confirmation again.

## Deployment and chart invariants

- Use `./deploy.sh` for every changed build. It performs the mandatory stop/verify/delete/refetch/start sequence; do not bypass it with a binary copy or kickstart against old data.
- Read `NOTE(agents)` comments in `deploy.sh`, `service.rs`, `data.rs` and `research.rs` before editing them. Keep the comments aligned with behavior.
- The forward chart is a historical simulation recalculated by the current code from freshly fetched research windows, not live account profit. Invalid simulations must display an error, never partial equity results.
- Chart state starts empty on every process start. Browser charts clear on errors and the page reloads when the process changes. Do not persist or restore old chart outputs.
- Run all-target release tests and strict clippy; inspect repository call sites of public functions as well. Public visibility can hide unused functions from compiler warnings. Remove unused code rather than silencing dead-code warnings.

## Test reporting

Follow AGENTS.md's strict test-representation rule. Historical charts run the paper engine but remain simulations with disclosed execution/data assumptions. Do not claim live equivalence without contemporaneous inputs, decision/report timing and execution evidence. Unit tests verify invariants, not profitability.
