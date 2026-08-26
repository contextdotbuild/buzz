#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
relay_ws="wss://buildcontext.communities.buzz.xyz"
relay_http="https://buildcontext.communities.buzz.xyz"

cd "$repo_root"
. ./bin/activate-hermit

cargo build --release \
    -p buzz-acp \
    -p buzz-agent \
    -p buzz-backend-kubernetes \
    -p buzz-dev-mcp \
    -p buzz-cli \
    -p git-credential-nostr
./scripts/bundle-sidecars.sh

cd desktop
BUZZ_RELAY_URL="$relay_ws" \
BUZZ_RELAY_HTTP="$relay_http" \
pnpm exec tauri build \
    --config src-tauri/tauri.context.conf.json \
    --bundles app \
    --no-sign \
    -- \
    --no-default-features

target_dir=$(cargo metadata \
    --manifest-path src-tauri/Cargo.toml \
    --format-version 1 \
    --no-deps \
    | node -p "JSON.parse(require('fs').readFileSync(0, 'utf8')).target_directory")
app_path="$target_dir/release/bundle/macos/Buzz Context.app"

codesign --force --deep --sign - "$app_path"
codesign --verify --deep --strict "$app_path"

echo "$app_path"
