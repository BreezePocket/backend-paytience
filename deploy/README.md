# Deploying the PAYtience backend

The backend runs on the same Google Cloud `e2-micro` as the house desk
(`breezepocket-mm`, `us-central1-a`, Ubuntu 24.04 x86_64), behind a **named** Cloudflare
tunnel, so it has a stable public hostname and no inbound ports:

```
browser ──https──▶ api.paytience.app ──cloudflared (paytience-tunnel)──▶ 127.0.0.1:8080  paytience-backend
                                                                              ▲
                                            house desk (mm-desk) ──ws://127.0.0.1:8080/ws/{rfq,sign}
outside MMs ──wss://api.paytience.app/ws/{rfq,sign}──────────────────────────┘
```

| Unit | What | Runs as |
|---|---|---|
| `paytience-backend` | `/opt/paytience/paytience-backend`, reads `/opt/paytience/.env`, writes `/opt/paytience/data/mm-registry.json` | `paytience` |
| `paytience-tunnel` | `cloudflared tunnel run` with `/etc/cloudflared/paytience-api.yml` | `paytience` |
| `mm-desk` | the house desk (`/opt/mm`), `AGGREGATOR_URL=ws://127.0.0.1:8080` | `mm` |

The VM (1 GB RAM) does not compile Rust. The binary is cross-compiled on a Mac with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```bash
brew install zig
cargo install --locked cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
```

## First-time setup

1. Log in to Cloudflare once (opens a browser; pick the `paytience.app` zone):
   ```bash
   cloudflared tunnel login
   ```
2. Create the tunnel and its DNS record:
   ```bash
   cloudflared tunnel create paytience-api
   cloudflared tunnel route dns paytience-api api.paytience.app
   ```
3. Install everything on the VM and start it:
   ```bash
   deploy/setup-vm.sh paytience-api
   ```
   It writes `/opt/paytience/.env` **on the VM** from the desk's own `/opt/mm/.env`
   (`SOLANA_RPC`, `SOLANA_SCAN_RPC` when the desk has one, `USDC_MINT`, and a copy of its
   `FAUCET_KEYPAIR`), sets `MM_REGISTRY_PATH=/opt/paytience/data/mm-registry.json`, adds
   a random `ADMIN_TOKEN`, and never copies those secrets off the VM. An existing `.env`
   is kept (it only gains `MM_REGISTRY_PATH` and the desk's `SOLANA_SCAN_RPC` if missing).
   It also creates `/opt/paytience/data` (owner `paytience`, mode 750). The backend's
   unit runs with `ProtectSystem=strict`, so that directory is the one place it may
   write: `ReadWritePaths=/opt/paytience/data` in `paytience-backend.service`. The
   directory must exist before the unit starts.
4. Point the house desk at the backend: add `AGGREGATOR_URL=ws://127.0.0.1:8080` to
   `/opt/mm/.env` and restart `mm-desk` (see the desk repo's `scripts/gcp-push-code.sh`).
5. Register and approve the house desk like any other market maker (below). Until then
   it is refused with 4003 and the backend has nobody to ask.

### Upgrading an install that used `MM_ALLOWLIST`

`MM_ALLOWLIST` is no longer read, so after this upgrade **no desk can connect until it
has registered and been approved**. Re-run `deploy/setup-vm.sh paytience-api` once
instead of only `deploy/deploy.sh`: it creates `/opt/paytience/data`, installs the unit
with `ReadWritePaths`, adds `MM_REGISTRY_PATH` (and `SOLANA_SCAN_RPC`) to the kept `.env`
and deploys the new binary. Then onboard every desk, the house desk included. The stale
`MM_ALLOWLIST` line in `/opt/paytience/.env` is ignored and can be deleted.

## Routine deploys

```bash
deploy/deploy.sh          # build, copy, restart, print local /health
curl -s https://api.paytience.app/health | head -c 300
```

## Operations

```bash
gcloud compute ssh breezepocket-mm --zone us-central1-a
journalctl -u paytience-backend -f                  # backend logs
journalctl -u paytience-tunnel -f                   # tunnel logs
# Read the admin token into the shell on the VM (it never leaves the VM or the terminal):
TOKEN=$(sudo grep -E "^ADMIN_TOKEN=" /opt/paytience/.env | cut -d= -f2-)
curl -s -H "authorization: Bearer $TOKEN" localhost:8080/admin/mms    # registrations, instances, counters, usage
sudo cat /opt/paytience/data/mm-registry.json                         # the registry file itself
```

The usage scan needs an RPC that allows `getProgramAccounts` (`SOLANA_SCAN_RPC`, else
`SOLANA_RPC`). While it fails, `journalctl` shows "usage scan failed" and `/admin/mms`
shows `"usage": { "known": false, … }`: an MM in that state is not asked for quotes.

### Onboarding a market maker

1. **They** generate a keypair, fund it on devnet (SOL for fees and rent, USDC for yield
   and collateral) and register that key from the desk repo (or with anything that
   signs the application of `docs/protocol.md` §5.2). Caps are USD notional at the fixed
   price; an asset without a cap cannot be quoted:
   ```bash
   npm run register -- --url https://api.paytience.app --name "Acme MM" \
     --contact ops@acme.example --total 20000 --caps SOL=10000,WBTC=5000
   npm run register -- --url https://api.paytience.app --status      # pending
   ```
2. **You** review and approve on the VM, with the token read there:
   ```bash
   TOKEN=$(sudo grep -E "^ADMIN_TOKEN=" /opt/paytience/.env | cut -d= -f2-)
   API=localhost:8080
   curl -s -H "authorization: Bearer $TOKEN" $API/admin/mms                       # see the request
   # as requested:
   curl -s -X POST -H "authorization: Bearer $TOKEN" $API/admin/mms/<pubkey>/approve
   # or with your own caps:
   curl -s -X POST -H "authorization: Bearer $TOKEN" -H "content-type: application/json" \
     -d '{"cap_total_usd":15000,"asset_caps_usd":{"SOL":10000}}' $API/admin/mms/<pubkey>/approve
   ```
   It applies at once: no restart, and the first usage scan of that MM starts right away.
3. **They** start the desk with `AGGREGATOR_URL=wss://api.paytience.app`. A desk that
   was already running (and being refused with 4003) reconnects on its own.

Later:

```bash
curl -s -X POST   -H "authorization: Bearer $TOKEN" $API/admin/mms/<pubkey>/suspend   # sockets closed (4003); approve again to restore
curl -s -X POST   -H "authorization: Bearer $TOKEN" $API/admin/mms/<pubkey>/reject    # approved caps cleared
curl -s -X DELETE -H "authorization: Bearer $TOKEN" $API/admin/mms/<pubkey>           # record removed
```

An MM that runs `npm run register` again while approved files a cap change request. Its
approved caps stay in force until you approve again.

### Rolling back the dashboard to the single desk

The backend's REST API is a superset of the desk's, so the dashboard can be pointed back
at a desk URL (`VITE_API_URL`) or at any desk with `?mm=<url>`.
