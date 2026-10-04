# paytience-backend

The PAYtience quote aggregator. Market-maker desks connect to it over WebSockets. The
dashboard talks only to its REST API. For every user RFQ it runs a best-yield auction
across all connected MMs. It merges their market data (health, assets, expiries, boards)
into one view, routes co-signing to the MM that pays the transaction fee, broadcasts the
co-signed transaction, and runs the devnet faucet. Market makers register with it, an
admin approves them with USD caps per asset and in total, and the backend only asks an
MM for a quote while those caps have room.

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
 │ mmreg.rs     registrations and caps (MM_REGISTRY_PATH): apply, approve, … │
 │ registry.rs  live instances (pubkey, instance_id), eligibility, counters  │
 │ exposure.rs  per-MM usage: getProgramAccounts scan + reservations, caps   │
 │ auction.rs   POST /rfq: ask the MMs with room, best yield wins, nonce mem │
 │ sign.rs      POST /sign: route to the fee payer's ws/sign, then broadcast │
 │ market/      health poller, fan-out, merge rules, cached /board /expiries │
 │ faucet.rs    POST /faucet: ATA CreateIdempotent + MintTo, SOL airdrop     │
 │ admin.rs     GET /admin/mms, approve / reject / suspend / delete          │
 └──────────────────────────────────────┬─────────────────────────────────────┘
                                        │ JSON-RPC (solana_rpc.rs)
                                        ▼
                    Solana devnet (SOLANA_RPC, SOLANA_SCAN_RPC)
```

- MMs dial **out** to the backend, so no desk needs a public URL.
- An MM instance is **eligible** for RFQs only while both of its sockets are
  authenticated.
- Market data asks one instance per MM pubkey, so one MM running two desks is counted
  once. RFQs still go to every eligible instance.
- An MM only takes part in an asset it has an approved cap for: RFQs, `/board`,
  `/expiries`, `/price` and the merged `/assets` all follow that.
- The backend never builds user transactions. The dashboard builds `open_position` from
  the winning quote. The backend only reads the fee payer and the instruction's
  arguments from it, to route the co-sign request and to reserve its notional.

## How market makers connect

Each desk opens two sockets, `/ws/rfq` (quotes and market data) and `/ws/sign`
(co-signing). Both are authenticated the same way (contract §1.2):

1. The backend sends `challenge` with 32 random bytes as hex.
2. The desk sends `hello`. It carries `version: 1`, the channel, its `mm_pubkey`, a
   random `instance_id`, its `program_id` and `usdc_mint`. It also carries an ed25519
   signature, by the MM keypair, over `paytience-mm-auth:v1:<channel>:<challenge>`.
3. The backend checks the version, the channel, that the pubkey has an **approved**
   registration (below), the signature, and that the program and mint match its own. It
   then answers `welcome`. On failure it closes with 4001 (bad hello), 4003 (no approved
   registration) or 4009 (program or mint mismatch).

A reconnect with the same `(pubkey, instance_id, channel)` replaces the old socket
(4000). Both sides ping every 20 s, and a socket that stays silent for 60 s is closed.

## Registration and caps

Who may connect, and how much each market maker may carry, is held in one file
(`MM_REGISTRY_PATH`) that only the endpoints below change. There is no allowlist in the
environment any more. The contract is §5.

1. **The MM applies.** `POST /mm/apply` carries the application as a JSON string plus an
   ed25519 signature by the MM key over `paytience-mm-apply:v1:<that string>`. It names
   the MM, a contact, a total cap and a cap per asset, all in USD notional at the fixed
   price. The desk does this with `npm run register` (see its README). The record starts
   as `pending`, and `GET /mm/status?pubkey=` shows its status and approved caps.
2. **An admin approves.** With `Authorization: Bearer <ADMIN_TOKEN>`:

   ```bash
   curl -s -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms
   curl -s -X POST -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms/<pubkey>/approve
   curl -s -X POST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{"cap_total_usd":15000,"asset_caps_usd":{"SOL":10000}}' localhost:8080/admin/mms/<pubkey>/approve
   curl -s -X POST -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms/<pubkey>/suspend
   curl -s -X POST -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms/<pubkey>/reject
   curl -s -X DELETE -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms/<pubkey>
   ```

   Approve without a body takes the requested caps; a body lowers (or changes) them.
   Changes apply at once, with no restart. Reject, suspend and delete close every socket
   of that MM with 4003. Without `ADMIN_TOKEN` nobody can be approved.
3. **The caps are enforced.** The backend keeps its own view of what each approved MM
   carries: every unsettled `PositionAccount` and `AssetPosition` it is the market maker
   of (scanned with `getProgramAccounts` on `SOLANA_SCAN_RPC` every `EXPOSURE_SYNC_MS`;
   an expired position counts until it is settled), plus every transaction it co-signed
   through the backend until a scan sees it (or 2 minutes pass). An MM is asked for a
   quote only if the asset has an approved cap, and both the asset cap and the total
   cap have room for the RFQ's notional. Until an MM's first successful scan its usage
   is unknown and it is not asked.

An MM that applies again while approved files a cap change request: its approved caps
stay in force until an admin approves again.

## REST API

The API is a superset of a single desk's REST API, so the dashboard also works against a
raw desk. Every response carries `access-control-allow-origin: *`. Errors are
`{ "error": "…" }`, apart from the typed RFQ and sign messages.

| Method and path | What it does | Contract |
|---|---|---|
| `GET /health` | Aggregated desk health, the backend's view of usage and caps (`exposure`), plus `service`, `version` and `stats` (never how many market makers are connected; see `/admin/mms`) | §2.1, §5.7 |
| `GET /assets` | Assets merged across fresh MMs, each MM only for the assets it has a cap for | §2.2 |
| `GET /price?asset=` | One MM's price snapshot, from the freshest MM with a cap for the asset | §2.3 |
| `GET /expiries?asset=` | Expiries merged across the MMs with a cap for the asset (cached 30 s) | §2.4 |
| `GET /board?asset=&product=&amount=&moneyness=&max_days=&exact=` | Merged quote board, best cell per strike (cached 5 s) | §2.5 |
| `POST /rfq` | Best-yield auction across the eligible MMs whose caps have room | §2.6, §5.7 |
| `POST /sign` | Co-sign through the fee-paying MM, reserve the notional, then broadcast | §2.7, §5.6 |
| `GET /faucet`, `POST /faucet` | Devnet test USDC or listed-asset mint, plus a SOL airdrop (404 when disabled) | §2.8 |
| `POST /mm/apply` | A market maker's signed application (or cap change request) | §5.2 |
| `GET /mm/status?pubkey=` | A registration's status and approved caps | §5.3 |
| `GET /admin/mms` | Every registration with its instances, counters and usage (`Authorization: Bearer <ADMIN_TOKEN>`) | §2.9 |
| `POST /admin/mms/{pubkey}/approve` | Approve, optionally with `{ cap_total_usd?, asset_caps_usd? }` | §5.4 |
| `POST /admin/mms/{pubkey}/reject`, `…/suspend` | Stop an MM; its sockets close with 4003 | §5.4 |
| `DELETE /admin/mms/{pubkey}` | Remove a registration; its sockets close with 4003 | §5.4 |
| `GET /ws/rfq`, `GET /ws/sign` | MM WebSockets | §1 |

Rate limits are per client IP (§2.10). The client IP is `CF-Connecting-IP` when it holds
an IP, otherwise the TCP peer. Windows are fixed one-minute windows. `POST /rfq` allows
120 per window, `POST /sign` 30, `POST /faucet` and `POST /mm/apply` 10 together, and all
`GET`s together 600. WebSocket upgrades have their own budget of 60. Over the limit the
response is `429 {"error":"rate limited"}`. `OPTIONS` and the admin actions are not
limited.

## Configuration

Set these in the environment or in a `.env` file in the working directory.
[`.env.example`](.env.example) lists every variable. Empty values count as unset.

| Variable | Default | Meaning |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP and WebSocket listen address |
| `SOLANA_RPC` | `https://api.devnet.solana.com` | JSON-RPC used for broadcasts and the faucet |
| `SOLANA_SCAN_RPC` | `SOLANA_RPC` | JSON-RPC that allows `getProgramAccounts`, for the usage scan |
| `EXPOSURE_SYNC_MS` | `30000` | Usage scan interval |
| `PROGRAM_ID` | `BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9` | breezepocket program; a desk reporting another one is refused |
| `USDC_MINT` | **required** | USDC mint in the program's GlobalConfig |
| `MM_REGISTRY_PATH` | `./data/mm-registry.json` | Registrations and caps; written atomically with mode 0600, its directory is created if missing |
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
| `ADMIN_TOKEN` | empty (off) | Enables the admin API; without it nobody can be approved |
| `RUST_LOG` | `info` | tracing filter, e.g. `paytience_backend=debug` |

The rate limits, the faucet's 30 s confirmation wait and the WebSocket liveness timings
are contract constants. They live on `Config` so tests can shrink them, but they are not
read from the environment. `MM_ALLOWLIST` is no longer read.

## Running locally

```bash
cp .env.example .env         # then set USDC_MINT and an ADMIN_TOKEN
cargo run                    # listens on 127.0.0.1:8080
curl -s localhost:8080/health
```

`SOLANA_SCAN_RPC` must allow `getProgramAccounts` (the public devnet endpoint does; some
free provider tiers do not). Until a scan succeeds, no market maker is asked for quotes.

Point a local desk (`MM-system-breezepocket`) at the backend by adding this to the
desk's `.env`, then starting the desk:

```bash
AGGREGATOR_URL=ws://127.0.0.1:8080
```

The desk signs `hello` with its `MM_KEYPAIR`, and that key must be registered and
approved first. From the desk repo, then against the backend:

```bash
npm run register -- --url http://127.0.0.1:8080 --name "Local desk" --contact me@example.com \
  --total 20000 --caps SOL=10000
curl -s -X POST -H "authorization: Bearer $ADMIN_TOKEN" localhost:8080/admin/mms/<desk pubkey>/approve
```

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
never touch the network or devnet. Most tests start with an in-memory registry of two
approved MMs and no usage scan; `tests/integration/caps.rs` runs the real scan against
the mock's `getProgramAccounts`, and `tests/integration/registration.rs` covers applying
and the admin actions. The transaction and hello fixtures in `tests/fixtures/` were
generated with the desk's own client by `tests/fixtures/gen-fixtures.mjs`, and the signed
application by `tests/fixtures/gen-mm-apply.mjs` (the desk's `signApplication`). To
regenerate them:

```bash
cd ../MM-system-breezepocket && node ../backend-paytience/tests/fixtures/gen-fixtures.mjs ../backend-paytience/tests/fixtures
cd ../MM-system-breezepocket && node ../backend-paytience/tests/fixtures/gen-mm-apply.mjs ../backend-paytience/tests/fixtures
```

## Deployment

The deploy guide is in [`deploy/README.md`](deploy/README.md). It covers the VM, the
systemd units, the Cloudflare tunnel, and the cross-compiled release binary.
