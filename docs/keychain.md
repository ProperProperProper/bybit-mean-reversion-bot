# `src/engine/keychain.rs`: read-only API credentials

Status (2 October 2026): credentials sign read-only GETs only and are never written or logged.

| Item | Meaning |
|---|---|
| `SERVICE`, `ACCOUNT` | The macOS Keychain generic-password item to read (`unified-combo-grid` / `live`) |
| `struct Credentials` | `api_key` and `api_secret`, held in memory only |

## `pub fn load() -> Result<Credentials>`

Runs `/usr/bin/security find-generic-password -s unified-combo-grid -a live -w`. It expects the item to contain JSON `{"api_key": "...", "api_secret": "..."}` and fails clearly if the item is missing, isn't JSON, or has an empty field.

**Use:** the credentials sign two **read-only** requests: the account's fee rates, and its USDT wallet with the margin already committed to other positions and orders. The bot never places orders.

**Safety:** credentials are never read from or written to files, never logged, and never part of the repository. To set them up, store your own key pair in the Keychain item above. Give the key read-only permissions on Bybit.
