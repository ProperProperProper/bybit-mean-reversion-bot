# Notes for agents (Codex, Claude) working on this repo

Last updated 2026-10-02 by Claude Code. Read this before changing anything.

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
- **Top 50 Bybit token USDT perpetuals by 24h turnover that have complete Bybit rules** (margin tiers, account fee, lot filter, measured book). The top 75 are measured; coins without data are dropped.
- **Delisted tokens are hard-excluded** (user request): never fetched or entered, held ones exit at the next open (`NextAction::Delisting`), and `Cache::retain_symbols` deletes their stored rows every bar and in every research/holdout DB. Do not reintroduce delisted symbols anywhere, research included.
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
- The walk-forward of the full strategy (relative edge + market exposure + costs) passes in 1 of 4 data sets; forward tests −9.8% and +21.9%. The signal's relative edge is proven out of sample (above); whether the long-only *account* makes money also depends on the market. See `docs/validation.md`.

## Evidence and traps

- **Survivorship / look-ahead bias:** the research windows (`window_k.db`) contain only *today's* top-50 coins. Coins are often top-turnover now *because* they just rallied, so long-only backtests on those windows look much better than reality (window 3: equal-weight +47%). Momentum-style long signals look good there and fail on the full historical universe.
- **The broad-universe test:** `cargo run --release --example signal_ic -- <dir>`, where `<dir>` holds windows with candles for every trading token (`holdout/` for Jun 25 → Aug 20; `backup-20261002-0029/` for windows 1–3, purged of delisted tokens). It ranks the top 50 by turnover at each bar, not today's top 50, and measures each signal's rank correlation with next-open-to-close returns relative to the average coin, over non-overlapping holds.
- On that full universe, two effects held in all 3 windows at every holding period: **higher volatility → lower relative return**, and **24h losers beat 24h winners**. `CalmDip` = mean of the volatility and 24h-return percentiles combines them.
- **CalmDip passed a pre-registered holdout test** (first run included delisted tokens; rerun after the hard exclusion also passes: pooled t −5.5 / −6.0, +0.89% per 24h) (commits 5329e48 and 6988079 fixed the criteria and the definition before the data was fetched): four never-seen windows, 2026-06-25 → 08-20, full universe including delisted coins (`runtime/holdout/window_-3..0.db`). IC negative in every window; pooled t −5.3 (8h) and −5.8 (24h); the 5 coins bought beat the average coin by +0.85% per 24h (t 2.7). This is a proven **relative** edge, not market timing: long-only P&L still follows the market. Don't re-tune CalmDip on the holdout; it is spent as a test now. New ideas need their own fresh holdout (e.g. windows before 2026-06-25).
- Pulse momentum (buying the strongest Pulse) *lost* to the average coin in all 3 windows on the full universe, even though it looks good on today's top 50.
- At ~110 USDT, slots are 11–40 USDT, so lot rounding and minimum orders matter. Results are noisy, and single-coin moves dominate.

## Good next steps

1. Let the paper account run on new data; compare it with equal-weight buy-and-hold of the same universe over the same days (the edge is relative to that).
2. Build an unbiased backtest: fetch mark candles, listing times (instruments-info with `status` filters for delisted coins) and funding for the full historical universe, so the walk-forward itself runs without survivorship bias.
3. Test further entry ideas with `signal_ic` on the full universe *before* adding them to a grid. Only add signals whose sign holds in every window.
4. Exits: a 24h hold is a blunt exit. Test take-profit / time-stop variants with `examples/risk_variants.rs` once the entry is settled.
