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

## Mandatory peer awareness and review — user instruction

- Codex, Claude and oMLX must read `bybit_inspector.peer_activity` and `task_board` before work, and publish planned/started/progress/completed/blocked actions using `report_action`. Before a conflicting edit, claim the code-change lane. Report edits, test results and deployment status without credentials, account amounts, raw prompts or logs. Unreported actions are not automatically visible; never claim another agent has read a notice without evidence.
- The shared model router automatically supplies peer metadata to bounded oMLX/Claude calls and records their start/outcome. Do not bypass this route with an uninstrumented model call for this workflow.
- **Every change, including follow-up edits after approval, requires a fresh actual Claude review before pushing or restarting the bot; when Claude is unavailable or capacity-limited, actual Codex review can approve instead (user override 2026-10-03).** Approval is bound to the exact source/instruction/config/test fingerprint. No automatic approval, no oMLX approval, no reusing approval after an edit. Record the actual reviewer and review findings.
- Request review through the shared review gate. Rejection, invalid review response, missing approval or changed files blocks push/deployment. Actual Claude unavailability permits a documented Codex review of the same fingerprint; it does not approve automatically. Address findings and request another review. `deploy.sh` checks the gate before destructive reset and restart; the repository's pre-push hook checks before pushing. Never use `--no-verify` or bypass a required gate.

## Shared vetted learning memory — user instruction

- At task startup, retrieve `bybit_inspector.learning_memory` alongside peer activity. Codex, Claude CLI and routed oMLX use the same persistent store outside disposable bot data. Current repository instructions and current source take precedence over older lessons.
- After a verified outcome, use `submit_lesson` with a concise reusable lesson, reproducible evidence and the relevant source digest. No credentials, account amounts, raw prompts or private logs. Model suggestions alone are not verified outcomes.
- Use `review_lesson` for actual Claude subscription vetting; if Claude is unavailable, actual Codex review can approve the exact lesson through the fallback receipt. Pending/rejected/superseded records must not enter retrieved working memory. A reviewer assesses supplied evidence; independently run tests and inspect source before applying a lesson.
- Shared retrieval is contextual learning for hosted Codex/Claude, not modification of their model weights. Local LoRA training is a separate evaluated process; never claim training, improved accuracy or serving until those steps actually succeed.
- Learning occurs through recorded task outcomes. External actions and idle sessions are not automatically observed. Reconnect clients after MCP/router updates.

## Codex fallback approval — user override (2026-10-03)

If actual Claude review cannot run because the provider is unavailable or capacity-limited, Codex may perform the review instead. This applies to exact-source push/restart approval and vetted lesson approval. Record the actual reviewer, exact fingerprint and concrete findings. Fallback is not automatic approval. Rejection findings still require fixes, and every later edit invalidates source approval. The local receipt is a cooperative record, not proof that a model ran; never issue a receipt without doing the review.

## Drawdown recovery — user override (2026-10-03)

Keep the 25% drawdown exit, but do not permanently disable trading. Re-arm after a 15-minute cooldown only once stopped positions have fully closed, current settings qualify, and free balance is at least 5 USDT. Force a new entry decision rather than waiting for a long rebalance interval; preserve causal execution delay. With timely complete data the bot must be eligible to resume within an hour. Missing funding/prices, incomplete exits or entry-rule failures must display their blocking reason, never invent data or force an invalid order. Preserve global drawdown metrics across cycles; only the cycle risk baseline resets.
