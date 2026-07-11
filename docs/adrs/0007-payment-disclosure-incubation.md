# ADR-0007: Incubate ZIP-311 Payment Disclosures in Zally

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-07-11 |
| Product | Zally |
| Domain | ZIP-311 protocol incubation, PCZT retention, wallet export |
| Related | [ADR-0001 Workspace crate boundaries](0001-workspace-crate-boundaries.md); [Public interfaces](../architecture/public-interfaces.md); [Draft1 profile](../../crates/zcash-payment-disclosure/README.md) |

## Context

ZIP-311 specifies how to recreate and verify Sapling spend-authority proofs and how an outgoing
cipher key discloses a Sapling output. The ZIP is still Draft. Transparent-input rules, Orchard
and Ironwood support, versioning, and the signed and unsigned encodings remain unspecified.

Zally needs sender-side production now so an embedding wallet can produce a proof of payment for
zpay. Placing the cryptography directly in `zally-wallet` would couple a prospective upstream
implementation to Zally storage, seed sealing, and chain-source types. Placing it in zpay would
duplicate sender and verifier logic and put spend-authority production in the wrong trust boundary.

A sender can recreate the ZIP-311 Sapling proof without retaining transaction-time randomness, but
it still needs the spent note, proof-generation key, original anchor and witness, and the selected
output's outgoing cipher key. The finalized PCZT already carries these facts. The ordinary extracted
transaction does not.

## Decision

1. **The portable protocol lives in `zcash-payment-disclosure`.** The crate depends on protocol and
   cryptography crates, not on any `zally-*` crate. It owns the Draft1 codec and Sapling proof
   path, plus the separately versioned Zally Ironwood extension. Both verify against canonical
   transaction bytes and a mined height. Zally and zpay consume the same crate. This dependency
   direction is the upstream-porting boundary.

2. **Draft1 is an explicit experimental profile.** The profile byte and canonical encoding are
   documented in the crate README. They do not claim to be the future ZIP encoding. Unknown profile
   bytes fail closed. When ZIP-311 defines a standard encoding, it receives a distinct profile and
   does not reinterpret existing Draft1 bytes.

3. **Draft1 supports Sapling only.** Production proves every Sapling spend in the transaction and
   discloses exactly one Sapling output selected by recipient and `amount_zat`. It rejects
   transparent inputs, Orchard actions, Ironwood actions, ZIP-304 address proofs, missing outgoing
   cipher keys, and ambiguous output matches. At least one Sapling spend is required by ZIP-311.

4. **Ironwood uses a separate chain-anchored extension profile.** `ZallyIronwood` uses profile byte
   `0x02` and is never represented as ZIP-311 compliance. The mined transaction's consensus proof
   binds each real Ironwood action's randomized verification key to its nullifier. Production
   reproduces that key from the retained PCZT randomizer and wallet spend-authorizing key, then
   signs the network-bound disclosure digest. Verification checks that signature against the key
   in the mined action and recovers the selected output using its outgoing cipher key. Dummy
   padding actions are excluded. This avoids inventing a fresh proof system while the ZIP has no
   Ironwood construction.

5. **Finalized PCZT bytes are the durable disclosure recipe.** Successful PCZT extraction records
   the exact finalized PCZT in the wallet database under its transaction ID before submission can
   proceed. Re-recording identical bytes is idempotent; different bytes for the same transaction ID
   fail closed. The bytes contain privacy-sensitive note, witness, and output-recovery material and
   are never logged. Existing transactions that predate this retention cannot be disclosed unless
   an operator restores their finalized PCZT.

6. **The wallet API is network-tagged and key access is late.**
   `Wallet::export_payment_disclosure(ExportPaymentDisclosurePlan)` validates that the recipient
   contains the receiver required by the selected profile, validates its network, finds the
   retained PCZT, then unseals the seed. The returned Zally
   `PaymentDisclosure` pairs the portable disclosure with its `Network`. The portable crate accepts
   network consensus parameters at production and verification boundaries because the ZIP digest
   is bound to the network coin type.

7. **Sapling funding is selected explicitly for strict Draft1 flows.**
   `ShieldTransparentPlan::with_destination_pool(ShieldedPool::Sapling)` opts a caller into Sapling
   shielding. The existing activation-aware default remains unchanged for ordinary wallets. This
   avoids silently moving all Zally funds back to an older pool solely because Draft1 lacks newer
   pool support.

8. **Verification takes chain facts from the caller.** The portable verifier does not fetch chain
   state. It accepts exact canonical transaction bytes, the mined height, consensus parameters, and
   the prepared Sapling Spend verifying key. Zpay obtains transaction bytes and height from
   Zinder's typed WalletQuery client, then separately reconciles the authenticated recipient and
   amount against its payment expectation and the authenticated message against its challenge.

## Consequences

- Zally can produce and locally verify real Sapling disclosures while ZIP-311 remains Draft, and
  can use a separately identified extension for current Ironwood transactions.
- zpay can share the exact codec and verifier without depending on Zally wallet, storage, or seed
  boundaries.
- Disclosure export is intentionally available only for transactions created through the retained
  PCZT extraction path. Routing every `send_payment` through PCZT is deferred because that would
  change multi-step and TEX behavior unrelated to this capability.
- The wallet database now contains an additional recovery capability. Backups and access controls
  must treat finalized PCZT rows with the same privacy posture as shielded wallet metadata.
- Orchard, transparent inputs, ZIP-304 address proofs, and the eventual ZIP-standard encoding
  require new explicit profiles or amendments. Ironwood remains isolated in its `0x02` extension;
  none of these capabilities are inferred from Draft1.
