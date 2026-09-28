# PAYtience backend: protocol contract

This file is the single source of truth for every interface the backend exposes or
consumes. The Rust crate in this repo, the desk in `MM-system-breezepocket`, and the
dashboard in `dashboard-breezepocket` all implement against it. If code and this file
disagree, fix one of them; do not let them drift.

```
dashboard (browser) ──HTTPS REST──▶ backend (Rust, api.paytience.app) ◀──WSS── MM desk #1
                                     │  quote auction, board merge,     ◀──WSS── MM desk #2 …
                                     │  sign routing, broadcast, faucet
                                     ▼
                              Solana JSON-RPC (devnet)
```

- MMs connect **outbound** to the backend over two WebSockets each: `/ws/rfq` and
  `/ws/sign`. No MM needs a public URL.
- The dashboard only ever talks to the backend's REST API. That API is a strict
  superset of the desk's own REST API (same paths, same JSON shapes), so the dashboard
  can still point at a raw desk with `?mm=<url>`.
- Settlement is out of scope: admins post prices and settle with the scripts in `core`.
- The dashboard builds the `open_position` transaction itself from the winning quote
  (a deliberate deviation from `core/docs/specs/breezepocket.md`, which had the
  aggregator build it). The backend never builds user transactions.

Conventions:
- All frames and bodies are JSON objects. Unknown fields are ignored by receivers.
- u64 values travel as **decimal strings** (`"1500000"`); receivers also accept JSON
  integers that are safe integers. `expiry_ts` is a JSON integer (unix seconds).
- Pubkeys are base58. Transactions are base64 of the wire format.
- Times: `valid_until`, `generated_at`, `*_ms` are unix **milliseconds**.

---

## 1. MM ↔ backend WebSocket protocol

### 1.1 Channels

| Path | Purpose | Backend → MM | MM → backend |
|---|---|---|---|
| `/ws/rfq` | quotes and market data | `challenge`, `welcome`, `error`, `rfq_request`, `market_request` | `hello`, `rfq_response`, `rfq_decline`, `market_response` |
| `/ws/sign` | co-signing | `challenge`, `welcome`, `error`, `sign_request` | `hello`, `sign_response`, `sign_rejection` |

The channel name used in auth is the path without the leading slash: `ws/rfq` or
`ws/sign`.

### 1.2 Authentication (both channels)

1. On upgrade, the backend immediately sends:
   ```json
   { "type": "challenge", "challenge": "<64 lowercase hex chars = 32 random bytes>", "server_time": 1759000000000 }
   ```
2. The MM replies with `hello`:
   ```json
   {
     "type": "hello",
     "version": 1,
     "channel": "ws/rfq",
     "mm_pubkey": "<base58 MM pubkey>",
     "instance_id": "<uuid, random per desk process start>",
     "program_id": "<base58 program id the desk trades>",
     "usdc_mint": "<base58 USDC mint the desk trades>",
     "signature": "<base64 ed25519 signature>"
   }
   ```
   `signature` is the ed25519 signature, by the MM keypair, over the UTF-8 bytes of
   ```
   paytience-mm-auth:v1:<channel>:<challenge>
   ```
   e.g. `paytience-mm-auth:v1:ws/rfq:9f86d0…`.
3. The backend verifies, in order: `version == 1`; `channel` equals the path that was
   dialled; `mm_pubkey` is on the allowlist; the signature verifies; `program_id` and
   `usdc_mint` equal the backend's own. On success it sends
   ```json
   { "type": "welcome", "channel": "ws/rfq", "mm_pubkey": "…", "name": "<allowlist display name>", "instance_id": "…" }
   ```
   On failure it sends `{ "type": "error", "reason": "<why>" }` and closes with code
   **4001** (bad hello / signature / version / channel), **4003** (not allowlisted), or
   **4009** (program or mint mismatch).
4. A connection that has not sent a valid `hello` within **10 s** is closed (4001).
   Frames other than `hello` before authentication are ignored.

Connection identity is `(mm_pubkey, instance_id, channel)`. A new authenticated
connection with the same identity replaces the old one (the old socket is closed with
code **4000** "replaced"). Different `instance_id`s of one pubkey coexist.

An **instance is eligible for RFQs** only while both its `ws/rfq` and `ws/sign`
connections are authenticated.

### 1.3 Liveness

- The backend sends a WebSocket ping every **20 s** on every connection and closes any
  connection that has sent no frame (including pong) for **60 s**.
- The desk also pings every 20 s (existing behaviour); both sides answer pings.
- On close, every request pending on that connection fails immediately
  ("market maker disconnected").
- The desk reconnects each channel independently with exponential backoff (1 s → 30 s).

### 1.4 `ws/rfq`: quotes

The backend forwards the user's RFQ with **its own** `rfq_id` (`agg-<uuid>`), so ids
never collide between users:

```json
{ "type": "rfq_request", "rfq_id": "agg-…", "asset": "SOL", "product": "sell_sol",
  "fixed_price": "250000000", "expiry_ts": 1759824000, "amount": "500000000",
  "user_pubkey": "…", "nonce": "1234567890123" }
```
`asset` is omitted for SOL (matches the dashboard). `token` is omitted (desk derives it).

MM answers, matched on `rfq_id`:
```json
{ "type": "rfq_response", "rfq_id": "agg-…", "quote_id": "…", "yield_amount": "1234567",
  "yield_token": "usdc", "yield_pct": 1.23, "apr_pct": 45.6, "mm_pubkey": "…",
  "valid_until": 1759000030000, "price_source": "deribit_mark", "instrument": "SOL_USDC-…",
  "index_price": 200.1, "forward_price": 200.3, "implied_vol": 0.71, "fee_pct": 0 }
```
or
```json
{ "type": "rfq_decline", "rfq_id": "agg-…", "reason": "expiry too close" }
```

The backend discards (and logs) an `rfq_response` whose `mm_pubkey` differs from the
connection's authenticated pubkey, whose `yield_amount` is not a u64 > 0, or whose
`valid_until` is not a number. `valid_until` is passed through unchanged (MM clock).

### 1.5 `ws/rfq`: market data

```json
{ "type": "market_request", "request_id": "mkt-<uuid>", "method": "board",
  "params": { "asset": "SOL", "product": "sell_sol", "amount": 1, "moneyness": [1.05, 1.1], "max_days": 90, "exact": false } }
```
| `method` | `params` | desk handler | `result` |
|---|---|---|---|
| `health` | `{}` | `desk.health()` | desk `/health` body |
| `assets` | `{}` | `desk.assets()` | `{ "assets": [...] }` |
| `price` | `{ asset? }` | `desk.priceSnapshot(asset)` | desk `/price` body |
| `expiries` | `{ asset? }` | `desk.expiries(asset)` | `{ "expiries": [...] }` |
| `board` | `{ asset?, product?, amount?, moneyness?, max_days?, exact? }` | `desk.board({...})` | desk `/board` body |

Omitted params mean the desk's defaults (exactly as the desk's REST query string).
`moneyness` is an array of numbers; `max_days` maps to the desk's `maxDays`.

Reply:
```json
{ "type": "market_response", "request_id": "mkt-…", "ok": true, "result": { … } }
{ "type": "market_response", "request_id": "mkt-…", "ok": false, "error": "SOL reference price is stale", "status": 400 }
```
`status` is the HTTP status the desk would have used (its `err.statusCode`, else 400).

### 1.6 `ws/sign`: co-signing

The backend forwards with its own `request_id` (`sig-<uuid>`):
```json
{ "type": "sign_request", "request_id": "sig-…", "tx_base64": "…" }
```
MM answers, matched on `request_id`:
```json
{ "type": "sign_response", "request_id": "sig-…", "tx_base64": "…", "signature": "<optional, if the desk broadcast>" }
{ "type": "sign_rejection", "request_id": "sig-…", "reason": "quote expired" }
```

---

## 2. Public REST API (dashboard → backend)

All responses carry `access-control-allow-origin: *`,
`access-control-allow-headers: content-type, authorization`,
`access-control-allow-methods: GET, POST, OPTIONS`; `OPTIONS` returns 204. Request
bodies are limited to 64 KiB. Errors that are not typed messages are
`{ "error": "<message>" }`.

The backend is **online** while at least one eligible MM instance (§1.2) exists.
"Fresh" MM data means a `health` snapshot received within the last 30 s.

### 2.1 `GET /health`

Same shape as the desk's `/health`, aggregated, plus backend fields:

```json
{
  "ok": true,
  "service": "paytience-backend",
  "version": "0.1.0",
  "mm_pubkey": "<pubkey of an online MM, or \"\" when none>",
  "mm_count": 2,
  "program_id": "<backend PROGRAM_ID>",
  "usdc_mint": "<backend USDC_MINT>",
  "dry_run": false,
  "price": { "source": "aggregate", "spot": 200.12, "spotAgeMs": 1200, "vol": 0.71, "expiries": 12 },
  "assets": [ /* merged, §2.2 */ ],
  "exposure": {
    "totalUsd": 1234.5, "capTotalUsd": 40000, "capPerExpiryUsd": 10000,
    "perExpiry": { "1759824000": { "onChainUsd": 1000, "reservedUsd": 234.5 } }
  },
  "stats": { "rfqs": 10, "quoted": 8, "declined": 1, "timed_out": 1, "signs": 5, "sign_rejections": 1, "sign_timeouts": 0, "broadcasts": 5 }
}
```

Aggregation over **fresh, eligible** MMs:
- `ok`: at least one such MM whose own `health.ok` is true.
- `mm_count`: number of distinct eligible MM pubkeys.
- `mm_pubkey`: kept for older dashboards (their `resolveDesk` requires a string).
- `dry_run`: true only when every such MM reports `dry_run: true` (nothing can co-sign).
- `price`: `spot` = median of the MMs' `price.spot`; `spotAgeMs` = min; `vol` = median;
  `expiries` = max; `source` = `"aggregate"`.
- `exposure`: `totalUsd`, `capTotalUsd`, `capPerExpiryUsd` are sums;
  `perExpiry[ts].onChainUsd` / `.reservedUsd` are sums per key.
- When no MM is fresh: `ok: false`, `mm_count: 0`, `price: { "source": "aggregate", "spot": null, … }`,
  `assets: []`, `exposure` all zero with `perExpiry: {}`.

MM identities beyond `mm_pubkey` are **not** listed publicly (spec: don't expose MM
identity); see `GET /admin/mms`.

### 2.2 `GET /assets`

`{ "assets": [...] }`, merged across fresh eligible MMs by case-insensitive `asset`:

| field | rule |
|---|---|
| `asset` | casing from the first MM that reports it |
| `underlying`, `venue`, `synthetic` | from a tradable entry if any, else the first |
| `tradable` | any MM reports it tradable |
| `mint`, `decimals`, `expiry_time_of_day` | from a tradable entry (null if none) |
| `fresh` | any MM's entry is fresh |
| `spot`, `atm_vol` | median of non-null values from fresh entries (all entries if none fresh) |
| `expiries` | max of non-null |
| `mm_count` | (new) number of MMs pricing it |

Order: SOL first, then first-seen order.

### 2.3 `GET /price?asset=SOL`

Forwarded as `market_request{method:"price"}` to one fresh eligible MM (the one with the
freshest snapshot) and returned verbatim. 503 `{error}` when no MM is online.

### 2.4 `GET /expiries?asset=SOL`

`{ "expiries": [...] }`: `market_request{method:"expiries"}` to every eligible MM
(timeout 2.5 s each), merged by `expiry_ts`:
`days` from the first; `forward_price` median; `atm_vol` median of non-null (else null);
`strikes` sorted ascending union (dedup exact values). Sorted by `expiry_ts`.
Cached 30 s per asset (case-insensitive key), with request coalescing.
If no MM is online: **503** `{error:"no market maker is online"}`. If every asked MM
errored or timed out: `{error}` with the most common MM error message (a timeout counts
as "market maker did not respond in time"), and the status every MM error shared, else
**503**. Error results are not cached.

### 2.5 `GET /board?asset=&product=&amount=&moneyness=&max_days=&exact=`

Same query parameters as the desk. The backend fans out `market_request{method:"board"}`
(timeout 2.5 s each) and merges:
- Top level from the first successful board: `asset`, `underlying`, `venue`,
  `tradable` (OR across boards), `mint`, `decimals`, `product`, `token`, `yield_token`,
  `amount`. `index_price` = median. `generated_at` = max. `fee_pct` = min across
  contributing MMs. `mm_pubkey` = the MM contributing the most cells (ties: first).
  New: `mm_count` = number of boards merged.
- `expiries`: rows merged by `expiry_ts` (`days` first seen, `forward_price` median,
  `atm_vol` median of non-null). Cells merged by `fixed_price` (exact equality); for
  each strike keep the cell with the largest `yield_amount` (u64 compare), ties by
  larger `apr_pct`, then first seen. Each kept cell gains `mm_pubkey` and `fee_pct` of
  the MM that produced it. Cells within a row are sorted by distance from the merged
  `index_price` (nearest first). Rows sorted by `expiry_ts`.
- Cached 5 s per normalized query (params sorted, `asset` lowercased, `moneyness`
  normalized), with request coalescing: concurrent identical requests share one fan-out.
- Errors as in §2.4.

### 2.6 `POST /rfq`

Request (same as the desk; the dashboard's `rfq_id` is echoed back):
```json
{ "type": "rfq_request", "rfq_id": "web-…", "asset": "SOL", "product": "buy_sol",
  "fixed_price": "180000000", "expiry_ts": 1759824000, "amount": "100000000",
  "user_pubkey": "…", "nonce": "…" }
```
Backend validation (else **400** `{ "type": "rfq_decline", "rfq_id": "…", "reason": "invalid rfq: <why>" }`):
`product` ∈ {`sell_sol`,`buy_sol`}; `fixed_price`, `amount` u64 > 0; `nonce` u64;
`expiry_ts` positive integer; `user_pubkey` valid base58 32-byte key; `rfq_id`
optional (generated if missing). Asset/expiry rules are the MMs' job.

Auction:
1. Asked set = every eligible MM instance (§1.2). None → **503**
   `rfq_decline` "no market maker is online".
2. Send `rfq_request` (backend `rfq_id`) to each; collect until all have answered or
   `RFQ_TIMEOUT_MS` (default 1000) elapses.
3. Winner = largest `yield_amount` among valid responses; ties → earliest received.
4. Response **200**: the winner's `rfq_response` with `rfq_id` replaced by the client's,
   plus `mm_name` (allowlist name), `quotes_received` (valid quotes), `mms_asked`.
5. No quotes but ≥1 decline → **200** `rfq_decline` with `reason`: the single distinct
   reason, or `"no market maker quoted: <r1>; <r2>"` (distinct, in arrival order).
6. No answers at all → **504** `rfq_decline` "no market maker responded in time".

The backend remembers the winner by `nonce` for 10 minutes:
`nonce → { mm_pubkey, instance_id, quote_id, valid_until }`.

### 2.7 `POST /sign`

Request: `{ "type": "sign_request", "request_id": "web-sign-…", "tx_base64": "…" }`.

1. Decode base64 → legacy Solana transaction (a versioned message, first message byte
   ≥ 0x80, is rejected). Fee payer = `account_keys[0]`. Find the instruction whose
   program is `PROGRAM_ID` and whose data starts with the `open_position`
   (`[135,128,47,77,15,152,240,49]`) or `open_asset_position`
   (`[120,227,20,71,252,225,87,70]`) discriminator; `nonce` = u64 LE at data bytes
   41..49 (after discriminator 8 + product 1 + fixed_price 8 + expiry_ts 8 + amount 8 +
   yield_amount 8). Any failure → **400** `sign_rejection` "invalid transaction: <why>".
2. Route: if the remembered winner for `nonce` has `mm_pubkey == fee payer` and that
   instance's `ws/sign` is connected → that instance. Otherwise any connected `ws/sign`
   of the fee payer pubkey (most recently authenticated first). None → **422**
   `sign_rejection` "market maker <first 8 chars> is not connected".
3. Forward `sign_request` (backend `request_id`); wait `SIGN_TIMEOUT_MS` (default
   10000). Timeout → **504** `sign_rejection` "market maker did not respond in time".
4. `sign_rejection` from the MM → **422**, reason passed through.
5. `sign_response` from the MM:
   - If it already has `signature`, return it (**200**).
   - Else if `BROADCAST` (default true): `sendTransaction` (base64, preflight on,
     `preflightCommitment: confirmed`) on `SOLANA_RPC`.
     - Success → **200** with `signature` set.
     - JSON-RPC error response (e.g. simulation failed) → **422** `sign_rejection`
       "broadcast failed: <rpc message>".
     - Transport failure / timeout / non-JSON → **200** without `signature` (the
       dashboard then broadcasts itself, as it does today).
   - Else **200** without `signature`.
   All typed responses echo the **client's** `request_id`.

### 2.8 `GET /faucet`, `POST /faucet`

Enabled only when `FAUCET_KEYPAIR` is set (else both return **404** `{error:"faucet disabled"}`).

`GET` → `{ "enabled": true, "amount_usdc": 1000, "asset_usd": 1000, "cooldown_ms": 3600000, "usdc_mint": "…" }`

`POST { "pubkey": "…", "asset"?: "TSLAon" }`:
- `asset` absent or `USDC` (case-insensitive) → mint `FAUCET_AMOUNT_USDC` × 10^6 of
  `USDC_MINT`.
- Otherwise the merged asset (§2.2) must be `tradable` with a `mint` and `decimals`
  (else **400** "<asset> is not listed on chain") and have a spot > 0 (else **503**
  "<asset> has no price yet; try again shortly"). Amount = `FAUCET_ASSET_USD / spot`
  rounded to 4 significant figures; base units = round(amount × 10^decimals).
- Cooldown per `(wallet, SYMBOL)` of `FAUCET_COOLDOWN_MS` → **429**
  "faucet cooldown: try again in N min". The slot is taken before sending and released
  if the mint fails.
- Transaction (fee payer and signer: the faucet authority): SPL ATA program
  `CreateIdempotent` (data `[1]`; accounts payer(s,w), ata(w), owner, mint,
  system program, token program) then SPL Token `MintTo` (data `[7] + u64 LE`;
  accounts mint(w), ata(w), authority(s)). Legacy token program
  `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`, ATA program
  `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`. Sent with `sendTransaction`, then
  polled with `getSignatureStatuses` until `confirmed` (≤ 30 s). RPC failure → **502**.
- Then best-effort `requestAirdrop(owner, FAUCET_SOL_AIRDROP × 10^9)`; failure is not
  an error.
- **200** `{ "pubkey", "asset": "USDC"|"<SYMBOL>", "amount", "usdc_amount", "signature", "usdc_signature", "sol_airdrop_signature": "…"|null }`
  (`usdc_amount` is `amount` for USDC else 0; `usdc_signature` = `signature`).

### 2.9 `GET /admin/mms`

Requires `Authorization: Bearer <ADMIN_TOKEN>`; **404** when `ADMIN_TOKEN` is unset,
**401** on a wrong token. Lists every allowlisted MM: `pubkey`, `name`, and per
instance `instance_id`, `rfq_connected`, `sign_connected`, `eligible`,
`connected_since_ms`, `last_health_ms`, `health_ok`, `dry_run`; plus per-pubkey
counters `rfqs_asked`, `quotes`, `declines`, `timeouts`, `wins`, `signs`,
`sign_rejections`, `sign_timeouts`.

### 2.10 Rate limits

Per client IP (`CF-Connecting-IP` header if present, else the peer address), fixed
one-minute windows: `POST /rfq` 120, `POST /sign` 30, `POST /faucet` 10, all `GET`s
600. Over the limit → **429** `{error:"rate limited"}`.

---

## 3. Backend configuration (env)

| var | default | meaning |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP + WS listen address |
| `SOLANA_RPC` | `https://api.devnet.solana.com` | JSON-RPC for broadcast + faucet |
| `PROGRAM_ID` | `BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9` | breezepocket program |
| `USDC_MINT` | **required** | USDC mint in the program's GlobalConfig |
| `MM_ALLOWLIST` | **required** | `name:pubkey,name:pubkey` (name optional: `pubkey` alone uses its first 8 chars) |
| `RFQ_TIMEOUT_MS` | 1000 | auction window |
| `SIGN_TIMEOUT_MS` | 10000 | co-sign wait |
| `MARKET_TIMEOUT_MS` | 2500 | per-MM market_request wait |
| `HEALTH_POLL_MS` | 5000 | per-instance `health` poll interval |
| `BOARD_CACHE_MS` | 5000 | board cache TTL |
| `EXPIRIES_CACHE_MS` | 30000 | expiries cache TTL |
| `BROADCAST` | true | backend broadcasts co-signed transactions |
| `FAUCET_KEYPAIR` | empty (off) | path to the test-mint authority keypair JSON |
| `FAUCET_AMOUNT_USDC` | 1000 | |
| `FAUCET_ASSET_USD` | 1000 | |
| `FAUCET_SOL_AIRDROP` | 0.5 | SOL |
| `FAUCET_COOLDOWN_MS` | 3600000 | |
| `ADMIN_TOKEN` | empty (off) | enables `GET /admin/mms` |
| `RUST_LOG` | `info` | tracing filter |

## 4. Desk configuration (MM-system-breezepocket)

| var | meaning |
|---|---|
| `AGGREGATOR_URL` | backend base URL: `ws://127.0.0.1:8080`, `wss://api.paytience.app`; `http(s)://` is accepted and converted to `ws(s)://` |

The desk signs `hello` with `MM_KEYPAIR` and reports its `PROGRAM_ID` and `USDC_MINT`.
Its own REST server keeps running for local testing.
