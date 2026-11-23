# Make a Testnet payment with Zally inside Zspend

Zspend embeds Zally in the process that holds the wallet and signs the payment. Zpay verifies it, submits it through Zinder, and tracks it to settlement. You can stop after checking the empty wallet or continue with the same wallet for one Testnet payment.

## Build and check the endpoint

Use a clean Bash shell on Linux with Git, rustup, a C/C++ toolchain, `pkg-config`, OpenSSL headers, Clang and libclang headers, protobuf compiler, Node.js, `grpcurl`, `jq`, `b2sum`, and `curl`. Install a system CA trust store for the public TLS endpoint. The [Zspend build image](https://github.com/gustavovalverde/zpay/blob/86d97f6a422521a51b1153b41f6eab70721bfc57/Dockerfile.zspend) lists the Debian build packages.

Zpay loads the public Sapling parameters at startup. If they are missing from `~/.zcash-params`, download and verify them against the BLAKE2b-512 digests used by the locked `zcash_proofs` crate.

```bash
(
  set -eu
  download=$(mktemp -d)
  trap 'rm -r -- "$download"' EXIT
  cd "$download"
  for name in sapling-spend.params sapling-output.params; do
    curl -fsSLO --retry 2 "https://download.z.cash/downloads/$name"
  done
  b2sum --check <<'HASHES'
8270785a1a0d0bc77196f000ee6d221c9c9894f55307bd9357c3f0105d31ca63991ab91324160d8f53e2bbd3c2633a6eb8bdf5205d822e7f3f73edac51b2b70c  sapling-spend.params
657e3d38dbb5cb5e7dd2970e8b03d69b4787dd907285b5a7f0790dcc8072f60bf593b32cc2d1c030e00ff5ae64bf84c5c3beb84ddc841d48264b4a171744d028  sapling-output.params
HASHES
  mkdir -p "$HOME/.zcash-params"
  install -m 600 sapling-spend.params sapling-output.params "$HOME/.zcash-params/"
)
```

Build the three binaries from a public checkout.

```bash
git clone https://github.com/gustavovalverde/zpay.git
cd zpay
git checkout --detach 86d97f6a422521a51b1153b41f6eab70721bfc57
cargo build --locked -p zspend-runtime -p zpay-runtime -p zpay-e2e
bins=$(cargo metadata --locked --no-deps --format-version 1 | jq -er '.target_directory')/debug
```

This checkout uses Rust 1.95.0 and embeds Zally v0.2.0 at `2e4ad39b23ec8403075ea51d0f7b6f2e4b52053f` with `zinder-client` 0.10.0. Keep the lockfile for this walkthrough. For Zally v0.2.3 and its SDK requirements, see the [release notes](../../CHANGELOG.md#023). [Zpay lockfile](https://github.com/gustavovalverde/zpay/blob/86d97f6a422521a51b1153b41f6eab70721bfc57/Cargo.lock)

Use the public Testnet endpoint or supply your own ready native WalletQuery endpoint with `ZINDER_GRPC_URL`. Supply the descriptor from the locked dependency to `grpcurl` so discovery works through a proxy that omits reflection.

```bash
endpoint=${ZINDER_GRPC_URL:-https://testnet.zinder.zfnd.org:443}
transport=()
if [[ $endpoint == http://* ]]; then transport=(-plaintext); fi
manifest=$(cargo metadata --locked --format-version 1 | jq -er '.packages[] | select(.name == "zinder-proto") | .manifest_path')
grpcurl "${transport[@]}" -protoset "${manifest%/*}/generated/zinder_v1_descriptor.bin" \
  -d '{}' "${endpoint#*://}" zinder.v1.wallet.WalletQuery/ServerInfo |
  jq -e '.info | select(.common.network == "zcash-testnet" and .common.contractRevision >= 8) |
    {version:.common.serviceVersion, contract:.common.contractRevision, window:.reorgWindowBlocks,
     broadcast_advertised:(.common.capabilities | index("wallet.broadcast.transaction_v1") != null)} | select(.broadcast_advertised)'
```

## Start the host and the facilitator

Use a new Testnet wallet and keep the shell open. The directory below holds the wallet, local issuer, and payment history. Back up the mnemonic that `init` shows in your private terminal. Choose free loopback ports if the defaults are occupied. The audience, key ID, and Zspend URL must match the values used to sign.

```bash
umask 077
mkdir -p .tmp
state=$(mktemp -d "$PWD/.tmp/zally-testnet.XXXXXXXX")
export ZSPEND_NETWORK=testnet ZSPEND_BIND_ADDR=127.0.0.1:8090
export ZSPEND_PUBLIC_URL=http://127.0.0.1:8090 ZSPEND_AUDIENCE=urn:zpay:zspend:local-dev
export ZSPEND_CHAIN_SOURCE_URL="$endpoint" ZSPEND_ALLOW_DEV_SEED=1
export ZSPEND_SEALED_SEED_PATH="$state/wallet.age" ZSPEND_STORAGE_PATH="$state/wallet.db"
export ZSPEND_LEDGER_URL="file:$state/usage-ledger.db" ZSPEND_JWKS_FILE="$state/issuer.jwks"
export ZPAY_NETWORK=testnet ZPAY_CHAIN_SOURCE_URL="$endpoint" ZPAY_STORE__URL="file:$state/facilitator.db"
export ZPAY_SERVER__BIND_ADDR=127.0.0.1:8080 ZPAY_OPS__BIND_ADDR=127.0.0.1:9295
export ZPAY_PAYEES__CONFIG_PATH="$PWD/etc/aether-demo.testnet.toml"
"$bins/zspend-runtime" init
node - "$state" <<'JS'
const {generateKeyPairSync} = require('node:crypto');
const {writeFileSync} = require('node:fs');
const dir = process.argv[2];
const {privateKey, publicKey} = generateKeyPairSync('ed25519');
writeFileSync(`${dir}/issuer.pem`, privateKey.export({format:'pem', type:'pkcs8'}), {flag:'wx', mode:0o600});
const jwk = {...publicKey.export({format:'jwk'}), kid:'zpay-e2e-dev', alg:'EdDSA', use:'sig'};
writeFileSync(`${dir}/issuer.jwks`, JSON.stringify({keys:[jwk]}), {flag:'wx', mode:0o600});
JS

"$bins/zspend-runtime" serve >"$state/zspend.log" 2>&1 & signer=$!
"$bins/zpay-runtime" >"$state/zpay.log" 2>&1 & facilitator=$!
```

Check that the host has synced, loaded the issuer, and selected Testnet. Zpay should report a healthy connection to the same chain. The checked-in merchant `aether-demo` charges one zatoshi to the faucet's Testnet donation receiver.

```bash
ready() { curl -fsS --retry 30 --retry-all-errors --retry-delay 2 --max-time 5 "$1/readyz"; }
ready "$ZSPEND_PUBLIC_URL" | jq -e 'select(.network == "testnet" and .jwks_cache == "loaded" and .wallet_sync.is_fresh) |
  .wallet_sync | {scanned_height, visible_tip_height, settled_tip_height}'
ready http://127.0.0.1:9295 | jq -e '.status == "ready" and .dependencies.chain.live_probe == "ok"'
curl -fsS 'http://127.0.0.1:8080/zpay/v1/accepts?payee_id=aether-demo' |
  jq -e '.[] | select(.network == "testnet" and .amount_zat == 1) | {pay_to, amount_zat}'
curl -fsS "$ZSPEND_PUBLIC_URL/v1/wallet/address" |
  jq -er 'select(.network == "testnet" and (.ua | startswith("utest1"))) | .ua'
```

## Fund the host and make one payment

Fund the **Zspend receiver** printed above. Enter it in the Testnet faucet form and retain its request ID. For the API, submit `network=testnet` and the receiver as `address` to `POST /api/v1/claim`. Supply a unique `request_id` and save it before sending so you can recover the same claim. Poll `GET /api/v1/status/testnet/{request_id}` for `confirmed` and its mined height. Use the host readiness command above to wait until `scanned_height` is at least the funding height plus nine blocks. The CLI also creates a harness wallet directory. Zspend holds the funds and signs. [Faucet](https://fauzec.com) · [Faucet API](https://fauzec.com/.well-known/openapi.json)

After checking the merchant receiver, run the following once. Zspend signs the payment and Zpay verifies and broadcasts it. Keep the private log for the prepared payment ID, transaction ID, verification, and broadcast result. A 1,000-block settlement window can take many hours.

```bash
birthday=$(ready "$ZSPEND_PUBLIC_URL" | jq -er '.wallet_sync.settled_tip_height')
RUST_LOG=zpay_e2e=info "$bins/zpay-e2e" --network testnet --wallet-dir "$state/harness" --birthday "$birthday" \
  --zinder-url "$endpoint" --zpay-url http://127.0.0.1:8080 --zspend-url "$ZSPEND_PUBLIC_URL" \
  agent-run --payee-id aether-demo --audience "$ZSPEND_AUDIENCE" \
  --issuer-key-path "$state/issuer.pem" --issuer-kid zpay-e2e-dev \
  --settlement-completion settled --poll-seconds 172800 >"$state/payment.log" 2>&1 & payment=$!
```

Use the prepared ID to check the same payment while the CLI waits or after an interruption. Run the first line once the log contains the prepared ID. Keep the same ID for both observations.

```bash
payment_id=$(sed -nE 's/.*prepared row received.*payment_id="?([[:alnum:]]+)"?.*/\1/p' "$state/payment.log" | head -1)
test -n "$payment_id" && curl -fsS "http://127.0.0.1:8080/zpay/v1/payments/$payment_id" |
  jq -e --arg payment_id "$payment_id" 'select(.payment_id == $payment_id) |
    {payment_id, status, txid: .broadcast_outcome.transaction_id, mined_block_height, confirmation_count, settled}'
```

At first confirmation, record the transaction ID, mined height, and positive confirmation count. A payment can already show `status=final` while `settled=false`. Completion is `settled=true` for the same transaction when the settlement watermark covers the mined block. The CLI also checks the Testnet explorer and exits successfully. Repeating `agent-run` creates a new payment. Use the saved ID to resume observation.

Once you have recorded settlement (or after an unfunded startup check), close both local services with `kill "$signer" "$facilitator"`. Keep the funded state, mnemonic backup, and private issuer material under your control.
