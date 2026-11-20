#!/usr/bin/env bash
# Run only against a disposable, funded Regtest node and native WalletQuery endpoint.
set -euo pipefail
: "${ZINDER_ENDPOINT:?set the native WalletQuery URI}"
: "${ZALLY_TEST_NODE_JSON_RPC_ADDR:?set the disposable Regtest node JSON-RPC URI}"
export ZALLY_TEST_LIVE=1 ZALLY_NETWORK=regtest
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
exec cargo nextest run --locked --profile=ci-live -p zally-wallet --features zinder \
  --run-ignored=only -E 'test(funded_wallet_syncs_sends_and_submits_pczt_with_zinder)' \
  --success-output=immediate-final
