#!/usr/bin/env bash
# Build the Linux binary on this machine (cargo-zigbuild) and roll it out to the VM.
# Needs: zig, cargo-zigbuild, `rustup target add x86_64-unknown-linux-gnu`, gcloud.
# Run from the repo root after deploy/setup-vm.sh has been run once.
set -euo pipefail
VM=${VM:-breezepocket-mm}
ZONE=${ZONE:-us-central1-a}
TARGET=x86_64-unknown-linux-gnu

# glibc 2.35 as the floor keeps the binary portable; the VM (Ubuntu 24.04) has 2.39.
cargo zigbuild --release --target "$TARGET.2.35"
BIN=target/$TARGET/release/paytience-backend

gcloud compute scp "$BIN" "$VM:/tmp/paytience-backend" --zone "$ZONE" --quiet
gcloud compute ssh "$VM" --zone "$ZONE" --quiet --command '
set -e
sudo install -o root -g root -m 755 /tmp/paytience-backend /opt/paytience/paytience-backend
rm -f /tmp/paytience-backend
sudo systemctl restart paytience-backend
sleep 3
systemctl is-active paytience-backend
curl -fsS localhost:8080/health | head -c 400; echo'
