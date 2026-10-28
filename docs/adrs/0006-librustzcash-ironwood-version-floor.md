# ADR-0006: librustzcash NU7 Dependency Cohort

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-07-04; amended on 2026-07-27, 2026-10-14, and 2026-10-27 |
| Product | Zally |
| Domain | librustzcash dependency versioning, source topology, NU7 network plumbing |
| Related | [ADR-0001 Workspace crate boundaries](0001-workspace-crate-boundaries.md); [ADR-0001 Zinder-only ChainSource](0001-zinder-only-chain-source.md); [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally previously adopted the mutually compatible published family for Ironwood (NU6.3). NU7 activates on the public testnet at height 4,465,026 and on mainnet at height 3,543,405. librustzcash published the NU7 cohort as stable releases on 2026-10-21; `zcash_protocol 0.11.0` is the first release that defines the mainnet activation height. Before that, `zcash_protocol 0.10.1` had no NU7 height on either network, so a wallet on it signs post-activation transactions with the NU6.3 branch ID and every send is rejected.

The `0.11.0-pre.0` prereleases encoded unified viewing keys and unified addresses at ZIP 316 Revision 2. That changed every UFVK-derived `AccountId` and dropped transparent receivers from address text. The final releases encode at Revision 0 whenever Revision 0 can represent the value, which restores the text Zally emitted on the `0.16` key cohort.

## Decision

1. **The wallet plane uses one librustzcash cohort.** The workspace pins `zcash_client_backend 0.25.0`, `zcash_client_sqlite 0.23.0`, `zcash_keys 0.17.0`, `zcash_primitives 0.31.0`, `zcash_protocol 0.11.0`, `zcash_address 0.14.0`, `zcash_proofs 0.31.0`, `zcash_transparent 0.11.0`, `pczt 0.10.0`, `zip321 0.10.0`, `orchard 0.16.0`, `sapling-crypto 0.9`, `zip32 0.3`, `shardtree 0.8.0`, and `incrementalmerkletree 0.9.0`. These versions are compatible as one cohort. Keep concrete type boundaries on the selected versions.

2. **Registry sources are authoritative.** Zally does not patch librustzcash, Orchard, shardtree, or incrementalmerkletree. A future unpublished fix requires evidence that no compatible registry cohort contains it and a time-bounded ADR amendment naming the upstream exit condition.

3. **Proposal locking follows the upstream lifecycle.** Payment, shielding, and PCZT proposal creation pass a librustzcash `LockRequest`. Input selection uses the default `LockedInputPolicy::Exclude`; failed transaction or PCZT construction releases the proposal's locks; successful transaction storage clears them while its persisted spends keep the inputs unavailable. An exported PCZT retains its non-secret `LockOwner` in a proprietary global field while Zally stores the exact selected output references in `ext_zally_pending_pczt_inputs`. Explicit abandonment, failed composed roles, and cancelled creation replies release only that owner's outputs; successful extraction removes the lifecycle rows. Zally does not wrap `WalletDb` to maintain a second pending-outpoint selector. The PCZT lifecycle table records upstream lock identities but does not participate in input selection, and the pending-broadcast table remains an operator read model.

4. **Lower-level cryptography follows the selected cohort.** The workspace uses `zcash_script 0.6`, `secp256k1 0.33`, and `rand 0.10`, alongside the versions selected by the librustzcash crates. Zally does not patch transitive cryptography or advance those concrete types independently.

5. **PCZT support is a wire contract, not a crate-version label.** Zally parses PCZT v1 and v2 and serializes the minimal version capable of representing the transaction. Ironwood and transaction v6 require PCZT v2. `Capability::PcztV1AndV2` advertises the supported wire encodings without exposing the `pczt` crate release number as a product capability.

6. **Zinder remains a separately pinned product dependency.** `zally-chain` continues to consume Zinder revision `71f49e5ebb260287647c6894d5a718c28d82d4c1`; this NU7 family move does not change that RPC/client contract pin. That revision brings its own `zcash_protocol 0.10.1`, `zcash_address 0.13.0`, and `zcash_encoding 0.4.0`, so the dependency graph carries two copies of those crates until Zally depends on a Zinder release built on the same cohort. Zinder domain and protobuf values terminate at the chain adapter; wallet, storage, and PCZT interfaces use Zally's librustzcash cohort, so the two copies never meet at a type boundary. The Zinder pin is the only allowed Git source in the dependency policy.

7. **Network activation remains explicit.** Regtest controls NU7 through Zally's local network parameters. Testnet and Mainnet follow upstream `TestNetwork` and `MainNetwork`, where NU7 activates at heights 4,465,026 and 3,543,405. Zally never defines an activation height of its own. Consensus branch, transaction version, and the default expiry delta (40 blocks before NU7, 120 from NU7) come from the selected cohort.

8. **Downstream consumers align at concrete-type seams.** Consumers that use only Zally types do not need direct librustzcash dependencies. A consumer that imports PCZT or Zcash concrete types directly must use the same selected cohort, or isolate differing types behind a byte or domain-value adapter. It must not restore the retired Git or patch topology.

9. **Account identity is pinned to ZIP 316 Revision 0.** An `AccountId` is the UUID v5 of the account's UFVK encoded at Revision 0 for the wallet's network, whatever revision the upstream default encoder selects. A UFVK with no Revision 0 encoding fails account resolution instead of deriving a second identity. Golden tests pin the UFVK encodings, `AccountId` values, and Unified Address encodings for a fixed seed on every network, and an earlier-release wallet database must open with its identity and exposed addresses unchanged.

## Consequences

- The wallet plane builds from registry-hosted stable releases and does not depend on a librustzcash checkout or custom Orchard and Merkle-tree forks.
- Proposal concurrency, crash-bounded locks, and unmined spend exclusion follow the upstream wallet database implementation.
- Dependency refreshes move the concrete family together. `cargo update --dry-run -v`, the duplicate-package tree, the full all-targets check, and the repository validation gate are required evidence for each move.
- The RustCrypto packages and the selected `zcash_script 0.6` line follow upstream dependency constraints. They are not independent Zally pins to update in isolation.
- Opening a wallet database written by an earlier release applies the wallet migrations added since that release. The upgrade from `zcash_client_sqlite 0.22.0-rc.4` adds 18 migrations, and one of them, `fix_bad_ironwood_change_flagging`, can change the `is_change` flag that Zally reports on Ironwood receives.
- The live wallet round trip remains the integration proof for funding, shielding, owner-scoped PCZT abandonment, standard send, and PCZT-signed send.

## Alternatives considered

- **Continue tracking librustzcash `main`.** Rejected because the registry cohort provides the required Ironwood, NU7, PCZT, storage, and input-locking APIs together. A moving Git commit would add source drift without providing a required capability.
- **Derive `AccountId` from the upstream default UFVK encoding.** Rejected because the default revision already changed once between prereleases and releases. Identity is a stored contract, so Zally fixes the revision it hashes.
- **Keep `FilteredWalletDb` as a defense in depth layer.** Rejected because it reimplements input selection, must duplicate each new `WalletRead`, `WalletWrite`, and `InputSource` method, and can disagree with upstream lock and spend state. Operator broadcast records do not need to participate in selection.
- **Patch transitive RustCrypto packages to other releases.** Rejected because those exact requirements are selected by current librustzcash members. Patching cryptographic transitive dependencies independently would test a type and behavior combination upstream did not publish.
- **Move only some concrete Zcash crates.** Rejected because types such as `ShieldedPool`, `TxId`, address values, scripts, PCZTs, and wallet traits cross Zally crate boundaries. A partial move creates incompatible duplicate types.

## Revision History

- 2026-07-05: Recorded Ironwood PCZT construction and the live testnet availability boundary.
- 2026-07-06: Extended the original single-commit family rule to every patched librustzcash workspace member and documented the upstream RustCrypto prerelease cluster.
- 2026-07-09: Moved the temporary family pin to upstream `zcash/librustzcash` commit `8e6864a3c67cab3c64a052dd20f83c553662e8b2` and added the temporary Orchard patch.
- 2026-07-10: Replaced global source patches with direct Git dependencies so Zinder's released lower-level family remained independently resolvable.
- 2026-07-27: Replaced the Git family and custom Orchard and Merkle-tree patches with the current published family, adopted upstream proposal input locking including exported-PCZT abandonment, and renamed the PCZT capability around wire versions.
- 2026-10-14: Moved the wallet dependency cohort to the NU7-compatible `0.11.0-pre.0` generation, recorded public testnet activation at height 4,465,026, and preserved the Zinder client revision and RPC contract.
- 2026-10-27: Moved the cohort to the 2026-10-21 stable releases, which define mainnet NU7 activation at height 3,543,405. Pinned `AccountId` derivation to ZIP 316 Revision 0, recorded the duplicate `zcash_protocol` and `zcash_address` copies that the Zinder revision brings, recorded the wallet migrations applied to databases from earlier releases, and restored exact `shardtree` and `incrementalmerkletree` pins.
