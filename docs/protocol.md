# PAYtience backend: protocol contract

This file is the single source of truth for every interface the backend exposes or
consumes. The Rust crate in this repo, the desk in `MM-system-breezepocket`, and the
dashboard in `dashboard-breezepocket` all implement against it. If code and this file
disagree, fix one of them; do not let them drift.

```
dashboard (browser) ──HTTPS REST──▶ backend (Rust, api.paytience.app) ◀──WSS── MM desk #1
                                     │  quote auction, board merge,     ◀──WSS── MM desk #2 …
                                     │  sign routing, broadcast, faucet,
                                     │  MM registration and caps
                                     ▼
                              Solana JSON-RPC (devnet)
```

- MMs connect **outbound** to the backend over two WebSockets each: `/ws/rfq` and
  `/ws/sign`. No MM needs a public URL. An MM registers first and connects once an
  admin has approved it (§5).
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
   dialled; `mm_pubkey` has a registration whose status is `approved` (§5.5); the
   signature verifies; `program_id` and `usdc_mint` equal the backend's own. On success
   it sends
   ```json
   { "type": "welcome", "channel": "ws/rfq", "mm_pubkey": "…", "name": "<the registration's name>", "instance_id": "…" }
   ```
   On failure it sends `{ "type": "error", "reason": "<why>" }` and closes with code
   **4001** (bad hello / signature / version / channel), **4003** (no approved
   registration), or **4009** (program or mint mismatch). **4003** is also the code
   every socket of an MM is closed with when an admin rejects, suspends or deletes it
   (§5.4).
4. A connection that has not sent a valid `hello` within **10 s** is closed (4001).
   Frames other than `hello` before authentication are ignored, except that a text or
   binary frame larger than **16 KiB** is refused without being parsed: `error`
   "hello too large", close **4001**.

Connection identity is `(mm_pubkey, instance_id, channel)`. A new authenticated
connection with the same identity replaces the old one (the old socket is closed with
code **4000** "replaced"). Different `instance_id`s of one pubkey coexist.

An **instance is eligible for RFQs** only while both its `ws/rfq` and `ws/sign`
connections are authenticated.

Connection limits (both channels):
- Every message (and frame) is capped at **1 MiB**, before and after authentication.
  The largest legitimate one is a `board` `market_response`: about 16 KB for SOL
  (4 cells × ≤ 15 expiries) and under 100 KB even for an asset with daily expiries out
  to 90 days, so the cap leaves ≥ 10× headroom. A larger message closes the
  connection; after authentication that is a disconnect (§1.3).
- At most **256** sockets may be upgraded but not yet authenticated at once, and at
  most **8** per client IP (the client as §2.10 identifies it). Past either cap the
  upgrade is answered **503** `{error}` without upgrading. A socket stops counting as
  soon as its `hello` is verified (or rejected), or it closes (including the 10 s
  timeout).
- Upgrade attempts are rate limited per client IP (§2.10): **429** `{error}`.
- A desk on the backend's own machine (a loopback peer that sends no
  `CF-Connecting-IP`) is exempt from both the caps above and the upgrade rate limit, so
  a flood through the public tunnel can never keep the house desk from reconnecting.
  cloudflared also connects from loopback but always sets that header, so public
  traffic never qualifies.

### 1.3 Liveness

- The backend sends a WebSocket ping every **20 s** on every connection and closes any
  connection that has sent no frame (including pong) for **60 s**, with code
  **4008** "idle timeout".
- On shutdown the backend closes every MM connection with **1001** "server shutting
  down".
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
"Fresh" MM data means a `health` snapshot received within the last 30 s (the backend
polls `health` on every `ws/rfq` connection every `HEALTH_POLL_MS`, first right after
it authenticates).

**Live over dry-run.** An eligible instance is **dry-run** when its fresh `health`
reports `dry_run: true`: it quotes but refuses to co-sign. Every other eligible
instance is **live**, including one without a fresh snapshot yet (just connected).
Wherever the backend picks MMs below (the auction's asked set, market data), it uses
the live ones if there are any, else the dry-run ones, so a dry-run-only setup still
works.

**Market data** (§2.1-§2.5) uses one **representative instance per MM pubkey**, so an
MM running two desks counts once: among that pubkey's instances in the chosen
(live, else dry-run) set, the one with the freshest `health` snapshot, else the most
recently connected. For `/health` and `/assets` the set is chosen among instances with
a fresh snapshot: the live ones if any MM with a fresh snapshot is live.

**Permission by asset** (§5.7). An MM takes part in anything about an asset only if its
approved caps list that asset (symbols compared case-insensitively; an omitted asset is
`SOL`). For `/price`, `/expiries` and `/board` the instances of MMs without a cap for
the asset are dropped **before** live is preferred over dry-run; if MMs are online but
none has a cap for it the answer is **404** `{error:"no market maker quotes <ASSET>"}`.

### 2.1 `GET /health`

Same shape as the desk's `/health`, aggregated, plus backend fields:

```json
{
  "ok": true,
  "service": "paytience-backend",
  "version": "0.1.0",
  "mm_pubkey": "<pubkey of an online MM, or \"\" when none>",
  "program_id": "<backend PROGRAM_ID>",
  "usdc_mint": "<backend USDC_MINT>",
  "dry_run": false,
  "price": { "source": "aggregate", "spot": 200.12, "spotAgeMs": 1200, "vol": 0.71, "expiries": 12 },
  "assets": [ /* merged, §2.2 */ ],
  "exposure": {
    "totalUsd": 1234.5, "capTotalUsd": 40000, "capPerExpiryUsd": 10000,
    "perExpiry": { "1759824000": { "onChainUsd": 1000, "reservedUsd": 234.5 } },
    "perAsset": { "SOL": { "usedUsd": 1234.5, "capUsd": 30000 } }
  },
  "stats": { "rfqs": 10, "quoted": 8, "declined": 1, "timed_out": 1, "signs": 5, "sign_rejections": 1, "sign_timeouts": 0, "broadcasts": 5 }
}
```

Aggregation over the **fresh, eligible** representatives (§2: live ones if any is
fresh, else dry-run ones; one snapshot per pubkey):
- `ok`: at least one such MM whose own `health.ok` is true.
- `mm_pubkey`: kept for older dashboards (their `resolveDesk` requires a string): the
  first merged MM, else any online MM, else `""`.
- `dry_run`: true only when every fresh eligible MM reports `dry_run: true` (nothing
  can co-sign); those are then the MMs merged.
- `price`: `spot` = median of the MMs' `price.spot`; `spotAgeMs` = min; `vol` = median;
  `expiries` = max; `source` = `"aggregate"`.
- `assets`: merged as §2.2 (each MM's entry for an asset only if it has an approved cap
  for it).
- `exposure`: `capPerExpiryUsd` is the sum of the desks' own figures and
  `perExpiry[ts].onChainUsd` / `.reservedUsd` are their sums per key. `totalUsd`,
  `capTotalUsd` and `perAsset` are the **backend's** view (§5.7) over the same MMs:
  their usage (on chain + reserved), their approved totals, and per approved symbol
  the summed usage and cap.
- When no MM is fresh: `ok: false`, `price: { "source": "aggregate", "spot": null, … }`,
  `assets: []`, `exposure` all zero with `perExpiry: {}` and `perAsset: {}`.

The status is always **200**, online or not; `ok` carries the verdict.

**How many** market makers are connected is never exposed publicly: no public response
carries a count of MMs, instances or quotes, and MM identities beyond `mm_pubkey` are
not listed either (spec: don't expose MM identity). Operators use `GET /admin/mms`.

### 2.2 `GET /assets`

`{ "assets": [...] }` (always **200**), built from the same `health` snapshots as
`/health` (their `assets` field; no `market_request` is sent), merged across the same
MMs by case-insensitive `asset`. An MM's entry for an asset is merged only if that MM
has an approved cap for it (§5.7):

| field | rule |
|---|---|
| `asset` | casing from the first MM that reports it |
| `underlying`, `venue`, `synthetic` | from a tradable entry if any, else the first |
| `tradable` | any MM reports it tradable |
| `mint`, `decimals`, `expiry_time_of_day` | from a tradable entry (null if none) |
| `fresh` | any MM's entry is fresh |
| `spot`, `atm_vol` | median of non-null values from fresh entries (all entries if none fresh) |
| `expiries` | max of non-null |

Order: SOL first, then first-seen order.

### 2.3 `GET /price?asset=SOL`

Forwarded as `market_request{method:"price"}` to one representative (§2) among the MMs
with an approved cap for the asset: the one with the freshest snapshot, else (right
after a connect) the most recently connected; the result is returned verbatim. 503
`{error}` when no MM is online, 404 when none of the online MMs quotes the asset (§2).

### 2.4 `GET /expiries?asset=SOL`

`{ "expiries": [...] }`: `market_request{method:"expiries"}` to every representative
(§2: one instance per MM pubkey with an approved cap for the asset, live preferred;
timeout 2.5 s each), merged by
`expiry_ts`:
`days` from the first; `forward_price` median; `atm_vol` median of non-null (else null);
`strikes` sorted ascending union (dedup exact values). Sorted by `expiry_ts`.
Cached 30 s per asset (case-insensitive key), with request coalescing.
If no MM is online: **503** `{error:"no market maker is online"}`. If MMs are online but
none has a cap for the asset: **404** (§2). If every asked MM
errored or timed out: `{error}` with the most common MM error message (a timeout counts
as "market maker did not respond in time"), and the status every MM error shared, else
**503**. Error results are not cached.

### 2.5 `GET /board?asset=&product=&amount=&moneyness=&max_days=&exact=`

Same query parameters as the desk. The backend fans out `market_request{method:"board"}`
to the same representatives as §2.4 (timeout 2.5 s each) and merges:
- Top level from the first successful board: `asset`, `underlying`, `venue`,
  `tradable` (OR across boards), `mint`, `decimals`, `product`, `token`, `yield_token`,
  `amount`. `index_price` = median. `generated_at` = max. `fee_pct` = min across
  contributing MMs. `mm_pubkey` = the MM contributing the most cells (ties: first).
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
1. No eligible MM instance at all (§1.2) → **503** `rfq_decline` "no market maker is
   online". Otherwise the asked set is built as §5.7 says: the instances of MMs with an
   approved cap for the asset; of those the **live** ones (§2), or every dry-run one if
   none is live (a dry-run MM would win quotes it then refuses to co-sign); of those,
   the ones whose usage is known and whose asset and total caps have room for the RFQ's
   notional. An asset the backend cannot size → **200** `rfq_decline` "<asset> is not
   listed on chain"; nobody left to ask → **200** `rfq_decline` "market-maker capacity
   reached for <ASSET>" if a cap excluded at least one MM, else "no market maker quotes
   <ASSET>".
2. Send `rfq_request` (backend `rfq_id`) to each; collect until all have answered or
   `RFQ_TIMEOUT_MS` (default 1000) elapses.
3. Winner = largest `yield_amount` among valid responses; ties → earliest received.
4. Response **200**: the winner's `rfq_response` with `rfq_id` replaced by the client's,
   plus `mm_name` (the winner's registered name). How many MMs were asked or quoted is
   not returned
   (§2.1: no public MM count).
5. No quotes but ≥1 decline → **200** `rfq_decline` with `reason`: the single distinct
   reason, or `"no market maker quoted: <r1>; <r2>"` (distinct, in arrival order).
6. No valid quote and no decline → **504** `rfq_decline` "no market maker responded
   in time". This includes MMs that timed out or disconnected, and answers that were
   discarded as invalid or spoofed (§1.4).

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
   yield_amount 8). `product` (0 SellSol, 1 BuySol), `fixed_price` and `amount` are read
   from the same data, and for `open_asset_position` the asset mint is the
   instruction's 5th account (market_maker, user, config, asset, **asset_mint**, …);
   they size the reservation of step 5. Any failure (an unknown product byte included)
   → **400** `sign_rejection` "invalid transaction: <why>".
2. Route: if the remembered winner for `nonce` has `mm_pubkey == fee payer` and that
   instance's `ws/sign` is connected → that instance. Otherwise any connected `ws/sign`
   of the fee payer pubkey (most recently authenticated first). None → **422**
   `sign_rejection` "market maker <first 8 chars> is not connected".
3. Forward `sign_request` (backend `request_id`); wait `SIGN_TIMEOUT_MS` (default
   10000). Timeout → **504** `sign_rejection` "market maker did not respond in time".
   A connection that was already gone before sending is skipped for the next
   candidate; one that closes after the request was sent → **504** `sign_rejection`
   "market maker disconnected" (not retried: the MM may have co-signed).
4. `sign_rejection` from the MM → **422**, reason passed through.
5. `sign_response` from the MM: the position's notional is reserved against that MM's
   caps under `(fee payer, nonce)` (§5.6), then
   - If it already has `signature`, return it (**200**).
   - Else if `BROADCAST` (default true): `sendTransaction` (base64, preflight on,
     `preflightCommitment: confirmed`) on `SOLANA_RPC`.
     - Success → **200** with `signature` set.
     - JSON-RPC error response with a 2xx HTTP status (e.g. simulation failed) →
       **422** `sign_rejection` "broadcast failed: <rpc message>", and the reservation
       is dropped.
     - HTTP **429** or **5xx** (whatever the body, since a throttled or failing node
       has not judged the transaction), transport failure / timeout, or a non-JSON
       body → **200** without `signature` (the dashboard then broadcasts itself, as it
       does today).
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
  "faucet cooldown: try again in N min". The slot is taken before sending.
- Transaction (fee payer and signer: the faucet authority): SPL ATA program
  `CreateIdempotent` (data `[1]`; accounts payer(s,w), ata(w), owner, mint,
  system program, token program) then SPL Token `MintTo` (data `[7] + u64 LE`;
  accounts mint(w), ata(w), authority(s)). Legacy token program
  `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`, ATA program
  `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`. Sent with `sendTransaction`, then
  polled with `getSignatureStatuses` until `confirmed` (≤ 30 s).
- The mint **definitely failed** (an error before sending, such as the blockhash
  fetch; a JSON-RPC error from `sendTransaction`; or a non-null `err` from
  `getSignatureStatuses`) → **502** `{error:"faucet mint failed: <why>"}` and the
  cooldown slot is released. `<why>` is the node's own message; "rpc busy; try again
  shortly" when the RPC answered HTTP 429 (a rate limiter refused the request, so the
  node never saw it); or "rpc unavailable" for a transport failure, HTTP 5xx or
  unparseable reply before sending (details are only logged).
- The **outcome is unknown** (`sendTransaction` hit a transport failure, HTTP 5xx or
  an unparseable reply, or the transaction was not `confirmed` within 30 s) → **502** `{ "error": "faucet mint outcome unknown; try again after the cooldown", "signature": "<the mint's signature>" }`
  and the cooldown slot is **kept**, since the mint may still land.
- Then best-effort `requestAirdrop(owner, FAUCET_SOL_AIRDROP × 10^9)`; failure is not
  an error.
- **200** `{ "pubkey", "asset": "USDC"|"<SYMBOL>", "amount", "usdc_amount", "signature", "usdc_signature", "sol_airdrop_signature": "…"|null }`
  (`usdc_amount` is `amount` for USDC else 0; `usdc_signature` = `signature`).

### 2.9 `GET /admin/mms`

Requires `Authorization: Bearer <ADMIN_TOKEN>`; **404** when `ADMIN_TOKEN` is unset,
**401** on a wrong token. The same rule guards every admin route of §5.4.

```json
{ "mms": [ {
    "mm_pubkey": "…", "name": "…", "contact": "…", "status": "approved",
    "requested": { … }, "approved": { … }, "created_at_ms": 0, "updated_at_ms": 0,
    "instances": [ { "instance_id": "…", "rfq_connected": true, "sign_connected": true, "eligible": true,
                     "connected_since_ms": 0, "last_health_ms": 0, "health_fresh": true, "health_ok": true, "dry_run": false } ],
    "rfqs_asked": 0, "quotes": 0, "declines": 0, "timeouts": 0, "wins": 0,
    "signs": 0, "sign_rejections": 0, "sign_timeouts": 0,
    "usage": { "known": true, "total_usd": 125, "by_asset_usd": { "SOL": 125 }, "reserved_usd": 0 }
} ] }
```
Every registration record (§5.1) in creation order, whatever its status, each with its
connected `instances` (oldest first), its per-pubkey counters and its `usage` (§5.6).
`total_usd` and `by_asset_usd` are on chain **plus** reserved; `reserved_usd` is the
reserved part of the total. `known` is false until the MM's first successful scan (only
approved MMs are scanned).

### 2.10 Rate limits

Per client IP (`CF-Connecting-IP` header if it parses as an IP, else the peer
address; IPv6 clients are grouped by their /64, IPv4-mapped IPv6 counts as IPv4), fixed one-minute windows: `POST /rfq` 120, `POST /sign` 30, `POST /faucet`
and `POST /mm/apply` 10 together (one class, §5.2), all `GET`s 600 (any path, unknown ones included), and WebSocket upgrade attempts
on `/ws/rfq` and `/ws/sign` 60 (their own budget, not counted as `GET`s). `OPTIONS`
and requests with other methods or `POST`s to other paths (the admin actions of §5.4
included) are not limited. Over the limit → **429**
`{error:"rate limited"}`: a plain error body on every route, `/rfq` and `/sign`
included (not a typed `rfq_decline` / `sign_rejection`), with the CORS headers.

---

## 3. Backend configuration (env)

| var | default | meaning |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP + WS listen address |
| `SOLANA_RPC` | `https://api.devnet.solana.com` | JSON-RPC for broadcast + faucet; may carry an API key in its path or query, so only its host is ever logged and no response or log line contains the rest |
| `SOLANA_SCAN_RPC` | `SOLANA_RPC` | JSON-RPC that allows `getProgramAccounts`, for the usage scan (§5.6); treated like `SOLANA_RPC` in logs and errors |
| `PROGRAM_ID` | `BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9` | breezepocket program |
| `USDC_MINT` | **required** | USDC mint in the program's GlobalConfig |
| `MM_REGISTRY_PATH` | `./data/mm-registry.json` | the registration file (§5.1); its directory is created if missing, the file is written with mode 0600 |
| `EXPOSURE_SYNC_MS` | 30000 | usage scan interval (§5.6) |
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
| `ADMIN_TOKEN` | empty (off) | enables `GET /admin/mms` and the admin actions of §5.4; without it nobody can be approved, so no MM can connect |
| `RUST_LOG` | `info` | tracing filter |

`MM_ALLOWLIST` is no longer read (§5 supersedes it). A malformed `.env` or a malformed
registry file fails startup (a missing one is fine).

Fixed limits, not read from the environment (fields of `Config` so tests can shrink
them): hello timeout 10 s, ping 20 s, idle 60 s, health freshness 30 s (§1.2, §1.3,
§2); RPC request timeout 10 s; faucet confirmation 30 s polled every 500 ms (§2.8);
rate limits (§2.10); WebSocket message cap 1 MiB, unauthenticated-socket caps
256 total / 8 per IP and the local-desk exemption (§1.2); whether the usage scan runs
at all, and records to seed an in-memory registry with (tests only, §5.6). The 16 KiB
pre-authentication frame limit, the 2 minute reservation and the 200 non-approved
records are constants.

## 4. Desk configuration (MM-system-breezepocket)

| var | meaning |
|---|---|
| `AGGREGATOR_URL` | backend base URL: `ws://127.0.0.1:8080`, `wss://api.paytience.app`; `http(s)://` is accepted and converted to `ws(s)://` |

The desk signs `hello` with `MM_KEYPAIR` and reports its `PROGRAM_ID` and `USDC_MINT`.
Its own REST server keeps running for local testing.

Before its first connection the same key registers with the backend (§5.2):
`npm run register -- --url <backend> --name … --contact … --total … --caps SOL=…`
signs and posts the application; `npm run register -- --url <backend> --status` shows
`GET /mm/status` for the desk's key.

---

## 5. Market-maker registration and caps

Supersedes `MM_ALLOWLIST`: who may connect, and how much each market maker may carry, is
held in a registry file on the backend (`MM_REGISTRY_PATH`), changed only through the
endpoints below. All caps are **USD notional at the fixed price**.

### 5.1 Records

```json
{
  "mm_pubkey": "…", "name": "Acme MM", "contact": "ops@acme.example",
  "status": "pending | approved | rejected | suspended",
  "requested": { "cap_total_usd": 20000, "asset_caps_usd": { "SOL": 10000, "WBTC": 5000 } },
  "approved":  { "cap_total_usd": 15000, "asset_caps_usd": { "SOL": 10000 } },
  "created_at_ms": 0, "updated_at_ms": 0
}
```
- Asset keys are symbols, matched case-insensitively and stored upper-cased
  (`SOL`, `WBTC`, `TSLAON`).
- `approved` is null until an admin approves. Only `approved` caps are enforced.
- An asset **absent** from `approved.asset_caps_usd` may not be quoted by that MM at all.
- The file is rewritten atomically (temp file + rename) on every change and is loaded at
  startup; a missing file is an empty registry, a malformed one fails startup.

### 5.2 `POST /mm/apply` (public; rate limit class `faucet`: 10/min per client)

```json
{ "application": "<JSON string>", "signature": "<base64 ed25519>" }
```
`application` is a JSON **string** whose parsed value is
```json
{ "mm_pubkey": "…", "name": "Acme MM", "contact": "ops@acme.example",
  "cap_total_usd": 20000, "asset_caps_usd": { "SOL": 10000, "WBTC": 5000 },
  "timestamp_ms": 1759000000000 }
```
`signature` is by `mm_pubkey` over the UTF-8 bytes of
`paytience-mm-apply:v1:` followed by the exact `application` string, so no
canonicalisation is needed.

Checks (else **400** `{error}`): valid pubkey; signature verifies; `timestamp_ms` within
10 minutes of the backend clock; `name` 1–64 chars, `contact` 1–128 chars (both trimmed,
no control characters); `cap_total_usd` finite and > 0; `asset_caps_usd` has 1–64
entries, each key 1–16 chars of `[A-Za-z0-9]`, each value finite and > 0 and
≤ `cap_total_usd`.

Result **200** `{ "mm_pubkey", "status" }`:
- unknown pubkey → new `pending` record;
- `pending` or `rejected` → the request is replaced, status becomes `pending`;
- `approved` or `suspended` → `requested` (and name/contact) are replaced, status and
  `approved` caps are unchanged: a cap change request takes effect only when an admin
  approves again.

At most 200 non-approved records are kept; beyond that → **429** `{error}`.

### 5.3 `GET /mm/status?pubkey=…` (public)

**200** `{ "mm_pubkey", "status", "approved": {…}|null }`; **404** `{error}` if unknown.
Never returns name, contact or usage.

### 5.4 Admin (Bearer `ADMIN_TOKEN`, as §2.9)

| Request | Effect |
|---|---|
| `GET /admin/mms` | Every record, plus per MM: `connected` instances and counters (§2.9) and `usage`: `{ "known": bool, "total_usd", "by_asset_usd": {SYMBOL: usd}, "reserved_usd" }` |
| `POST /admin/mms/{pubkey}/approve` body `{ "cap_total_usd"?, "asset_caps_usd"? }` | Status `approved`. Omitted fields default to the `requested` values; given ones are validated as in §5.2. Works from any status. |
| `POST /admin/mms/{pubkey}/reject` | Status `rejected`, `approved` cleared, its sockets closed (4003). |
| `POST /admin/mms/{pubkey}/suspend` | Status `suspended`, `approved` caps kept, its sockets closed (4003). Approve again to restore. |
| `DELETE /admin/mms/{pubkey}` | Record removed, sockets closed (4003). |

Unknown pubkey → **404**. Changes apply immediately, with no restart.

### 5.5 Connecting

The WebSocket hello (§1.2) is accepted only for a pubkey whose status is `approved`;
any other status, or no record, closes with **4003**. `welcome.name` is the record's name.

### 5.6 Usage

For each approved MM the backend keeps its own view of what the MM carries:

- **On chain:** every `PositionAccount` and `AssetPosition` whose `market_maker` is that
  MM and whose `settled` flag is false. **Expiry is ignored**: a position that has
  expired but is not yet settled still counts; it stops counting only once settled.
  Scanned with `getProgramAccounts` on `SOLANA_SCAN_RPC` every `EXPOSURE_SYNC_MS`.
- **Reserved:** when an MM returns a `sign_response` through the backend, that
  position's notional is reserved until the position shows up in a scan, or for 2
  minutes, whichever is first. A reservation is dropped at once if the backend's own
  broadcast is refused by the node (§2.7, 422).
- **Asset of a position:** `SOL` for `PositionAccount`; for `AssetPosition`, the symbol
  of the on-chain `AssetConfig` whose `mint` equals `asset_mint` (unknown mint →
  counted in the total under `UNKNOWN`).
- **Notional:** product SellSol → `user_collateral / 10^decimals × fixed_price / 10^6`
  (decimals 9 for SOL, else the `AssetConfig`'s); product BuySol → `user_collateral / 10^6`.

Account layouts (little-endian, after the 8-byte discriminator):
`PositionAccount` `[60,125,250,193,181,109,238,86]`: user 32, market_maker 32, product
u8 (0 SellSol, 1 BuySol), token u8, fixed_price u64, expiry_ts i64, user_collateral u64,
mm_collateral u64, yield_amount u64, nonce u64, settled u8, bump u8.
`AssetPosition` `[64,144,148,81,123,31,96,233]`: user 32, market_maker 32, asset_mint 32,
product u8, fixed_price u64, expiry_ts i64, user_collateral u64, mm_collateral u64,
yield_amount u64, nonce u64, settled u8, bump u8.
`AssetConfig` `[57,112,247,166,247,64,140,23]`: mint 32, symbol (u32 length + UTF-8),
decimals u8, expiry_time_of_day i64, bump u8.
The scan filters by discriminator and by `market_maker` at offset 40.

Until an MM's first successful scan its usage is **unknown** and it is not asked for
quotes (fail closed). If later scans fail, the last known view plus reservations is used
and the failure is logged.

### 5.7 Enforcement

The RFQ's notional is computed as in §5.6 from `product`, `amount` and `fixed_price`
(the asset's decimals from `AssetConfig`, 9 for SOL). An MM is **asked** only if
1. the asset has an approved cap for it,
2. `asset usage + notional ≤ asset cap`, and
3. `total usage + notional ≤ cap_total_usd`

(usage = on chain + reserved). MMs that fail are skipped; the rest compete as in §2.6.
The MM's own desk limits still apply on top, so the lower limit wins.

If MMs are online but none may be asked → **200** `rfq_decline` with reason
`"market-maker capacity reached for <ASSET>"` when at least one was excluded by a cap,
else `"no market maker quotes <ASSET>"`. An asset the backend cannot size (not SOL and
no `AssetConfig`) → **200** `rfq_decline` `"<asset> is not listed on chain"`.

Market data follows the same permission: `/board`, `/expiries` and `/price` for an asset
ask only MMs with an approved cap for it, and `/assets` (and `/health.assets`) merge an
MM's entry for an asset only if it has one.

`/health.exposure` reports the backend's view over the online MMs: `capTotalUsd` = sum of
their approved totals, `totalUsd` = sum of their usage, plus
`perAsset: { SYMBOL: { "usedUsd", "capUsd" } }` (sums). `capPerExpiryUsd` and
`perExpiry` remain the desks' own figures.

### 5.8 Configuration

| var | default | meaning |
|---|---|---|
| `MM_REGISTRY_PATH` | `./data/mm-registry.json` | registry file |
| `SOLANA_SCAN_RPC` | `SOLANA_RPC` | RPC that allows `getProgramAccounts` |
| `EXPOSURE_SYNC_MS` | 30000 | usage scan interval |

`MM_ALLOWLIST` is no longer read. `ADMIN_TOKEN` is required for approvals; without it
nobody can be approved.

### 5.9 Clarifications

Where the sections above leave a choice, the backend does this:

- **Admin responses.** approve, reject and suspend answer **200** with the updated
  record (the §5.1 fields); `DELETE` answers **200** `{ "mm_pubkey", "deleted": true }`.
  Invalid caps on approve → **400** `{error}`; a registry file that cannot be written →
  **500** `{error}` and nothing changes. The approve body may be empty. Approving
  triggers a usage scan at once. The admin routes are meant for `curl`: the CORS
  `allow-methods` header (§2) does not list `DELETE`.
- **`GET /mm/status`** without a `pubkey`, or with one that is not registered → **404**.
- **Duplicate symbols.** `asset_caps_usd` keys that collide once upper-cased (`sol` and
  `SOL`) → **400**.
- **Order of exclusion (§5.7).** Rule 1 is applied before live is preferred over dry-run
  (§2.6); rules 2 and 3, and unknown usage, after. Rule 1 failing for everyone, or
  everyone left having unknown usage, is "no market maker quotes <ASSET>".
- **`/health.exposure`** covers the MMs merged into `/health` (§2.1); usage that is still
  unknown counts as 0 there.
- **Reservations** are keyed by `(mm_pubkey, nonce)` and cleared when a scan of that MM
  returns a position with the same nonce, settled or not. Reserving a nonce the last
  scan already holds is a no-op.
- **Unknown mint.** A SellSol `AssetPosition` whose mint has no `AssetConfig` is sized
  with 9 decimals (under `UNKNOWN`); BuySol needs no decimals.
- **Scan failures.** If the `AssetConfig` scan has never succeeded, no MM is scanned (an
  `AssetPosition` could not be sized), so every usage stays unknown. An account that
  matches the filters but does not decode fails that MM's scan.
