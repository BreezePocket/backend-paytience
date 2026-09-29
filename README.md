# paytience-backend

The PAYtience quote aggregator. Market-maker desks connect to it over WebSockets. The
dashboard talks only to its REST API. For every user RFQ it runs a best-yield auction
across all connected MMs. It merges their market data (health, assets, expiries, boards)
into one view, routes co-signing to the MM that pays the transaction fee, broadcasts the
co-signed transaction, and runs the devnet faucet.

**[`docs/protocol.md`](docs/protocol.md) is the contract** for every interface below: the
MM WebSocket protocol, the REST API and the configuration. If this README and the
contract disagree, the contract wins.

## Architecture

```
 dashboard (browser)                                   MM desk #1 (MM-system-breezepocket)
        │  HTTPS REST                                     │  WSS /ws/rfq + /ws/sign (outbound)
        ▼                                                 ▼
 ┌──────────────────────────── paytience-backend ────────────────────────────┐
 │ http.rs      router, CORS, 64 KiB bodies, rate limits (ratelimit.rs)      │
 │ ws.rs/auth   challenge → signed hello → welcome; pings; replacement      │◀── MM desk #2 …
 │ registry.rs  instances (pubkey, instance_id), eligibility, counters       │
 │ auction.rs   POST /rfq: fan out rfq_request, best yield wins, nonce memory│
 │ sign.rs      POST /sign: route to the fee payer's ws/sign, then broadcast │
 │ market/      health poller, fan-out, merge rules, cached /board /expiries │
 │ faucet.rs    POST /faucet: ATA CreateIdempotent + MintTo, SOL airdrop     │
 │ admin.rs     GET /admin/mms                                               │
 └──────────────────────────────────────┬─────────────────────────────────────┘
                                        │ JSON-RPC (solana_rpc.rs)
                                        ▼
                             Solana devnet (SOLANA_RPC)
```

- MMs dial **out** to the backend, so no desk needs a public URL.
- An MM instance is **eligible** for RFQs only while both of its sockets are
  authenticated.
- Market data asks one instance per MM pubkey, so one MM running two desks is counted
  once. RFQs still go to every eligible instance.
- The backend never builds user transactions. The dashboard builds `open_position` from
  the winning quote. The backend only reads the fee payer and the `nonce` from it, to
  route the co-sign request.

## How market makers connect

Each desk opens two sockets, `/ws/rfq` (quotes and market data) and `/ws/sign`
(co-signing). Both are authenticated the same way (contract §1.2):

1. The backend sends `challenge` with 32 random bytes as hex.
2. The desk sends `hello`. It carries `version: 1`, the channel, its `mm_pubkey`, a
   random `instance_id`, its `program_id` and `usdc_mint`. It also carries an ed25519
   signature, by the MM keypair, over `paytience-mm-auth:v1:<channel>:<challenge>`.
3. The backend checks the version, the channel, the allowlist (`MM_ALLOWLIST`), the
   signature, and that the program and mint match its own. It then answers `welcome`.
   On failure it closes with 4001 (bad hello), 4003 (not allowlisted) or 4009 (program
   or mint mismatch).

A reconnect with the same `(pubkey, instance_id, channel)` replaces the old socket
(4000). Both sides ping every 20 s, and a socket that stays silent for 60 s is closed.

## REST API

The API is a superset of a single desk's REST API, so the dashboard also works against a
raw desk. Every response carries `access-control-allow-origin: *`. Errors are
`{ "error": "…" }`, apart from the typed RFQ and sign messages.

| Method and path | What it does | Contract |
|---|---|---|
| `GET /health` | Aggregated desk health, plus `service`, `version` and `stats` (never how many market makers are connected; see `/admin/mms`) | §2.1 |
| `GET /assets` | Assets merged across fresh MMs | §2.2 |
| `GET /price?asset=` | One MM's price snapshot, from the freshest MM | §2.3 |
| `GET /expiries?asset=` | Expiries merged across MMs (cached 30 s) | §2.4 |
| `GET /board?asset=&product=&amount=&moneyness=&max_days=&exact=` | Merged quote board, best cell per strike (cached 5 s) | §2.5 |
| `POST /rfq` | Best-yield auction across eligible MMs | §2.6 |
| `POST /sign` | Co-sign through the fee-paying MM, then broadcast | §2.7 |
| `GET /faucet`, `POST /faucet` | Devnet test USDC or listed-asset mint, plus a SOL airdrop (404 when disabled) | §2.8 |
| `GET /admin/mms` | Every allowlisted MM, its instances and counters (`Authorization: Bearer <ADMIN_TOKEN>`) | §2.9 |
| `GET /ws/rfq`, `GET /ws/sign` | MM WebSockets | §1 |

Rate limits are per client IP (§2.10). The client IP is `CF-Connecting-IP` when it holds
an IP, otherwise the TCP peer. Windows are fixed one-minute windows. `POST /rfq` allows
120 per window, `POST /sign` 30, `POST /faucet` 10, and all `GET`s together 600. Over the
limit the response is `429 {"error":"rate limited"}`. WebSocket upgrades and `OPTIONS`
are not limited.

## Configuration

Set these in the environment or in a `.env` file in the working directory.
[`.env.example`](.env.example) lists every variable. Empty values count as unset.

| Variable | Default | Meaning |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP and WebSocket listen address |
| `SOLANA_RPC` | `https://api.devnet.solana.com` | JSON-RPC used for broadcasts and the faucet |
| `PROGRAM_ID` | `BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9` | breezepocket program; a desk reporting another one is refused |
| `USDC_MINT` | **required** | USDC mint in the program's GlobalConfig |
| `MM_ALLOWLIST` | **required** | `name:pubkey,name:pubkey`; a bare `pubkey` is named after its first 8 chars |
| `RFQ_TIMEOUT_MS` | `1000` | Auction window |
| `SIGN_TIMEOUT_MS` | `10000` | How long to wait for a co-signature |
| `MARKET_TIMEOUT_MS` | `2500` | Wait per MM for each `market_request` |
| `HEALTH_POLL_MS` | `5000` | `health` poll interval per instance |
| `BOARD_CACHE_MS` | `5000` | `/board` cache TTL |
| `EXPIRIES_CACHE_MS` | `30000` | `/expiries` cache TTL |
| `BROADCAST` | `true` | Whether the backend broadcasts co-signed transactions |
| `FAUCET_KEYPAIR` | empty (off) | Path to the test-mint authority keypair (Solana CLI JSON, 64-byte array) |
| `FAUCET_AMOUNT_USDC` | `1000` | USDC per claim |
| `FAUCET_ASSET_USD` | `1000` | USD worth of a listed asset per claim |
| `FAUCET_SOL_AIRDROP` | `0.5` | SOL airdropped per claim, best effort (`0` skips the airdrop) |
| `FAUCET_COOLDOWN_MS` | `3600000` | Cooldown per wallet and token |
| `ADMIN_TOKEN` | empty (off) | Enables `GET /admin/mms` |
| `RUST_LOG` | `info` | tracing filter, e.g. `paytience_backend=debug` |

The rate limits, the faucet's 30 s confirmation wait and the WebSocket liveness timings
are contract constants. They live on `Config` so tests can shrink them, but they are not
read from the environment.

## Running locally

```bash
cp .env.example .env         # then set USDC_MINT and MM_ALLOWLIST (the desk's MM pubkey)
cargo run                    # listens on 127.0.0.1:8080
curl -s localhost:8080/health
```

Point a local desk (`MM-system-breezepocket`) at the backend by adding this to the
desk's `.env`, then starting the desk:

```bash
AGGREGATOR_URL=ws://127.0.0.1:8080
```

The desk signs `hello` with its `MM_KEYPAIR`, so that pubkey must be in `MM_ALLOWLIST`.
Its `PROGRAM_ID` and `USDC_MINT` must match the backend's. The desk's own REST server
keeps serving on its `REST_PORT` (8787 by default).

Point the dashboard (`dashboard-breezepocket`) at the backend in any of these ways:

- Leave `VITE_API_URL` empty. The dashboard tries `http://localhost:8080` first.
- Set `VITE_API_URL=http://localhost:8080` in the dashboard's `.env`, then `npm run dev`.
- Open the dashboard with `?mm=http://localhost:8080`. The override is persisted.

## Testing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                   # unit tests plus tests/integration (one binary)
cargo build --release
```

The integration tests start the real server on `127.0.0.1:0`, with fake MMs that
authenticate like the desk. Solana calls go to a local mock JSON-RPC server, so the tests
never touch the network or devnet. The transaction and hello fixtures in
`tests/fixtures/` were generated with the desk's own client by
`tests/fixtures/gen-fixtures.mjs`. To regenerate them:

```bash
cd ../MM-system-breezepocket && node ../backend-paytience/tests/fixtures/gen-fixtures.mjs ../backend-paytience/tests/fixtures
```

## Deployment

The deploy guide is in [`deploy/README.md`](deploy/README.md). It covers the VM, the
systemd units, the Cloudflare tunnel, and the cross-compiled release binary.
