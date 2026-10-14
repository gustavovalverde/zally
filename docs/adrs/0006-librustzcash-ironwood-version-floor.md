# ADR-0006: librustzcash NU7 Dependency Cohort

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-07-04; amended on 2026-07-27 and 2026-10-14 |
| Product | Zally |
| Domain | librustzcash dependency versioning, source topology, NU7 network plumbing |
| Related | [ADR-0001 Workspace crate boundaries](0001-workspace-crate-boundaries.md); [ADR-0001 Zinder-only ChainSource](0001-zinder-only-chain-source.md); [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally previously adopted the mutually compatible published family for Ironwood (NU6.3). The public testnet activated NU7 at height 4,465,026, and the compatible librustzcash cohort is currently published as prereleases. Zally therefore moves the concrete family together to the `0.11.0-pre.0` protocol generation while keeping the existing Zinder client revision and wallet-facing contracts intact. These crate versions are prereleases, not stable releases.

## Decision

1. **The wallet plane uses one librustzcash cohort.** The workspace pins `zcash_client_backend 0.25.0-pre.0`, `zcash_client_sqlite 0.23.0-pre.0`, `zcash_keys 0.17.0-pre.0`, `zcash_primitives 0.31.0-pre.0`, `zcash_protocol 0.11.0-pre.0`, `zcash_address 0.14.0-pre.0`, `zcash_proofs 0.31.0-pre.0`, `zcash_transparent 0.11.0-pre.0`, `pczt 0.10.0-pre.0`, `zip321 0.10.0-pre.0`, `orchard 0.16.0`, `sapling-crypto 0.9`, `shardtree 0.8`, and `incrementalmerkletree 0.9`. The librustzcash prerelease versions are compatible as one cohort; this is not a claim that they are stable releases. Keep concrete type boundaries on the selected versions.

2. **Registry sources are authoritative.** Zally does not patch librustzcash, Orchard, shardtree, or incrementalmerkletree. A future unpublished fix requires evidence that no compatible registry cohort contains it and a time-bounded ADR amendment naming the upstream exit condition.

3. **Proposal locking follows the upstream lifecycle.** Payment, shielding, and PCZT proposal creation pass a librustzcash `LockRequest`. Input selection uses the default `LockedInputPolicy::Exclude`; failed transaction or PCZT construction releases the proposal's locks; successful transaction storage clears them while its persisted spends keep the inputs unavailable. An exported PCZT retains its non-secret `LockOwner` in a proprietary global field while Zally stores the exact selected output references in `ext_zally_pending_pczt_inputs`. Explicit abandonment, failed composed roles, and cancelled creation replies release only that owner's outputs; successful extraction removes the lifecycle rows. Zally does not wrap `WalletDb` to maintain a second pending-outpoint selector. The PCZT lifecycle table records upstream lock identities but does not participate in input selection, and the pending-broadcast table remains an operator read model.

4. **Lower-level cryptography follows the selected cohort.** The workspace uses `zcash_script 0.6` and `secp256k1 0.33`, alongside the versions selected by the prerelease librustzcash crates. Zally does not patch transitive cryptography or advance those concrete types independently.

5. **PCZT support is a wire contract, not a crate-version label.** Zally parses PCZT v1 and v2 and serializes the minimal version capable of representing the transaction. Ironwood and transaction v6 require PCZT v2. `Capability::PcztV1AndV2` advertises the supported wire encodings without exposing the `pczt` crate release number as a product capability.

6. **Zinder remains a separately pinned product dependency.** `zally-chain` continues to consume Zinder revision `71f49e5ebb260287647c6894d5a718c28d82d4c1`; this NU7 family move does not change that RPC/client contract pin. Zinder domain and protobuf values terminate at the chain adapter; wallet, storage, and PCZT interfaces use Zally's librustzcash cohort. The Zinder pin is the only allowed Git source in the dependency policy.

7. **Network activation remains explicit.** Regtest controls NU7 through Zally's local network parameters. Testnet follows upstream `TestNetwork`, where NU7 activates at height 4,465,026. Mainnet support is enabled only when upstream consensus parameters define the activation and the live wallet round trip passes against that network; this ADR does not invent a mainnet activation height.

8. **Downstream consumers align at concrete-type seams.** Consumers that use only Zally types do not need direct librustzcash dependencies. A consumer that imports PCZT or Zcash concrete types directly must use the same selected cohort, or isolate differing types behind a byte or domain-value adapter. It must not restore the retired Git or patch topology.

## Consequences

- The wallet plane builds from registry-hosted crates, including prereleases, and no longer depends on a librustzcash checkout or custom Orchard and Merkle-tree forks.
- Proposal concurrency, crash-bounded locks, and unmined spend exclusion follow the upstream wallet database implementation.
- Dependency refreshes move the concrete family together. `cargo update --dry-run -v`, the duplicate-package tree, the full all-targets check, and the repository validation gate are required evidence for each move.
- The prerelease RustCrypto packages and the selected `zcash_script 0.6` line follow upstream dependency constraints. They are not independent Zally pins to update in isolation.
- The NU7-compatible librustzcash versions are prereleases; Zally must move them as one reviewed cohort when compatible stable releases become available.
- The live wallet round trip remains the integration proof for funding, shielding, owner-scoped PCZT abandonment, standard send, and PCZT-signed send.

## Alternatives considered

- **Continue tracking librustzcash `main`.** Rejected because the registry prerelease cohort provides the required Ironwood, NU7, PCZT, storage, and input-locking APIs together. A moving Git commit would add source drift without providing a required capability.
- **Keep `FilteredWalletDb` as a defense in depth layer.** Rejected because it reimplements input selection, must duplicate each new `WalletRead`, `WalletWrite`, and `InputSource` method, and can disagree with upstream lock and spend state. Operator broadcast records do not need to participate in selection.
- **Patch the RustCrypto prerelease cluster to stable releases.** Rejected because those exact requirements are selected by current librustzcash members. Patching cryptographic transitive dependencies independently would test a type and behavior combination upstream did not publish.
- **Move only some concrete Zcash crates.** Rejected because types such as `ShieldedPool`, `TxId`, address values, scripts, PCZTs, and wallet traits cross Zally crate boundaries. A partial move creates incompatible duplicate types.

## Revision History

- 2026-07-05: Recorded Ironwood PCZT construction and the live testnet availability boundary.
- 2026-07-06: Extended the original single-commit family rule to every patched librustzcash workspace member and documented the upstream RustCrypto prerelease cluster.
- 2026-07-09: Moved the temporary family pin to upstream `zcash/librustzcash` commit `8e6864a3c67cab3c64a052dd20f83c553662e8b2` and added the temporary Orchard patch.
- 2026-07-10: Replaced global source patches with direct Git dependencies so Zinder's released lower-level family remained independently resolvable.
- 2026-07-27: Replaced the Git family and custom Orchard and Merkle-tree patches with the current published family, adopted upstream proposal input locking including exported-PCZT abandonment, and renamed the PCZT capability around wire versions.
- 2026-10-14: Moved the wallet dependency cohort to the NU7-compatible `0.11.0-pre.0` generation, recorded public testnet activation at height 4,465,026, and preserved the Zinder client revision and RPC contract.
