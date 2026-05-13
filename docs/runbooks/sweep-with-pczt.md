# Runbook: Sweep with PCZT

## Purpose

Move every spendable note out of a Zally wallet to one operator-controlled receiver, signing offline through the PCZT role chain. Use this when retiring an environment, rotating sealing material, or moving an exchange's float between vaults.

## Pre-requisites

- The operator owns both the *source* wallet (online, watch-only or hot) and the *destination* receiver (UA or transparent address).
- The destination receiver is recorded in the operator's secret manager and verified out-of-band.
- The cold signer holds the sealed seed for the source wallet.
- The PCZT role surface lives in [`zally-pczt`](../../crates/zally-pczt/src/lib.rs).

## Slice-5 status

The PCZT role surface (`Creator`, `Signer`, `Combiner`, `Extractor`) is stable. The deep proposal-to-PCZT wiring on the wallet side lands with the v1 follow-up; see [`docs/reference/v1-follow-up.md`](../reference/v1-follow-up.md) for the exact open items.

This runbook documents the **operator-visible flow** that survives the follow-up. The interface remains the same; only the response on the wallet's `propose_pczt`/`extract_and_submit_pczt` shifts from "stub" to "wired" when the upstream work lands. The custody example [`crates/zally-wallet/examples/custody-with-pczt/main.rs`](../../crates/zally-wallet/examples/custody-with-pczt/main.rs) exercises every role today and produces the canonical log lines for monitoring.

## Steps

1. **Online: propose**
   - Construct a [`ProposalPlan`](../../crates/zally-wallet/src/spend.rs) naming the destination recipient and amount equal to `wallet.balance_zat()` (post-follow-up; today it returns `InsufficientBalance` against the empty test storage).
   - Call `wallet.propose(plan)` and verify the returned `Proposal`'s fee + expiry height against the operator's policy.
   - Hand the proposal to the watch-only side that owns the `Creator`.
2. **Online: create PCZT**
   - `Creator::wrap(pczt)` returns `PcztBytes` tagged with the wallet's network.
   - Transport the bytes to the cold signer (USB stick, QR, hardware wallet bridge — your choice).
3. **Cold: sign**
   - On the cold signer, instantiate `Signer::new(network)` and call `sign_with_seed(pczt, &seed)`.
   - The signer validates the embedded network *before* touching the seed. A mismatched PCZT routes to `PcztError::NetworkMismatch` with no key derivation.
4. **Cold → online: combine (optional)**
   - For FROST or multi-sig quorums, gather every signer's `PcztBytes` and call `Combiner::new().combine(pczts)`.
   - The combiner rejects mixed networks and conflicting inputs/outputs before returning.
5. **Online: extract and submit**
   - `Extractor::new().extract(pczt)` yields `ExtractedTransaction { raw_bytes, tx_id, network }`.
   - Hand `raw_bytes` to your `Submitter` implementation (`MockSubmitter` for tests, the v1-follow-up `ZinderSubmitter` for production).
   - Persist `tx_id` in the operator's audit log and watch for `WalletEvent::TransactionConfirmed { tx_id, .. }` on the wallet's `observe()` stream.

## Verification

| Signal | Source | Expected |
|--------|--------|----------|
| `event = "pczt_roles_constructed"` | custody-with-pczt example | logs network for every role; mismatches caught here |
| `event = "network_guard_rejected_*"` | custody-with-pczt example | proof the signer rejects misrouted PCZTs before key derivation |
| `WalletEvent::TransactionConfirmed` | `Wallet::observe()` | tx_id lands at the configured confirmation depth |
| `Submitter` returns `SubmitOutcome::Duplicate` | live submitter | sweep tx was already submitted; do not retry blindly |

## Failure modes

| Error | Meaning | Recovery |
|-------|---------|----------|
| `PcztError::NetworkMismatch` | PCZT and signer disagree on network | abort; never sign a misrouted PCZT |
| `PcztError::NoMatchingKeys` | the cold signer's seed cannot spend the inputs | wrong sealed seed; switch to the matching cold signer |
| `PcztError::CombineConflict` | quorum members signed different proposals | rebuild from a single canonical proposal |
| `PcztError::NotFinalized` | extractor called before all signers ran | check the combine step ran successfully |
| `SubmitterError::NodeUnavailable { is_retryable: true }` | node down | retry; idempotency is preserved by the persisted `tx_id` |

## Operator checklist

- [ ] Destination receiver verified out-of-band
- [ ] PCZT transport channel hardened (no plain-text email)
- [ ] Cold-signer audit log captures the `pczt_network` and `configured_network` from the network guard
- [ ] `Wallet::observe()` subscribed before the submit step
- [ ] Audit log records `tx_id`, `confirmation_height`, and `Submitter` outcome
