# `examples/`: research tools

Status (2 October 2026): results from these tools on the corrected engine are in [validation.md](validation.md).

The original three tools are read-only towards Bybit. They need the Keychain credentials ([keychain.md](keychain.md)) for fee rates and the real balance.

## `fetch_research_data.rs`: `main()`

Builds the research data:

1. Prints the real account balance.
2. Lists today's top 50 token USDT perpetuals by 24h turnover (never delisted ones) and stores their launch times and the universe.
3. Measures their Bybit rules now (fails rather than storing an incomplete snapshot).
4. For each of the three 14-day windows ([research.md](research.md)), fetches any missing traded and mark candles and the settled funding into `window_k.db` (6 coins at a time, paced), recording a new listing's first trade.

Any symbol still missing a candle after its listing start fails the run.

## `research.rs`: `main()`

Signal research, from the real balance (or `EQ=…`).

1. Loads the three windows and joins them for forward tests.
2. Runs each of the seven signal families, and all of them together, through the **14-day walk-forward in every window**. It prints the verdict, out-of-sample net, profit factor, trades and liquidations.
3. Runs **two 14-day forward tests**: settings chosen at the end of window k trade window k+1, at real and at 2x costs.

Set `EQ=` to the observed balance to run without network access. A family has to pass in all three windows and in both forward tests to count; none currently does.

## `risk_variants.rs`: `main()`

Drawdown rules compared on the same five checks.

- **The list:** a fixed set of nine rules plus a 1x leverage reference, decided before any result was seen.
- **Fair comparison:** each rule is applied on its own to the live family's grid, and the walk-forward picks the other settings exactly as live.
- **Output, per rule:** both forward tests (return, return at 2x costs, max drawdown, profit factor, leverage) and the three walk-forwards' out-of-sample returns.

The current paper grid uses no optional drawdown rule.

## `validate_cached.rs`: offline current-grid validation

`cargo run --release --example validate_cached -- SNAPSHOT_DIR OBSERVED_EQUITY output.json`

Loads `data.db` plus `window_1.db` through `window_3.db` from a disposable snapshot. The equity argument must be finite and positive and should come from an observed account value. It performs four current-grid walk-forwards (latest cache plus three research windows) and two chronological forward tests at normal and doubled fee/book costs.

The JSON records data coverage, parameters, reports, returns, costs, drawdown and liquidations. `coverage` counts leading/internal/trailing absent candles without claiming their cause. `outcome` aggregates simulated metrics/fees/funding. The tool makes no API, Keychain or order call. It opens the copies in SQLite WAL mode; it should not run against the live service directory.
