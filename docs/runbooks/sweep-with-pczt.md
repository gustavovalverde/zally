# Runbook: Sweep with PCZT

## Purpose

Move every spendable note out of a Zally wallet to one operator-controlled receiver, signing offline through the PCZT role chain. Use this when retiring an environment, rotating sealing material, or moving an exchange's float between vaults.

## Pre-requisites

- The operator owns both the *source* wallet (online, watch-only or hot) and the *destination* receiver (UA or transparent address).
- The destination receiver is recorded in the operator's secret manager and verified out-of-band.
- The cold signer holds the sealed seed for the source wallet.
- The PCZT role surface lives in [`zally-pczt`](../../crates/zally-pczt/src/lib.rs).

## Steps

1. **Online: propose**
   - Construct a [`ProposalPlan`](../../crates/zally-wallet/src/spend.rs) naming the destination recipient and the amount the operator intends to move.
   - Call `wallet.propose(plan)` and verify the returned `Proposal`'s fee and expiry height against the operator's policy.
   - Hand the proposal off to the watch-only side that owns the `Creator`.
2. **Online: create PCZT**
   - `Creator::wrap(pczt)` returns `PcztBytes` tagged with the wallet's network.
   - Transport the bytes to the cold signer (USB stick, QR, hardware wallet bridge: your choice).
3. **Cold: prove**
   - On the cold signer, instantiate `Prover::new(network)` and call `prove_with_seed(pczt, &seed)`.
   - The prover validates the embedded network before touching the seed and creates required Sapling and Orchard proofs.
4. **Cold: sign**
   - On the cold signer, instantiate `Signer::new(network)` and call `sign_with_seed(pczt, &seed)`.
   - The signer validates the embedded network *before* touching the seed. A mismatched PCZT routes to `PcztError::NetworkMismatch` with no key derivation.
5. **Cold to online: combine (optional)**
   - For FROST or multi-sig quorums, gather every signer's `PcztBytes` and call `Combiner::new().combine(pczts)`.
   - The combiner rejects mixed networks and conflicting inputs and outputs before returning.
6. **Online: extract and submit**
   - `Extractor::new().extract(pczt)` yields `ExtractedTransaction { raw_bytes, tx_id, network }`.
   - Hand `raw_bytes` to a `Submitter` implementation. The default `ZinderSubmitter` is available behind the `zinder` feature; operators with a different chain plane plug in their own implementation.
   - Persist `tx_id` in the operator's audit log and watch for `WalletEvent::TransactionConfirmed { tx_id, .. }` on the wallet's `observe()` stream.

The custody example [`crates/zally-wallet/examples/custody-with-pczt/main.rs`](../../crates/zally-wallet/examples/custody-with-pczt/main.rs) exercises every role and produces the canonical log lines for monitoring.

## Verification

| Signal | Source | Expected |
|--------|--------|----------|
| `event = "pczt_roles_constructed"` | custody-with-pczt example | Logs network for every role; mismatches caught here. |
| `event = "network_guard_rejected_*"` | custody-with-pczt example | Proof the signer rejects misrouted PCZTs before key derivation. |
| `WalletEvent::TransactionConfirmed` | `Wallet::observe()` | `tx_id` lands at the configured confirmation depth. |
| `Submitter` returns `SubmitOutcome::Duplicate` | live submitter | Sweep tx was already submitted; do not retry blindly. |

## Failure modes

| Error | Meaning | Recovery |
|-------|---------|----------|
| `PcztError::NetworkMismatch` | PCZT and signer disagree on network. | Abort; never sign a misrouted PCZT. |
| `PcztError::NoMatchingKeys` | The cold signer's seed cannot spend the inputs. | Wrong sealed seed; switch to the matching cold signer. |
| `PcztError::CombineConflict` | Quorum members signed different proposals. | Rebuild from a single canonical proposal. |
| `PcztError::NotFinalized` | Extractor called before proving or signing completed. | Check the prove, sign, and combine steps ran successfully. |
| `SubmitterError::NodeUnavailable { is_retryable: true }` | Node down. | Retry; idempotency is preserved by the persisted `tx_id`. |

## Operator checklist

- [ ] Destination receiver verified out-of-band.
- [ ] PCZT transport channel hardened (no plain-text email).
- [ ] Cold-signer audit log captures the `pczt_network` and `configured_network` from the network guard.
- [ ] `Wallet::observe()` subscribed before the submit step.
- [ ] Audit log records `tx_id`, `confirmation_height`, and `Submitter` outcome.
