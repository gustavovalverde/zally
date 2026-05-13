# Runbook: Bootstrap an Operator Wallet

## Purpose

Stand up a fresh Zally wallet on regtest, testnet, or mainnet, recording the BIP-39 mnemonic to a tightly-permissioned secret store. Use this once per environment.

## Inputs

| Input | Source | Notes |
|-------|--------|-------|
| Sealing path | operator-owned filesystem | `wallet.age` lives next to `wallet.db` by convention |
| Storage path | operator-owned filesystem | sqlite database; back it up to durable storage |
| Network | operator decision | `regtest`, `testnet`, or `mainnet`; cannot be changed after bootstrap |
| Birthday height | chain-source query or operator decision | Use the current chain tip on first deploy; anything earlier costs scan time |
| Sealing passphrase | secret manager | only used by `AgeFileSealing`; rotate via re-seal |

## Steps

1. Choose your sealing implementation.
   - Production: [`AgeFileSealing`](../../crates/zally-keys/src/age_file_sealing.rs). The sealed file is encrypted at rest using age with a passphrase-derived key.
   - Tests only: [`InMemorySealing`](../../crates/zally-testkit/src/in_memory_sealing.rs). Never use in production.
   - Demos only: `PlaintextSealing` behind the `unsafe_plaintext_seed` cargo feature. The wallet emits a `WARN` log on every `open`/`create` and tags `Capability::Plaintext` in its [capabilities snapshot](../../crates/zally-wallet/src/capabilities.rs).
2. Call [`Wallet::create`](../../crates/zally-wallet/src/wallet.rs). The function generates a 24-word mnemonic, seals the derived seed, and creates the v1 account at `birthday`.
3. Capture the returned `Mnemonic` to your secret manager out-of-band. Zally never persists it.
4. Persist the returned `AccountId`. It is the only account in the wallet; future `Wallet::open` calls re-discover it but log lines and metrics gain context if the operator records it.
5. Subscribe to [`Wallet::observe`](../../crates/zally-wallet/src/wallet.rs) before kicking off any sync so the bootstrap path emits `ScanProgress`, `TransactionConfirmed`, and `ReorgDetected` events from the first block.

## Verification

Run [`cargo run --example open-wallet`](../../crates/zally-wallet/examples/open-wallet/main.rs). The example proves the create → seal → re-open → next-address round-trip for a regtest wallet. The address printed on first `derive_next_address` must differ from the address printed after re-opening; the example asserts this and fails loudly otherwise.

For a mining-pool deployment, also run [`cargo run --example mining-payout`](../../crates/zally-wallet/examples/mining-payout/main.rs); it prints the ZIP-213 100-block confirmation depth alongside the hot-dispense single-block depth so the operator confirms which receiver-purpose policy is active.

## Failure modes

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| `WalletError::AccountAlreadyExists` | sealed seed exists and the storage already has an account | switch to `Wallet::open` |
| `WalletError::NoSealedSeed` (on open) | first run, or sealed file deleted | run `Wallet::create` |
| `WalletError::NetworkMismatch` | sealing or storage created against a different network | re-bootstrap; networks are pinned at creation |
| `WalletError::SealingError(_)` with `requires_operator: true` | wrong passphrase or corrupted sealed file | rotate from mnemonic backup; never patch the sealed file in place |

## Operator checklist

- [ ] Mnemonic captured to secret manager and verified by a second operator
- [ ] Sealed file path and sqlite path backed up to durable storage
- [ ] Capabilities snapshot reviewed for the deployment (no `Plaintext`, expected `Sqlite` storage, expected network)
- [ ] First `derive_next_address` call recorded in audit log
