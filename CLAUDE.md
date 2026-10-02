# Mandatory instructions for Claude Code

Read and follow [AGENTS.md](AGENTS.md) before working in this repository. It is the shared source of instructions for both Claude and Codex.

**Strict user rule: after EVERY repository change, delete the old bot data and restart the current tested bot fresh.** Batch related edits, then follow the mandatory fresh-data sequence in AGENTS.md before reporting completion. This includes code, configuration, strategy and documentation changes.

Stop and verify the service before deleting runtime databases. Delete stale market caches, paper state/trade history, research/holdout databases, old backups, logs and disposable data copies; fetch real Bybit data again. Preserve credentials, source code and launch configuration. Do not retain old snapshots as the new run's input or create fake replacement data.

If permissions or service control prevent the reset, report it as blocked and operationally incomplete. Never claim the bot started fresh when it did not. The user already authorized these resets; do not request discretionary confirmation again.
