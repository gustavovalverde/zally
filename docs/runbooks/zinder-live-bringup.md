# Runbook: Zinder Live Bring-Up for Zally

## Purpose

Stand up a local `zinder-ingest` + `zinder-query` pair against a running Zebra node so Zally's `ZinderChainSource` can read live chain data. Verified procedure for regtest and testnet; mainnet uses the same shape with a different config.

## Pre-requisites

- A Zebra node reachable on the host, with `enable_cookie_auth` (or basic-auth) on its JSON-RPC port.
- The Zinder workspace at `/Users/gustavovalverde/dev/zfnd/zinder` checked out and building (`cargo check -p zinder-client` succeeds).
- The Zally workspace at `/Users/gustavovalverde/dev/zfnd/zally` builds with `--features zinder` (it path-depends on the zinder workspace).

Zally consumes `zinder-client` directly through Rust; zinder-query's gRPC endpoint is only needed for the cross-process variant exercised by `ZinderChainSource::connect_remote`.

## Regtest (verified)

The `z3_regtest_sidecar_zebra` container exposes regtest JSON-RPC on `127.0.0.1:39232` with `zebra:zebra` basic auth.

A minimal `.tmp/regtest.toml`:

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

Start the writer (zinder-ingest tip-follow):

```sh
cd /Users/gustavovalverde/dev/zfnd/zinder
rm -rf .tmp/regtest.zinder-store && mkdir -p .tmp/regtest.zinder-store
cargo run --release -p zinder-ingest --bin zinder-ingest -- \
  --config .tmp/regtest.toml \
  --ops-listen-addr 127.0.0.1:9105 \
  tip-follow
```

Start the reader (zinder-query) in a separate terminal:

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
cd /Users/gustavovalverde/dev/zfnd/zally
ZINDER_ENDPOINT=http://127.0.0.1:9101 ZALLY_NETWORK=regtest \
  cargo run --example live-zinder-probe --features zinder
```

Expected output: `live_zinder_tip_observed tip_height=<N>` then `live_zinder_sync_outcome scanned_to_height=<N>`.

## Testnet (verified)

The `z3_zebra` container is testnet on `127.0.0.1:18232` with cookie auth. Refresh the cookie before each session:

```sh
cd /Users/gustavovalverde/dev/zfnd/zinder
COOKIE_FULL=$(docker exec z3_zebra cat /var/run/auth/.cookie)
COOKIE_PW="${COOKIE_FULL##*:}"
sed -i.bak "s|^password = .*|password = \"$COOKIE_PW\"|" \
  .tmp/testnet/config/zinder-ingest.toml
```

Start writer + reader on dedicated 92xx ports so they don't collide with the regtest 91xx pair:

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
cd /Users/gustavovalverde/dev/zfnd/zally
ZINDER_ENDPOINT=http://127.0.0.1:9203 ZALLY_NETWORK=testnet \
  cargo run --example live-zinder-probe --features zinder
```

## Port allocation

| Network | Zebra RPC | zinder-ingest control | zinder-ingest ops | zinder-query gRPC | zinder-query ops |
|---------|-----------|----------------------|-------------------|-------------------|------------------|
| regtest | 39232     | 9100                 | 9105              | 9101              | 9106             |
| testnet | 18232     | 9201                 | 9205              | 9203              | 9206             |

## Operator notes

- Both `tip-follow` and `zinder-query` log structured events with `event="..."` keys; tail them through `jq` or grep.
- Zinder stores tree-state payloads as JSON of Zebra's `z_gettreestate` response. The Zally bridge accepts the gRPC fetch but does not yet translate that JSON into the lightwalletd `TreeState` protobuf — `ZinderChainSource::tree_state_at` is the integration point that needs that translator before scanning consumers can use it. Tracked in [v1 follow-up](../reference/v1-follow-up.md).
- The `--ingest-control-addr` link from zinder-query to zinder-ingest lets the reader observe writer status; it is required for the query process to start.
- Wipe `.tmp/.../zinder-store` between runs when changing chain topology (a regtest reset, a network swap). The RocksDB store does not tolerate cross-chain reuse.
