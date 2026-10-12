# paperbot

Hedged-pair PAPER farm on papertrade (HyperEVM). One account, two isolated positions on the same
market in opposite directions; when price moves the target distance both close in one intent. The
loser mints PAPER on 98% of its loss, the winner returns ~85% of its gain; the haircut is the whole
cost of a cycle.

The bot holds **only a session key**. Session keys can sign OpenPosition / Close / CancelIntent and
nothing else — funding, withdrawals, staking and claims stay in the frontend with your wallet.

## Setup

```sh
cargo build --release
export PAPERBOT_STATE=~/.paperbot/state.json   # session key + nonce + in-flight cycle (0600)
export PAPERBOT_LEDGER=~/.paperbot/ledger.jsonl
```

1. Deposit USDC in the papertrade app as usual.
2. Register a session key — the master key is read from the environment once and never written:
   ```sh
   PAPERBOT_MASTER_KEY=0x… ./target/release/paperbot register          # add --dry-run to just print the body
   ```
   Registration posts one `registerSessionKey` intent signed by the master key and stores the fresh
   session key in `state.json`. Keys expire after 30 days (`--days`).
3. Check the account and the protocol:
   ```sh
   ./target/release/paperbot status
   ```

## Run

```sh
RUST_LOG=info ./target/release/paperbot run --dry-run      # whole loop, signs everything, posts nothing
RUST_LOG=info ./target/release/paperbot run                # defaults: auto market, 0.5% move, 25% buffer
```

| flag | default | meaning |
|---|---|---|
| `--market` | `auto` | `btc`, `eth`, or per-cycle switch: cheaper of (capital cost ÷ capital left) + (wait ÷ time left) |
| `--move` | `0.5` | % price move harvested per cycle; leverage = `1 / (move·(1+buffer))` |
| `--buffer-pct` | `25` | margin headroom past the move; must be ≥ `0.08% / move` so the close lands before the bust guard |
| `--capital-fraction` | `1.0` | share of balance deployed per cycle (split across the two legs) |
| `--drawdown-stop-pct` | `60` | stop when balance is down this much from the start |
| `--rate-floor` | `10` | stop when the mint rate (PAPER per $1 loss) is below this |
| `--max-cycles`, `--horizon-days` | — / `14` | hard stops |
| `--max-entry-gap-bps` | `5` | abort a pair whose two entries differ by more than this |

Ctrl-C closes any open pair, flushes the ledger and exits. If the process dies mid-cycle, the next
`run` finds the pair in `state.json`, re-reads its ids from the chain log and resumes watching it.

```sh
./target/release/paperbot close-all     # emergency: one Close intent for every open position
```

## GUI

```sh
./target/release/paperbot-gui --move 0.5 --buffer-pct 25     # same PAPERBOT_STATE / PAPERBOT_LEDGER env
```

A compact iced window in papertrade's uwu dress: protocol chips (mint rate, LP, queue, supply),
account (balance, queued, open positions, PAPER held, session expiry), the in-flight pair with a live
move-to-trigger bar, the strategy card (leverage, bust guard, BTC vs ETH keep / cost / PAPER-per-$ and
realised vol), a live mid sparkline with entry and trigger bands, and the cycle ledger. It reads the
same files and endpoints the bot does and never signs or posts anything; pass the same `--move` /
`--buffer-pct` you run the bot with so the strategy card matches.

## Telegram

```sh
export PAPERBOT_TG_TOKEN=123456:ABC…     # @BotFather token
export PAPERBOT_TG_CHAT=123456789        # your chat id (message the bot, then /getUpdates)
```

When both are set the engine pings on start, every pair open, every cycle result (pnl, PAPER minted,
running PAPER/$), aborts (naked leg, wide entry gap, a leg vanishing), the stop reason, and fatal
errors. Sends are fire-and-forget and never delay a close. The GUI's `tg` chip shows whether it's on.

## Running on a laptop

```sh
caffeinate -i ./target/release/paperbot-gui        # keeps the machine awake while the window is open
```

Starting the farm from the GUI runs the engine inside the GUI process: closing the window stops it
(open pairs are *not* closed on a hard kill — use "stop after cycle" or `close-all`). The CLI `run`
is equivalent and survives the window; don't run both against the same state file.

## What it watches

- **Price**: Hyperliquid's public `bbo` websocket — the same best bid/ask the Exchange's precompile
  reads. Trigger is `|mid / entry − 1| ≥ move`; an extra guard closes early if the move approaches
  the bust trigger (5 bps before zero equity).
- **Positions**: the Exchange's `PositionOpened` / `PositionClosed` logs over JSON-RPC, filtered by
  your address — this is where position ids and entry prices come from.
- **Balance / open count**: `/state/leaderboard/accounts/{addr}`. If the open-position count drops
  while a pair is on (liquidation), the other leg is closed at once.
- **Mint rate**: `/query/protocol/summary` → `100·(120M/(120M+tailProgress))²`.
- **Reconciliation**: `/query/accounts/{addr}/trades` after each close; one JSON line per cycle in the
  ledger with pnl, fees, PAPER minted, entries, elapsed time.

## Notes

- Minimums from `/state/trading`: $10 margin, **$10,000 notional** per position, $10M max.
- The two opens are two intents and may land in different blocks; the close of both legs is one
  intent. The entry gap is logged per cycle.
- The server can pause intake per action (`intake_action_paused`); the bot waits and retries.
- `cargo test` covers the EIP-712 signing against a viem-computed golden vector (digest, r, s, v,
  intentId) for the exact domain the live client uses.
