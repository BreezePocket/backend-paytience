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
| `paytience-backend` | `/opt/paytience/paytience-backend`, reads `/opt/paytience/.env` | `paytience` |
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
3. Install everything on the VM and start it. The second argument is the MM allowlist
   (`name:pubkey`, comma separated):
   ```bash
   deploy/setup-vm.sh paytience-api "PAYtience desk:7i9u4xucW9KWpjSFiVUC67EKMZ1VQVK1Tt1uJdK2xwPd"
   ```
   It writes `/opt/paytience/.env` **on the VM** from the desk's own `/opt/mm/.env`
   (`SOLANA_RPC`, `USDC_MINT`, and a copy of its `FAUCET_KEYPAIR`), adds a random
   `ADMIN_TOKEN`, and never copies those secrets off the VM. An existing `.env` is kept.
4. Point the house desk at the backend: add `AGGREGATOR_URL=ws://127.0.0.1:8080` to
   `/opt/mm/.env` and restart `mm-desk` (see the desk repo's `scripts/gcp-push-code.sh`).

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
sudo -u paytience grep ADMIN_TOKEN /opt/paytience/.env
curl -s -H "authorization: Bearer <token>" localhost:8080/admin/mms   # connected MMs + counters
```

### Onboarding another market maker

1. They generate a keypair, fund it on devnet (SOL for fees and rent, USDC for yield and
   collateral) and run the desk (or anything that speaks `docs/protocol.md`) with
   `AGGREGATOR_URL=wss://api.paytience.app`.
2. Add `Name:<their pubkey>` to `MM_ALLOWLIST` in `/opt/paytience/.env` and
   `sudo systemctl restart paytience-backend`. Connected desks reconnect on their own.

### Rolling back the dashboard to the single desk

The backend's REST API is a superset of the desk's, so the dashboard can be pointed back
at a desk URL (`VITE_API_URL`) or at any desk with `?mm=<url>`.
