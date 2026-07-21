# ADR-0001: Zinder-Only ChainSource

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-06-07 |
| Product | Zally |
| Domain | Chain-read source selection, wallet safety, portability posture |
| Related | [ADR-0002 Source failure posture](0002-source-failure-posture.md); [zinder ADR-0006 ingest control transport security](https://github.com/gustavovalverde/zinder/blob/main/docs/adrs/0006-ingest-control-transport-security.md) |

## Context

`zally-chain` ships exactly one `ChainSource` implementation: `ZinderChainSource`, backed by the typed `zinder-client` gRPC surface. Community Zcash wallets (zecwallet-lite, Nighthawk, Edge, YWallet) reach the chain through `lightwalletd`, the long-running Zcash Foundation light-client RPC. The recurring proposal is to ship a second built-in `ChainSource` implementation, `LightwalletdChainSource`, so operators who already run lightwalletd can point Zally at it without standing up Zinder.

The temptation is portability: a `LightwalletdChainSource` would let Zally inherit lightwalletd's deployed footprint, the public Zcash Foundation endpoints, and the operator-familiar configuration. The cost is a second source path inside `zally-chain` whose semantic guarantees do not match the trait the wallet was written against.

## Decision

`zally-chain` remains Zinder-only. We do not add a `LightwalletdChainSource` implementation to the workspace. Operators who need a public chain source self-host Zinder, and consumers who must speak the lightwalletd protocol run a translation layer outside Zally.

## Rationale

Three trait methods on `ChainSource` have no faithful lightwalletd mapping. Each gap is a wallet-safety regression, not an ergonomic complaint.

1. **`settled_tip` has no lightwalletd equivalent.** Zinder publishes a server-derived settled tip that accounts for its own reorg margin and writer state. Lightwalletd exposes only the current chain height; a `LightwalletdChainSource` would have to derive settlement client-side by subtracting a constant from the current tip. That heuristic is a wallet safety regression: the margin is no longer a server-authoritative value tied to actual reorg depth observed by the indexer, it is a guess baked into the client. Settlement-sensitive policy and persisted visible/settled reconciliation depend on `settled_tip` being trustworthy, while scanning proceeds through the pinned visible tip.

2. **`chain_event_envelopes` is push-based-with-cursor on Zinder, polling-only on lightwalletd.** Zinder streams chain events with a resumable cursor; reorgs land in the consumer's queue within the indexer's detection window. Lightwalletd has no event stream, so a `LightwalletdChainSource` would have to poll `GetLatestBlock` on a timer and diff. Reorgs are detected late, and the wallet may commit witness state for blocks that were already orphaned by the time the next poll cycle runs. The wallet's correctness argument assumes timely reorg notification; a polling adapter quietly breaks that assumption while satisfying the trait signature.

3. **`transparent_utxos` takes raw scriptPubKey bytes; lightwalletd takes a t-address string.** The trait passes scriptPubKey bytes so the source can serve any transparent receiver the wallet derives, including P2SH, multisig, and future script forms. Lightwalletd's `GetAddressUtxos` accepts only a t-address string (legacy P2PKH or P2SH-derived). A `LightwalletdChainSource` would have to encode the scriptPubKey to a t-address, which silently reports zero balance for any scriptPubKey that is not a textbook P2PKH or P2SH. Non-P2PKH/P2SH receivers would appear unfunded with no error surfaced. This is the same shape as the OAuth scope downgrade class: the surface accepts the request and returns a structurally valid empty answer.

## Consequences

- Operators who need a public chain source self-host Zinder. The TLS-over-public-internet capability lands in `zinder-client` (Slice 1 of the chain-source slice plan) so cross-region and cross-cloud Zinder deployments are straightforward without a bastion or VPN.
- Lightwalletd compatibility is the responsibility of consumers running their own translation layer. The reference path is `zinder/services/zinder-compat-lightwalletd`, which speaks the lightwalletd protocol on the south side and Zinder on the north side. Wallets that want both surfaces run the compat layer themselves; Zally does not host that translation in-process.
- `zally-chain` keeps a single native source path. The trait, failure-posture classification (ADR-0002), and resume-anchor cadence (ADR-0005) all reason about one backend's semantics. We do not pay the matrix cost of validating wallet correctness against two sources with different reorg-detection timing and different transparent-receiver coverage.
- The `zinder` cargo feature on `zally-chain` stays the gate for chain-plane functionality. Operators building a Zally distribution that does not need chain reads can still depend on `zally-chain` for its trait surface.

## Alternatives considered

- **Add `LightwalletdChainSource` and document the gaps.** Rejected: shipping a source that silently degrades settled-tip safety, reorg timing, and transparent receiver coverage moves the failure mode from a build-time decision to a runtime balance discrepancy. Documentation does not undo the silent zero-balance case on non-P2PKH/P2SH receivers.
- **Narrow the `ChainSource` trait so lightwalletd can satisfy it.** Rejected: that means dropping `settled_tip` and the scriptPubKey-bytes shape from the trait, which downgrades the wallet's correctness story to lightwalletd's lowest common denominator. The wallet was written against the richer surface on purpose.
- **Host `zinder-compat-lightwalletd` inside `zally-chain`.** Rejected: the compat layer is a south-side gRPC server, not a Rust trait impl. Putting it in `zally-chain` mixes server lifecycle with wallet library lifecycle. It stays a separate service inside the zinder workspace where operators can deploy it independently.

## References

- `crates/zally-chain/src/source.rs` (`ChainSource` trait definition)
- `crates/zally-chain/src/zinder_source.rs` (the only impl)
- `https://github.com/gustavovalverde/zinder/blob/main/crates/zinder-client/src/remote.rs` (typed client surface)
- This ADR
