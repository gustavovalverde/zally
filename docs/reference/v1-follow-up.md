# v1 Follow-Up Inventory

Slices 1 through 5 land the v1 wallet surface and its mock-backed integration story. This document inventories the deferred pieces that turn the mock-backed surfaces into a production runtime. It is the single source of truth for "what stops shipping today's library at v1.0."

Every item below names:

- The Zally surface that already ships and remains stable.
- The upstream blocker, if any.
- The minimum acceptance criteria for closing the gap.

## 1. End-to-end PCZT cycle with a funded wallet

**Surface:** [`Wallet::propose_pczt`](../../crates/zally-wallet/src/pczt.rs), [`Wallet::sign_pczt`](../../crates/zally-wallet/src/pczt.rs), [`Wallet::extract_and_submit_pczt`](../../crates/zally-wallet/src/pczt.rs) + the four `zally_pczt` roles.

**Status:** all three wallet methods ship and validate the full path against a freshly-scanned regtest and testnet WalletDb via `live-zinder-probe`. `Extractor::extract` wires `pczt::roles::tx_extractor::TransactionExtractor` with Sapling VKs from `LocalTxProver`. Storage exposes `create_pczt` and `extract_and_store_pczt` against the upstream `create_pczt_from_proposal` + `extract_and_store_transaction_from_pczt`. Pending: transparent-spend signing, and end-to-end execution against a funded wallet.

**Blocker:** transparent signing requires per-input scriptCode + sighash plumbing in the upstream pczt 0.6 transparent signer. End-to-end execution requires operator-coordinated funding (Zebra `generate N` with miner_address = Zally UA + ZIP-213 100-block maturity).

**Acceptance:**
- The custody-with-pczt example produces a real `tx_id` against a funded operator-owned wallet.
- Transparent spends sign cleanly through `Wallet::sign_pczt` rather than returning `NoMatchingKeys`.
- The runbook in [`docs/runbooks/sweep-with-pczt.md`](../runbooks/sweep-with-pczt.md) is executable end-to-end.

## 2. `LightwalletdChainSource` (optional)

**Surface:** the same [`zally_chain::ChainSource`](../../crates/zally-chain/src/chain_source.rs) trait that [`ZinderChainSource`](../../crates/zally-chain/src/zinder_chain_source.rs) ships against.

**Status:** not started. Operators who prefer lightwalletd over Zinder need a second implementation.

**Blocker:** none beyond Zally's bandwidth. The `lightwalletd` gRPC client is in `zcash_client_backend`.

**Acceptance:**
- `crates/zally-chain-lightwalletd` ships a `LightwalletdChainSource` that implements `ChainSource` against a remote lightwalletd endpoint.
- A second live-test target exercises it under `ZALLY_TEST_LIVE=1` + `ZALLY_CHAIN_SOURCE=lightwalletd`.

## 3. Operator-facing CLI / daemon (not in v1)

**Surface:** none yet.

**Status:** explicitly out of scope. Zally is a library; the operator owns the binary. This entry exists so the question "where is the CLI?" has a written answer.

**Blocker:** intentional. The first consumer (fauzec) will exercise the library shape and surface any CLI requirements that should land in v2.

---

## Tracking

Each item above should map to a GitHub issue when the team commits to closing it. Until those issues exist, this file is the index. When an item closes, **delete it from this file** rather than marking it done. The file's value is in showing what is still outstanding.
