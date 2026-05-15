# Runbook: Live-Test Setup

## Purpose

Run Zally's live-test suite against a Zcash node. Live tests are gated behind environment variables and `#[ignore]`d by default, so a developer machine without infrastructure never touches a real chain.

## Why this exists separately from the default gate

The default validation gate (`cargo nextest run --profile=ci`) only runs in-process tests against the `MockChainSource` and `MockSubmitter` fixtures. Live tests need:

- An always-on, network-reachable Zcash node (Zebra or zcashd) on regtest, testnet, or mainnet.
- A chain-read backend that fronts the node (Zally's default `ChainSource` implementation reads from a running zinder pair).
- Credentials wired through environment variables (sensitive leaves never come from `ZALLY_*` env vars on the wallet side; the live-test runner is the only consumer).
- An additional `--run-ignored=all` flag so `#[ignore]` tests execute.

## Profiles

The repo defines two nextest profiles:

| Profile | Purpose |
|---------|---------|
| `ci` | Default profile; in-process tests only. |
| `ci-live` | Long-running profile that runs `#[ignore]` tests in serial against a live node. |

## Regtest (recommended for local iteration)

1. Bring up or reuse a Zebra regtest node. The local `z3` sidecar stack exposes JSON-RPC on `127.0.0.1:39232`.
2. Bring up or reuse a `zinder-ingest` + `zinder-query` pair for that node. See [Bringing up a zinder-backed chain source](#bringing-up-a-zinder-backed-chain-source).
3. Verify readiness:

   ```sh
   curl -sS --fail http://127.0.0.1:9105/readyz
   curl -sS --fail http://127.0.0.1:9106/readyz
   ```

4. Run:

   ```sh
   ZALLY_TEST_LIVE=1 \
     ZALLY_NETWORK=regtest \
     ZINDER_ENDPOINT=http://127.0.0.1:9101 \
     cargo nextest run --profile=ci-live --features zinder --run-ignored=all
   ```

## Public testnet

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=testnet \
  ZINDER_ENDPOINT=http://127.0.0.1:9203 \
  cargo nextest run --profile=ci-live --features zinder --run-ignored=all
```

## Mainnet (gated behind explicit acknowledgement)

Mainnet runs require an extra acknowledgement environment variable on top of `ZALLY_TEST_LIVE=1`:

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_TEST_ALLOW_MAINNET=1 \
  ZALLY_NETWORK=mainnet \
  ZINDER_ENDPOINT=https://mainnet-zinder.example/ \
  cargo nextest run --profile=ci-live --features zinder --run-ignored=all
```

**Never** run live tests against an operator-owned mainnet wallet without explicit confirmation in the same operations session. The test runner does not validate which wallet it is talking to; if the sealed file points at a real account, real funds are at stake.

## Bringing up a zinder-backed chain source

Tests that exercise `ZinderChainSource` need a running `zinder-ingest` + `zinder-query` pair against the node above. The `live_zinder_chain_source.rs` test target, the funded wallet proof, and the `live-zinder-probe` example all read `ZINDER_ENDPOINT` for the query process address.

Pre-requisites:

- A Zebra node reachable on the host, with `enable_cookie_auth` (or basic-auth) on its JSON-RPC port.
- A zinder workspace built locally so `zinder-ingest` and `zinder-query` can run.
- The Zally workspace built with `--features zinder`.

### Regtest

The `z3_regtest_sidecar_zebra` container exposes regtest JSON-RPC on `127.0.0.1:39232` with `zebra:zebra` basic auth.

A minimal `.tmp/regtest.toml` for `zinder-ingest`:

```toml
[network]
name = "zcash-regtest"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:39232"

[node.auth]
method = "basic"
username = "zebra"
password = "zebra"

[storage]
path = ".tmp/regtest.zinder-store"
```

Start the writer (`zinder-ingest tip-follow`):

```sh
rm -rf .tmp/regtest.zinder-store && mkdir -p .tmp/regtest.zinder-store
cargo run --release -p zinder-ingest --bin zinder-ingest -- \
  --config .tmp/regtest.toml \
  --ops-listen-addr 127.0.0.1:9105 \
  tip-follow
```

Start the reader (`zinder-query`) in a separate terminal:

```sh
rm -rf .tmp/regtest.zinder-query-secondary && mkdir -p .tmp/regtest.zinder-query-secondary
cargo run --release -p zinder-query --bin zinder-query -- \
  --config .tmp/regtest.reader.toml \
  --secondary-path .tmp/regtest.zinder-query-secondary \
  --ingest-control-addr http://127.0.0.1:9100 \
  --listen-addr 127.0.0.1:9101 \
  --ops-listen-addr 127.0.0.1:9106 \
  --node-json-rpc-addr http://127.0.0.1:39232
```

Verify Zally connects:

```sh
ZINDER_ENDPOINT=http://127.0.0.1:9101 ZALLY_NETWORK=regtest \
  cargo run -p zally-wallet --example live-zinder-probe --features zinder
```

Expected output: `live_zinder_tip_observed tip_height=<N>` then `live_zinder_sync_outcome scanned_to_height=<N>`.

### Testnet

The `z3_zebra` container is testnet on `127.0.0.1:18232` with cookie auth. Refresh the cookie before each session:

```sh
COOKIE_FULL=$(docker exec z3_zebra cat /var/run/auth/.cookie)
COOKIE_PW="${COOKIE_FULL##*:}"
sed -i.bak "s|^password = .*|password = \"$COOKIE_PW\"|" \
  .tmp/testnet/config/zinder-ingest.toml
```

Start writer and reader on dedicated 92xx ports so they don't collide with the regtest 91xx pair:

```sh
rm -rf .tmp/testnet/store && mkdir -p .tmp/testnet/store
cargo run --release -p zinder-ingest --bin zinder-ingest -- \
  --config .tmp/testnet/config/zinder-ingest.toml \
  --ops-listen-addr 127.0.0.1:9205 \
  tip-follow

rm -rf .tmp/testnet/query-secondary && mkdir -p .tmp/testnet/query-secondary
cargo run --release -p zinder-query --bin zinder-query -- \
  --config .tmp/testnet/config/zinder-query.toml \
  --secondary-path .tmp/testnet/query-secondary \
  --ingest-control-addr http://127.0.0.1:9201 \
  --listen-addr 127.0.0.1:9203 \
  --ops-listen-addr 127.0.0.1:9206 \
  --node-json-rpc-addr http://127.0.0.1:18232
```

Verify Zally connects:

```sh
ZINDER_ENDPOINT=http://127.0.0.1:9203 ZALLY_NETWORK=testnet \
  cargo run -p zally-wallet --example live-zinder-probe --features zinder
```

## Funded Zinder wallet proof

The funded T3 wallet test proves the full library path:

1. Create a Zally wallet.
2. Derive a Unified Address with a transparent receiver.
3. Fund that receiver by spending a mature deterministic regtest coinbase.
4. Mine enough regtest blocks for transparent and shielded spends.
5. Let `SyncDriver` catch up through `ZinderChainSource`.
6. Shield the transparent UTXO through `Wallet::shield_transparent_funds`.
7. Send a payment through `Wallet::send_payment` and `ZinderSubmitter`.
8. Propose, prove, sign, extract, and submit a PCZT.

It is regtest-only because it spends a mature regtest coinbase controlled by the testkit key.
It does not require Zallet or a separate funder wallet. The test derives regtest upgrade
activations from the running node so Zally's transaction builder matches the node's active
consensus branch.

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=regtest \
  ZINDER_ENDPOINT=http://127.0.0.1:9101 \
  ZALLY_TEST_NODE_JSON_RPC_ADDR=http://127.0.0.1:39232/ \
  cargo nextest run --profile=ci-live --features zinder --run-ignored=all \
    -p zally-wallet funded_wallet_syncs_sends_and_submits_pczt_with_zinder
```

Set `ZALLY_TEST_NODE_RPC_USER` and `ZALLY_TEST_NODE_RPC_PASSWORD` together only when the
node requires basic auth.

Optional amount overrides:

- `ZALLY_TEST_SHIELDING_THRESHOLD_ZAT`: minimum transparent value to shield (default `1000000`).
- `ZALLY_TEST_SEND_ZAT`: integer zatoshis for each Zally-originated spend (default `10000`).

### Port allocation

| Network | Zebra RPC | ingest control | ingest ops | query gRPC | query ops |
|---------|-----------|----------------|------------|------------|-----------|
| regtest | 39232     | 9100           | 9105       | 9101       | 9106      |
| testnet | 18232     | 9201           | 9205       | 9203       | 9206      |

### Operator notes

- Both `tip-follow` and `zinder-query` log structured events with `event="..."` keys; tail them through `jq` or grep.
- The `--ingest-control-addr` link from the reader to the writer lets the reader observe writer status; it is required for the query process to start.
- Wipe `.tmp/.../zinder-store` between runs when changing chain topology (a regtest reset, a network swap). The RocksDB store does not tolerate cross-chain reuse.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| Tests skipped silently | `ZALLY_TEST_LIVE` not set, or `--run-ignored=all` missing. |
| `LiveTestError::MissingEnv` panic | Required env var not exported. |
| Cookie auth rejected | Regenerate the cookie; the node rewrites it on restart. |
| `LiveTestError::Refused` (mainnet) | `ZALLY_TEST_ALLOW_MAINNET=1` missing. |
| `incorrect consensus branch id` | The wallet, storage, or test signer is not using the running node's advertised regtest activations. |
