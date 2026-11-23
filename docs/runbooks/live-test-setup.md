# Runbook: Live-Test Setup

## Purpose

Run Zally's live-test suite against a Zcash node. Live tests are gated behind environment variables and `#[ignore]`d by default, so a developer machine without infrastructure never touches a real chain.

## Why this exists separately from the default gate

The default validation gate (`cargo nextest run --profile=ci`) only runs in-process tests against the `MockChainSource` and `MockSubmitter` fixtures. Live tests need:

- An always-on, network-reachable Zcash node (Zebra or zcashd) on regtest, testnet, or mainnet.
- A chain-read backend that fronts the node (Zally's default `ChainSource` implementation reads from a running zinder pair).
- Credentials wired through environment variables (sensitive leaves never come from `ZALLY_*` env vars on the wallet side; the live-test runner is the only consumer).
- An additional `--run-ignored=all` flag so `#[ignore]` tests execute.

## Native wallet lifecycle conformance

Zally uses Zinder's native `WalletQuery` gRPC implementation through
`zinder-client`, not the lightwalletd compatibility service. The integration pins
`zinder-client`, `zinder-core`, and the resolved `zinder-proto` to 0.21.0.
Rust 1.100.0 is required by that client. The wallet SDK remains on
`zcash_client_backend` 0.25.0 and `zcash_client_sqlite` 0.23.0; the stack update
advances `zcash_primitives` to 0.31.1 and `zcash_transparent` to 0.11.1.
Existing wallet databases use the same SDK schema.

Run the funded conformance test only against a disposable Regtest node. It spends
one mature coinbase derived from the harness's public test seed `[0x42; 32]`
(account 0, external child 0), then mines blocks. Its activation schedule comes
from the node's `getblockchaininfo`; Ironwood must be active. The node must expose
`generate`, `getaddressutxos`, `gettxout`, and `getrawtransaction`. The native query
endpoint must expose the corresponding chain, mempool, and broadcast capabilities.

```sh
ZINDER_ENDPOINT=http://127.0.0.1:32402 \
ZALLY_TEST_NODE_JSON_RPC_ADDR=http://127.0.0.1:32432 \
scripts/verify-zinder-wallet.sh
```

For basic authentication, supply `ZALLY_TEST_NODE_RPC_USER` and
`ZALLY_TEST_NODE_RPC_PASSWORD` through the environment. The harness never prints
these credentials or its generated mnemonic. It creates fresh temporary wallet
paths and seals the seed with age.

The isolated nextest process asserts account creation, transparent funding,
Ironwood self-send discovery, fresh-database restore from the same seed, retained
balances and notes after wallet reopen, resumed sync onto a new block, public
wallet pending-input observation before shielding is mined, mined status and
wallet receive discovery, PCZT abandonment, and a proven, signed PCZT payment. The final PCZT also supplies
an Ironwood disclosure that the harness verifies against the mined transaction.
The campaign send, pending, mined, and scanned steps refer to the same transparent
shielding transaction: its returned ID matches a pending input ledger entry and
the mined Ironwood receive. The additional shielded send has node RPC mempool
evidence only. Zally has no public shielded-pending read method. Its epoch-pinned
transaction status lookup deliberately excludes the live mempool, preserving
canonical absence semantics.

The actual suite order is create, transparent funding, shielding and discovery,
restore, PCZT abandonment, send and pending observation, mining and receive
scanning, PCZT payment and disclosure verification, wallet reopen, and resumed
sync. `scripts/zinder-wallet-lifecycle.py` adapts this suite to the shared Zinder
lifecycle runner: `prepare` runs the suite once, and subsequent step commands
read its assertions. Reader order does not change the suite's execution order.
Set `restart_scope` to `process-restart` in that runner's case manifest.

The suite closes the first wallet and launches a subprocess that opens the
sealed database, asserts its persisted state, and exits. After its successful
exit, the parent mines a new block and starts a second subprocess that opens
the same database and syncs that block. The parent then reopens the database
and verifies progress onto another newly mined block. Reports retain distinct
process IDs and the actual persisted heights.

The schema v2 adapter hashes independently observed transaction IDs from the
send outcome, pending wallet row, canonical mined response, and scanned wallet
note. It also records independent database, public account, and recovered
activity fingerprints for restore. Validation uses always-on checks, including
when Python runs with optimization.

For the orchestrated adapter, install `strace` and use explicit loopback IPv4
URIs with ports for the native endpoint and control node. It traces connection
syscalls across the wallet test process tree, excluding build tools. It requires
a connection to the configured
native endpoint, and rejects other network peers except the control node.
Reports distinguish the configured URI from this observed process trace and
retain its SHA-256. Set `ZALLY_TEST_LIFECYCLE_ARCHIVE` to a new private directory
to retain sanitized observations and the connection trace after runner cleanup.

Abrupt process termination and crash recovery require separate evidence.
Sapling and Orchard funded sends,
reorg recovery, Testnet spending, and public-network funds are outside this
Regtest command's coverage.

## Profiles

The repo defines two nextest profiles:

| Profile | Purpose |
|---------|---------|
| `ci` | Default profile; in-process tests only. |
| `ci-live` | Long-running profile that runs `#[ignore]` tests in serial against a live node. |

## Regtest (recommended for local iteration)

1. Bring up or reuse a Zebra regtest node. The local Z3 stack exposes JSON-RPC on `127.0.0.1:29232`.
2. Bring up or reuse a `zinder-ingest` + `zinder-query` pair for that node. See [Bringing up a Regtest Zinder chain source](#bringing-up-a-regtest-zinder-chain-source).
3. Verify readiness:

   ```sh
   curl -sS --fail http://127.0.0.1:9105/readyz
   curl -sS --fail http://127.0.0.1:9106/readyz
   ```

4. Run:

   ```sh
   ZALLY_TEST_LIVE=1 \
     ZALLY_NETWORK=regtest \
     ZINDER_ENDPOINT=http://127.0.0.1:9102 \
     cargo nextest run --profile=ci-live --features zinder --run-ignored=all
   ```

## Public Testnet

Run the probe against the Foundation Testnet endpoint:

```sh
export ZINDER_ENDPOINT=https://testnet.zinder.zfnd.org:443
export ZALLY_NETWORK=testnet
cargo run --locked -p zally-wallet --features zinder --example live-zinder-probe
```

To use your own node and indexer, follow the [Zinder Testnet deployment
guide](https://github.com/ZcashFoundation/zinder/blob/v0.23.0/docs/runbooks/deploying.md#quickstart-with-docker-compose).
Wait for the native query reader to become ready and set `ZINDER_ENDPOINT`
to its native address, `http://127.0.0.1:19102` in the Compose quickstart.

The example creates a temporary, unfunded wallet. Look for
`live_zinder_sync_outcome` with a scanned height and `live_zinder_tree_state`
with the height of a settled block. To test the reorg window and read a short
compact-block range from the same endpoint, run:

```sh
ZALLY_TEST_LIVE=1 cargo nextest run --locked -p zally-chain --features zinder \
  --test live_zinder_source --profile=ci-live --run-ignored=all
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

## Bringing up a Regtest Zinder chain source

Tests that exercise `ZinderChainSource` need a running `zinder-ingest` + `zinder-query` pair against the node above. The `live_zinder_source.rs` test target, the funded wallet proof, and the `live-zinder-probe` example all read `ZINDER_ENDPOINT` for the query process address.

Pre-requisites:

- A Zebra node reachable on the host, with `enable_cookie_auth` (or basic-auth) on its JSON-RPC port.
- A zinder workspace built locally so `zinder-ingest` and `zinder-query` can run.
- The Zally workspace built with `--features zinder`.

The `z3-regtest-zebra-1` container exposes regtest JSON-RPC on `127.0.0.1:29232` with `zebra:zebra` basic auth.

A minimal `.tmp/regtest.toml` for `zinder-ingest`:

```toml
[network]
name = "zcash-regtest"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:29232"

[node.auth]
method = "basic"
username = "zebra"
password = "zebra"

[storage]
path = ".tmp/regtest.zinder-store"
```

This configuration keeps Zinder's default 100-block reorg window, which is enough for a Regtest chain the test mines itself. The funded round trip therefore disables the wallet's minimum-window check (`WalletOptions::minimum_source_reorg_window_blocks`); a deployment runs Zinder with `reorg_window_blocks` of at least 1,000 instead (see [Bootstrap an operator wallet](bootstrap-operator-wallet.md)).

Start the writer (`zinder-ingest tip-follow`):

```sh
rm -rf .tmp/regtest.zinder-store && mkdir -p .tmp/regtest.zinder-store
cargo run --release -p zinder-ingest --bin zinder-ingest -- \
  --config .tmp/regtest.toml \
  --ops-listen-addr 127.0.0.1:9105 \
  tip-follow
```

Start the wallet projector in a separate terminal. It owns the wallet primary; port 9101 is
its private control endpoint and is not a wallet API:

```sh
rm -rf .tmp/regtest.zinder-wallet .tmp/regtest.projector-canonical-secondary
cargo run --release -p zinder-projector --bin zinder-projector -- \
  --config .tmp/regtest.toml \
  --canonical-path .tmp/regtest.zinder-store \
  --canonical-secondary-path .tmp/regtest.projector-canonical-secondary \
  --wallet-path .tmp/regtest.zinder-wallet \
  --projector-control-listen-addr 127.0.0.1:9101 \
  --ops-listen-addr 127.0.0.1:9104
```

Start the native reader (`zinder-query`) in another terminal:

```sh
rm -rf .tmp/regtest.query-canonical-secondary .tmp/regtest.query-wallet-secondary
cargo run --release -p zinder-query --bin zinder-query -- \
  --config .tmp/regtest.toml \
  --canonical-primary-path .tmp/regtest.zinder-store \
  --canonical-secondary-root .tmp/regtest.query-canonical-secondary \
  --wallet-primary-path .tmp/regtest.zinder-wallet \
  --wallet-secondary-root .tmp/regtest.query-wallet-secondary \
  --ingest-control-addr http://127.0.0.1:9100 \
  --listen-addr 127.0.0.1:9102 \
  --ops-listen-addr 127.0.0.1:9106 \
  --node-json-rpc-addr http://127.0.0.1:29232
```

Verify Zally connects:

```sh
ZINDER_ENDPOINT=http://127.0.0.1:9102 ZALLY_NETWORK=regtest \
  cargo run -p zally-wallet --example live-zinder-probe --features zinder
```

Expected output: `live_zinder_tip_observed tip_height=<N>` then `live_zinder_sync_outcome scanned_to_height=<N>`.

## Funded Zinder wallet proof

The funded T3 wallet test proves the full library path:

1. Create a Zally wallet.
2. Derive a Unified Address with a transparent receiver.
3. Fund that receiver by spending a mature deterministic regtest coinbase.
4. Mine enough regtest blocks for transparent and shielded spends.
5. Let `SyncDriver` catch up through `ZinderChainSource`.
6. Shield the transparent UTXO into Ironwood through `Wallet::shield_transparent_funds`.
7. Send a payment through `Wallet::send_payment` and `ZinderSubmitter`.
8. Propose, prove, sign, extract, and submit an Ironwood PCZT.
9. Export its Zally Ironwood disclosure and verify its message-bound spend authority and recovered
   output against the mined transaction bytes.

It is regtest-only because it spends a mature regtest coinbase controlled by the testkit key.
It does not require Zallet or a separate funder wallet. The test derives regtest upgrade
activations from the running node so Zally's transaction builder matches the node's active
consensus branch. Using Ironwood avoids making this proof depend on the 32,256-block ZIP-212 grace
period that applies to a fresh Sapling regtest chain.

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=regtest \
  ZINDER_ENDPOINT=http://127.0.0.1:9102 \
  ZALLY_TEST_NODE_JSON_RPC_ADDR=http://127.0.0.1:29232/ \
  cargo nextest run --profile=ci-live --features zinder --run-ignored=all \
    -p zally-wallet funded_wallet_syncs_sends_and_submits_pczt_with_zinder
```

Set `ZALLY_TEST_NODE_RPC_USER` and `ZALLY_TEST_NODE_RPC_PASSWORD` together only when the
node requires basic auth.

To run the round trip under NU7, use Zebra 7.0.0 or later. Zebra 7 leaves NU5 through NU7
unset on Regtest, so set every upgrade the test needs explicitly, in activation order, and keep
any configured funding streams from overlapping:

```toml
[network]
network = "Regtest"

[network.testnet_parameters.activation_heights]
NU5 = 1
NU6 = 1
"NU6.1" = 1
"NU6.2" = 1
"NU6.3" = 1
NU7 = 1
```

The test reads the `NU7` row from `getblockchaininfo`, so Zally builds version 6 transactions
on branch `0x77190AD9` with the 120-block default expiry once the chain reaches that height.

Optional amount overrides:

- `ZALLY_TEST_SHIELDING_THRESHOLD_ZAT`: minimum transparent value to shield (default `1000000`).
- `ZALLY_TEST_SEND_ZAT`: integer zatoshis for each Zally-originated spend (default `10000`).

### Port allocation

| Network | Zebra RPC | ingest control | projector control | ingest ops | query gRPC | query ops |
|---------|-----------|----------------|-------------------|------------|------------|-----------|
| regtest | 29232     | 9100           | 9101              | 9105       | 9102       | 9106      |

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
