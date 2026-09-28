#!/usr/bin/env bash
# One-time setup of the PAYtience backend on the GCE desk VM, run from the repo root
# on a machine where `cloudflared tunnel login` has been done:
#
#   deploy/setup-vm.sh <tunnel-name-or-id> "<name>:<mm pubkey>[,<name>:<pubkey>...]"
#
# Creates the `paytience` user and /opt/paytience, writes /opt/paytience/.env from the
# desk's own settings on the VM (so the RPC URL and faucet key never leave it), installs
# the tunnel credentials and both systemd units, then runs deploy/deploy.sh.
# Re-running is safe: an existing .env is kept.
set -euo pipefail
VM=${VM:-breezepocket-mm}
ZONE=${ZONE:-us-central1-a}
API_HOST=${API_HOST:-api.paytience.app}
TUNNEL=${1:?usage: deploy/setup-vm.sh <tunnel-name-or-id> <mm-allowlist>}
ALLOWLIST=${2:?usage: deploy/setup-vm.sh <tunnel-name-or-id> <mm-allowlist>}

TUNNEL_ID=$(cloudflared tunnel list --output json | python3 -c 'import json,sys; t=sys.argv[1]; print(next(x["id"] for x in json.load(sys.stdin) if t in (x["id"], x["name"])))' "$TUNNEL")
CREDS="$HOME/.cloudflared/$TUNNEL_ID.json"
[ -f "$CREDS" ] || { echo "missing tunnel credentials $CREDS" >&2; exit 1; }

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
cp deploy/paytience-backend.service deploy/paytience-tunnel.service "$tmp/"
cp "$CREDS" "$tmp/paytience-api.json"
cat > "$tmp/paytience-api.yml" <<EOF
tunnel: $TUNNEL_ID
credentials-file: /etc/cloudflared/paytience-api.json
ingress:
  - hostname: $API_HOST
    service: http://127.0.0.1:8080
  - service: http_status:404
EOF
printf '%s' "$ALLOWLIST" > "$tmp/allowlist"

gcloud compute ssh "$VM" --zone "$ZONE" --quiet --command 'rm -rf /tmp/paytience-setup && mkdir -m 700 /tmp/paytience-setup'
gcloud compute scp "$tmp"/* "$VM:/tmp/paytience-setup/" --zone "$ZONE" --quiet

gcloud compute ssh "$VM" --zone "$ZONE" --quiet --command '
set -euo pipefail
S=/tmp/paytience-setup
id paytience >/dev/null 2>&1 || sudo useradd --system --home-dir /opt/paytience --shell /usr/sbin/nologin paytience
sudo install -d -o paytience -g paytience -m 750 /opt/paytience /opt/paytience/keys

if ! sudo test -f /opt/paytience/.env; then
  desk=/opt/mm/.env
  faucet=$(sudo grep -E "^FAUCET_KEYPAIR=" $desk | head -1 | cut -d= -f2- | tr -d "\"")
  {
    echo "BIND_ADDR=127.0.0.1:8080"
    sudo grep -E "^SOLANA_RPC=" $desk | head -1
    sudo grep -E "^USDC_MINT=" $desk | head -1
    echo "MM_ALLOWLIST=\"$(cat $S/allowlist)\""
    [ -n "$faucet" ] && echo "FAUCET_KEYPAIR=/opt/paytience/keys/faucet.json"
    echo "ADMIN_TOKEN=$(openssl rand -hex 24)"
    echo "RUST_LOG=info"
  } | sudo tee /opt/paytience/.env >/dev/null
  [ -n "$faucet" ] && sudo install -o paytience -g paytience -m 600 "$faucet" /opt/paytience/keys/faucet.json
  sudo chown paytience:paytience /opt/paytience/.env
  sudo chmod 600 /opt/paytience/.env
  echo "wrote /opt/paytience/.env"
else
  echo "kept existing /opt/paytience/.env"
fi

sudo install -d -o root -g paytience -m 750 /etc/cloudflared
sudo install -o root -g paytience -m 640 $S/paytience-api.json $S/paytience-api.yml /etc/cloudflared/
sudo install -o root -g root -m 644 $S/paytience-backend.service $S/paytience-tunnel.service /etc/systemd/system/
rm -rf $S
sudo systemctl daemon-reload
sudo systemctl enable paytience-backend paytience-tunnel'

"$(dirname "$0")/deploy.sh"
gcloud compute ssh "$VM" --zone "$ZONE" --quiet --command 'sudo systemctl restart paytience-tunnel && sleep 5 && systemctl is-active paytience-tunnel'
echo "public health: https://$API_HOST/health"
