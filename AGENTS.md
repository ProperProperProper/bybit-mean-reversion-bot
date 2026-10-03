# Notes for agents (Codex, Claude) working on this repo

Last updated 2026-10-03 by Codex with an independent agent audit and user policy overrides. Read this, CLAUDE.md and the `NOTE(agents)` comments before changing anything.

## Mandatory fresh-data rule — user instruction, 2 October 2026

**This applies to both Codex and Claude. After EVERY change to this repository, including code, configuration, strategy parameters and documentation, delete the old bot data and start the current bot fresh. Do not reuse stale data or paper state across changes.** Batch edits belonging to one change, then perform this reset before declaring the change complete.

Required sequence:

1. Build and run the appropriate tests on the final current tree.
2. Stop the running service and verify it has stopped. If service control is denied or stopping cannot be verified, do not delete an active SQLite database. Report the reset/restart as blocked and the change as operationally incomplete.
3. Delete the old runtime market cache and its SQLite WAL/SHM files, paper-account state and trade history, research/holdout databases, old data backups, stale logs and disposable working database copies. **Do not archive or retain old data as a substitute for deletion.** Remove derived cached results that would otherwise be reused by the bot.
4. Preserve source code, agent instructions, credentials/Keychain entries, secrets, installed binaries and launch configuration. This rule never authorizes deleting credentials or unrelated files.
5. Install the current tested bot, keeping the stable signing identifier described below, and restart it in its existing paper mode. Fetch market data and rules again from Bybit; listing history and leverage eligibility must also be obtained afresh. Never seed the new run from old snapshots or invent missing data.
6. Verify the new process and newly created data. State what was deleted and whether the fresh start succeeded. Never claim a fresh start when old data remains or the process has not restarted.

The user has explicitly authorized this reset after every change; do not ask again for discretionary confirmation. Request only permissions actually required by the environment. A denied permission or service-control operation is a blocker, not permission to bypass the restriction. This instruction overrides earlier advice to retain old runtime backups or paper state across deployments. Historical figures in documentation are records of past experiments, not inputs to reuse in a new run.

Reset enforcement: `deploy.sh` now builds/tests/checks the current tree, stops launchd and verifies no bot process remains, deletes old runtime data, refetches research windows from Bybit, then installs and starts the current bot. Any stop, verification or refetch failure aborts deployment. Changes in the current batch still require this deployment before they are operationally complete.

## Notes in the code

Every place where a change could quietly break a user rule, an accounting rule or a timing rule carries a `NOTE(agents):` comment explaining the invariant and why it exists. Read them before editing a file:

```sh
grep -rn "NOTE(agents)" src examples deploy.sh
```

Keep them current: if you change the behaviour a note describes, update or remove the note in the same commit.

## What the user wants (rules, newest first)

- **Lifecycle and timing must be right before anyone relies on paper P&L or optimizer results** (2026-10-03). See "Lifecycle and timing rules" below; do not loosen them.
- **Entry wick guard:** entries and adds fill only when the traded open is within `xs::ENTRY_WICK_GUARD_PCT` (0.5%) of the mark open; otherwise they wait for the next open. Entries fill at TRADED prices (never at the mark).
- **Live, wick-safe stops:** `price_task` watches Bybit's WebSocket mark price for held coins; `XsPortfolio::live_check` locks in exits, booked by the next bar after its funding. Stops trigger on MARK price, never on last-trade wicks; candles are the fallback.
- **25% drawdown stop:** `xs::DRAWDOWN_STOP_PCT`; at 25% below the cycle peak everything exits; after a 15-minute cooldown and completed exits, valid entries are re-evaluated without waiting for the normal holding period. Global drawdown history remains intact.
- **Never halve the budget:** no half size after drawdowns, no half slot reserved for averaging down. Every slot gets its full share of the free balance.
- **Parameter search:** ONE 14-day window, never split; test EVERY parameter (`walkforward::LIVE_COMBOS` = 1,474,560, all 8 signals both ways, about 12 min). The next search starts 60 minutes after the previous one FINISHES (`search_task`), never per candle. Champion/challenger: the search's best replaces the settings in use only if it scores strictly higher; the two safety rules (no liquidation, drawdown <= 25% over the 14 days) always apply.
- **Long only.** Do not bring back shorts or market-neutral as the live mode unless the user asks.
- **Top 20 only**, by 24h turnover among eligible Bybit token USDT perpetuals with complete rules. Held coins outside the top 20 are still measured and managed, never re-entered.
- **Strict exclusion:** delisted, delisting, missing/invalid-leverage and 1x-only contracts are rejected before ranking, fetching or scoring; their stored rows are deleted every bar; a held ineligible symbol stops the bar for explicit recovery.
- **5 USDT floor:** no entries or adds while the free balance (wallet minus margin committed anywhere on the account) is below 5 USDT.
- **Real data and real code paths only.** Never fill a missing candle, fee, tier or balance with an invented value.
- No dead code; strict clippy clean. The user works unattended: don't stop to ask. After every change: tests, commit, push, `./deploy.sh` (fresh reset). Claude normally reviews Codex changes; when Claude cannot review, Codex performs and records the review before committing, pushing and deploying.
- Report results honestly, including losses. Don't tune on test windows until a number looks good.

## Lifecycle and timing rules (fixed 2026-10-03)

- **Decision latency (`xs::decision_fill_bound`):** a decision taken at bar t's close fills no earlier than bar t+2's open, in backtests, the search, the forward chart, research tools and paper alike. Live, bar t is processed after its close, so t+1's open is already gone. Each decision is stamped once, when made; re-reaching the same decision at a later close keeps its bound (re-stamping slid adds and breakers forward forever). Live, `defer_new_decisions(now, decision_bar_ts)` adjusts only decisions created at that close; existing queued bounds remain fixed.
- **Resting orders are not delayed:** intrabar stops, liquidation and live WebSocket exits act at once, as on the exchange.
- **Flattens (breaker, close-based drawdown stop)** carry their own bound (`flatten_not_before`); they never fill at an open that had passed before they were decided.
- **Volatility weights** are fixed at the decision close (`pending_weights`, keyed by symbol name) and reused by a delayed fill; never recomputed at execution.
- **Held positions stay manageable:** rules are measured for candidates plus held symbols (`data::with_held`); if fresh rules are still missing, exits, funding and liquidation use the rules measured at entry (`XsPosition::entry_rules`). New orders still require fresh rules.
- **Live exits** are only locked in by the monitor and booked when their bar is stepped, after that bar's opening funding, so the settlement ledger stays exact.

## Where things are

- Repo (this folder) → GitHub `ProperProperProper/bybit-mean-reversion-bot`, branch `main`. The repo is **public**: never commit keys or account details.
- Runtime: `~/Library/Application Support/BybitMeanReversionBot` (`data.db`, `window_1..3.db`, `paper_xs.json`, `logs/bot.err`). launchd job `com.bybitmeanreversion.bot`; console http://127.0.0.1:8787.
- Tasks: `bar_task` (15m bars, paper), `search_task` (hourly full search), `price_task` (WebSocket mark prices), `console`.
- Deploy: `./deploy.sh` builds, runs all Rust tests, clippy and `tests/test_deploy_reset.py`, stops and verifies the service, deletes runtime data, refetches research windows, installs, re-signs with identifier `bot-06c819130362ed6a` and starts. **Keep the identifier:** the LuLu allow rule matches path + signing identifier.
- LuLu blocks any *new* network binary until someone clicks Allow. Example binaries with an allow rule: `fetch_research_data`. For research, pass `EQ=<balance>` so nothing needs the network.
- Never run tools against the live SQLite files: copy with `sqlite3 SRC ".backup DEST"` first.

## Current state (2026-10-03)

- The working tree includes the additional production-path and drawdown-recovery fixes in `docs/audit.md`. Final test results and deployment status must be checked before treating the running process as updated.
- Live grid: `walkforward::live_combo(i)` for `i < LIVE_COMBOS`: every parameter (docs/walkforward.md), long only.
- Results recorded before 2026-10-03 (docs/validation.md) used next-open fills and the old split walk-forward; they are history, not validation of the current timing. Judge the current system by the dashboard's chronological forward chart and the live paper account.

## Evidence and traps

- **Survivorship / look-ahead bias:** the research windows (`window_k.db`) contain only *today's* top-20 coins. Coins are often top-turnover now *because* they just rallied, so long-only backtests on those windows look much better than reality (window 3: equal-weight +47%). Momentum-style long signals look good there and fail on the full historical universe.
- **The broad-universe test:** `cargo run --release --example signal_ic -- <dir>`, where `<dir>` holds windows with candles for every eligible token (create them with `HOLDOUT=k,... fetch_research_data`; the holdout and backup databases used on 2026-10-02 were deleted in the fresh-data reset). It ranks the top `UNIVERSE` (20) by turnover at each bar, not today's top 20, and measures each signal's rank correlation with next-open-to-close returns relative to the average coin, over non-overlapping holds.
- On that full universe, two effects held in all 3 windows at every holding period: **higher volatility → lower relative return**, and **24h losers beat 24h winners**. `CalmDip` = mean of the volatility and 24h-return percentiles combines them.
- **CalmDip passed a pre-registered holdout test** (first run included delisted tokens; rerun after the hard exclusion also passes: pooled t −5.5 / −6.0, +0.89% per 24h) (commits 5329e48 and 6988079 fixed the criteria and the definition before the data was fetched): four never-seen windows, 2026-06-25 → 08-20, full universe including delisted coins (those databases have since been deleted; the results are recorded in docs/validation.md). IC negative in every window; pooled t −5.3 (8h) and −5.8 (24h); the 5 coins bought beat the average coin by +0.85% per 24h (t 2.7). This is positive **relative** evidence, not market timing: long-only P&L still follows the market. Don't re-tune CalmDip on the holdout; it is spent as a test now. New ideas need their own fresh holdout (e.g. windows before 2026-06-25).
- Pulse momentum (buying the strongest Pulse) *lost* to the average coin in all 3 windows on the full universe, even though it looked good on the top-50 research windows used at the time (2026-10-02, before the switch to top 20).
- At small balances, lot rounding and minimum orders matter. Results are noisy, and single-coin moves dominate.

## Good next steps

1. Let the paper account run on new data; compare it with equal-weight buy-and-hold of the same universe over the same days (CalmDip's proven edge is relative to that).
2. Make the search run on the broad universe (every eligible token ranked by turnover at the time, not today's top 20), with mark candles, listing times and funding. Delisted tokens stay excluded (user rule), so some survivorship bias remains; state it with any result.
3. Test further entry ideas with `signal_ic` on the full universe *before* adding them; only signals whose sign holds in every window. New evidence needs a fresh pre-registered holdout.
4. Judge the full search by its chronological forward results, not by the best settings' in-sample 14-day result (with 1.47M combinations it is heavily selected).

## Codex review items (2026-10-02/03) and their status

- `signal_ic` freezes decision-time membership before reading future prices; a missing endpoint excludes the whole sample (`incomplete`). Conditional-on-coverage; t-statistics assume independence.
- `validate_state` rejects nonfinite or negative reserved margin; commitments above equity are valid and block entries.
- Terminal closing rejects an unpaid closing-boundary settlement; paper defers newest-boundary funding until its real mark open arrives.
- Settlement revision detection does not detect repaired historical candles or removed funding rows; do not describe those as reconciled.
- Volatility weights recomputed at execution: **fixed** 2026-10-03 (decision-time `pending_weights`).
- Strict instrument eligibility requires a finite `leverageFilter.maxLeverage > 1`; rejected before ranking, rules, scoring or fetching.
- Held symbols leaving the top 20 lost their rules and became unclosable: **fixed** 2026-10-03 (`with_held` + `entry_rules`).
- Fresh-sync regression `fresh_fourteen_day_fetch_gets_all_1344_traded_and_mark_candles` exercises the real HTTP/parser/pagination path against a local endpoint reproducing Bybit's start-only behaviour; test candles are transport fixtures, not trading evidence.

## Strict test-representation rule — user instruction

Tests must use the production engine and actual eligibility, quantity/minimum-order, free-balance, fee, funding, mark/liquidation and decision-time rules whenever claiming to represent the bot. Never use a simplified profitable surrogate or future information. State what the test measures: unit regression, historical simulation, prospective paper run or actual exchange execution.

Historical simulations are not verified live execution: current universe/books/fees/tiers applied to past candles, unverified history coverage and report latency, and candle-level intrabar ambiguity must be disclosed. Do not present simulated fills as exchange fills or historical returns as live account profit. The chart exposes its assumptions. A true live-parity claim requires replaying recorded decision/report timestamps and contemporaneous account/market inputs through the same paper-service path, with independent outcomes; actual exchange fills require exchange execution records.

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
