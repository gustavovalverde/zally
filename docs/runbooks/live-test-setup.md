# Runbook: Live-Test Setup

## Purpose

Run Zally's live-test suite against a Zcash node. Live tests are gated behind environment variables and `#[ignore]`d by default so a developer machine without infrastructure never touches a real chain.

## Why this exists separately from the default gate

The default validation gate (`cargo nextest run --profile=ci`) only runs in-process tests against the `MockChainSource` and `MockSubmitter` fixtures. Live tests need:

- An always-on, network-reachable Zcash node (Zebra or zcashd) on regtest, testnet, or mainnet
- Credentials wired through environment variables (sensitive leaves never come from `ZALLY_*` env vars on the wallet side; the live-test runner is the only consumer)
- An additional `--run-ignored=all` flag so `#[ignore]` tests execute

## Profiles

The repo defines two nextest profiles:

| Profile | Purpose |
|---------|---------|
| `ci` | Default profile; in-process tests only |
| `ci-live` | Long-running profile that runs `#[ignore]` tests in serial against a live node |

## Regtest (recommended for local iteration)

1. Bring up a Zebra (`z3`) regtest node listening on `127.0.0.1:18232` with cookie auth enabled.
2. Locate the cookie file the node wrote to disk and copy the cookie value.
3. Run:
   ```sh
   ZALLY_TEST_LIVE=1 \
     ZALLY_NETWORK=regtest \
     ZALLY_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232 \
     ZALLY_NODE__COOKIE_VALUE=__cookie__:<value> \
     cargo nextest run --profile=ci-live --run-ignored=all
   ```

## Public testnet

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=testnet \
  ZALLY_NODE__JSON_RPC_ADDR=https://testnet-zebra.example/ \
  cargo nextest run --profile=ci-live --run-ignored=all
```

## Mainnet (gated behind explicit acknowledgement)

Mainnet runs require an extra acknowledgement environment variable on top of `ZALLY_TEST_LIVE=1`:

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_TEST_ALLOW_MAINNET=1 \
  ZALLY_NETWORK=mainnet \
  ZALLY_NODE__JSON_RPC_ADDR=https://mainnet-zebra.example/ \
  cargo nextest run --profile=ci-live --run-ignored=all
```

**Never** run live tests against an operator-owned mainnet wallet without explicit confirmation in the same operations session. The test runner does not validate which wallet it is talking to; if the sealed file points at a real account, real funds are at stake.

## Current scope of live tests

Slice 1 through Slice 5 land the wallet surface against `MockChainSource` and `MockSubmitter`. The first live tests will turn on with the v1 follow-up that lands `ZinderChainSource`; see [`docs/reference/v1-follow-up.md`](../reference/v1-follow-up.md). Until then, `ZALLY_TEST_LIVE=1` only flips the `require_live` gate; no live test compiles into the workspace yet, so the command above is a no-op except for proving the env-var plumbing works end-to-end.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| Tests skipped silently | `ZALLY_TEST_LIVE` not set, or `--run-ignored=all` missing |
| `LiveTestError::MissingEnv` panic | required env var not exported |
| Cookie auth rejected | regenerate the cookie; the node rewrites it on restart |
| `LiveTestError::Refused` (mainnet) | `ZALLY_TEST_ALLOW_MAINNET=1` missing |
