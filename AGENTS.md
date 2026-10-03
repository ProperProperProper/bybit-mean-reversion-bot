# Notes for agents (Codex, Claude) working on this repo

Last updated 2026-10-02 by Codex and Claude Code. Read this before changing anything.

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

Every place where a change could quietly break a requirement or an accounting rule carries a `NOTE(agents):` comment explaining the invariant and why it exists. Read them before editing a file:

```sh
grep -rn "NOTE(agents)" src examples deploy.sh
```

Keep them current: if you change the behaviour a note describes, update or remove the note in the same commit.

## What the user wants

- **Long only.** The live strategy is long-only. Do not bring back shorts or the market-neutral mode as the live strategy unless the user asks.
- **Improve entry signals**, judged on real data and on data the signal never saw.
- **Real data and real code paths only.** Never fill a missing candle, fee, tier or balance with an invented or default value. A missing input means "don't trade" or an explicit error.
- **Top 20 only**, by 24h turnover among eligible Bybit token USDT perpetuals. Measure only those 20; incomplete rules reduce the universe rather than scanning extra candidates. Historical top-50 results below describe the former configuration, not validation of the new top-20 setup.
- **Delisted tokens are hard-excluded** (user request): never fetched or entered, excluded held symbols stop paper with an explicit recovery error rather than fetch excluded contracts or fabricate exits, and `Cache::retain_symbols` deletes their stored rows every bar and in every research/holdout DB. Do not reintroduce delisted symbols anywhere, research included.
- **5 USDT floor:** no entries or adds while the free balance (wallet minus margin committed anywhere on the account) is below 5 USDT.
- No dead code. Strict clippy clean. The user works unattended: don't stop to ask, redeploy when needed, commit and push when done.
- Report results honestly, including losses. Don't tune on the test windows until a number looks good.

## Where things are

- Repo (this folder) → GitHub `ProperProperProper/bybit-mean-reversion-bot`, branch `main`. The repo is **public**: never commit keys, balances or account details.
- Runtime: `~/Library/Application Support/BybitMeanReversionBot` (`data.db`, `window_1..3.db`, `paper_xs.json`, `logs/bot.err`). launchd job `com.bybitmeanreversion.bot`; console http://127.0.0.1:8787.
- Deploy: `./deploy.sh` (builds, runs every test, installs, restarts). It re-signs the binary with identifier `bot-06c819130362ed6a`. **Keep that:** the LuLu firewall allow rule matches path + signing identifier, and a new identifier makes the bot's connections hang on a firewall prompt.
- LuLu also blocks any *new* binary that touches the network until someone clicks Allow. Example binaries with existing allow rules: `fetch_research_data`, `risk_variants`. For research, pass `EQ=<balance>` so nothing needs the network.
- Never run tools against the live SQLite files: copy with `sqlite3 SRC ".backup DEST"` first.

## Current state (2026-10-02)

- All findings in `docs/audit.md` are fixed with regression tests (mark-price funding/liquidation, isolated margin, neutral fills, lot rounding, risk-tier deductions, new listings, balance floor, and more).
- Live grid: `walkforward::live_grid()` = long-only `Signal::CalmDip` (unflipped), hold 8h or 24h, 3 or 5 coins, 1× or 2×, stop none or 10%, BTC trend filter off or on (32 combos).
- The walk-forward of the full strategy (relative edge + market exposure + costs) passes in 1 of 4 data sets; forward tests −9.8% and +21.9%. The signal has positive relative evidence on the tested sample; whether the long-only *account* makes money also depends on the market. See `docs/validation.md`.

## Evidence and traps

- **Survivorship / look-ahead bias:** the research windows (`window_k.db`) contain only *today's* top-20 coins. Coins are often top-turnover now *because* they just rallied, so long-only backtests on those windows look much better than reality (window 3: equal-weight +47%). Momentum-style long signals look good there and fail on the full historical universe.
- **The broad-universe test:** `cargo run --release --example signal_ic -- <dir>`, where `<dir>` holds windows with candles for every eligible token (create them with `HOLDOUT=k,... fetch_research_data`; the holdout and backup databases used on 2026-10-02 were deleted in the fresh-data reset). It ranks the top `UNIVERSE` (20) by turnover at each bar, not today's top 20, and measures each signal's rank correlation with next-open-to-close returns relative to the average coin, over non-overlapping holds.
- On that full universe, two effects held in all 3 windows at every holding period: **higher volatility → lower relative return**, and **24h losers beat 24h winners**. `CalmDip` = mean of the volatility and 24h-return percentiles combines them.
- **CalmDip passed a pre-registered holdout test** (first run included delisted tokens; rerun after the hard exclusion also passes: pooled t −5.5 / −6.0, +0.89% per 24h) (commits 5329e48 and 6988079 fixed the criteria and the definition before the data was fetched): four never-seen windows, 2026-06-25 → 08-20, full universe including delisted coins (those databases have since been deleted; the results are recorded in docs/validation.md). IC negative in every window; pooled t −5.3 (8h) and −5.8 (24h); the 5 coins bought beat the average coin by +0.85% per 24h (t 2.7). This is positive **relative** evidence, not market timing: long-only P&L still follows the market. Don't re-tune CalmDip on the holdout; it is spent as a test now. New ideas need their own fresh holdout (e.g. windows before 2026-06-25).
- Pulse momentum (buying the strongest Pulse) *lost* to the average coin in all 3 windows on the full universe, even though it looked good on the top-50 research windows used at the time (2026-10-02, before the switch to top 20).
- At small balances, lot rounding and minimum orders matter. Results are noisy, and single-coin moves dominate.

## Good next steps

1. Let the paper account run on new data; compare it with equal-weight buy-and-hold of the same universe over the same days (the edge is relative to that).
2. Make the walk-forward itself run on the broad universe (every eligible token ranked by turnover at the time, not today's top 20), with mark candles, listing times and funding. Delisted tokens stay excluded (user rule), so some survivorship bias remains; state it with any result.
3. Test further entry ideas with `signal_ic` on the full universe *before* adding them to a grid. Only add signals whose sign holds in every window.
4. Exits: a 24h hold is a blunt exit. Test take-profit / time-stop variants with `examples/risk_variants.rs` once the entry is settled.

## Follow-up code review (2026-10-02, Codex)

- `signal_ic` now freezes decision-time membership before reading future prices. A missing endpoint excludes the whole sample and increments `incomplete`; it never replaces a missing coin with a survivor. This is still conditional-on-coverage analysis, not an unbiased missing-outcome estimator.
- A rerun on backups of the purged holdout found zero incomplete samples; 24h relative excess remains +0.89%, pooled IC t −6.0 (8h −5.5). Delisted exclusion retains survivorship bias. Non-overlap does not imply independent samples; reported t-statistics assume independence.
- `validate_state` rejects nonfinite or negative reserved margin. Commitments above equity remain valid and block entries.
- Terminal closing rejects an unpaid funding settlement at the closing timestamp. Paper still defers newest-boundary funding until its real mark open arrives.
- Settlement revision detection does not establish detection of repaired historical candles or removed funding rows. Do not describe those as fully reconciled.
- Volatility-scaled orders delayed by missing candles recompute weights at execution; the live grid leaves volatility scaling disabled. Do not enable it before storing decision-time weights.

- Strict instrument eligibility also requires a finite Bybit `leverageFilter.maxLeverage > 1`. Missing, malformed and 1x-only contracts are rejected before ranking, rule requests, scoring or fetching candles, in live and research discovery.
- Held symbols outside the top 20 remain managed only while still instrument-eligible. Excluded held symbols are purged and require explicit recovery; no excluded-symbol candle requests are made.

## Strict test-representation rule — user instruction

Tests must use the production engine and actual eligibility, quantity/minimum-order, free-balance, fee, funding, mark/liquidation and decision-time rules whenever claiming to represent the bot. Never use a simplified profitable surrogate or future information. State what the test measures: unit regression, historical simulation, prospective paper run or actual exchange execution.

Historical simulations are not verified live execution: current universe/books/fees/tiers applied to past candles, unverified history coverage and report latency, and candle-level intrabar ambiguity must be disclosed. Do not present simulated fills as exchange fills or historical returns as live account profit. The chart exposes its assumptions. A true live-parity claim requires replaying recorded decision/report timestamps and contemporaneous account/market inputs through the same paper-service path, with independent outcomes; actual exchange fills require exchange execution records.

- Fresh-sync regression: `fresh_fourteen_day_fetch_gets_all_1344_traded_and_mark_candles` exercises the actual HTTP/parser/pagination path against a local endpoint reproducing Bybit's start-only first-1000 behavior. Both traded `klines_since` and mark `mark_range` must return all 1,344 timestamps on the first attempt, with `end` on both pages. Removing `end` was checked to fail at 1,000 versus 1,344 candles. Test candles are transport fixtures, not trading evidence.
