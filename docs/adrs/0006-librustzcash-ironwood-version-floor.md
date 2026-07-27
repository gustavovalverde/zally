# ADR-0006: Released librustzcash Ironwood/NU6.3 Dependency Family

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-07-04; amended on 2026-07-27 |
| Product | Zally |
| Domain | librustzcash dependency versioning, source topology, NU6.3 network plumbing |
| Related | [ADR-0001 Workspace crate boundaries](0001-workspace-crate-boundaries.md); [ADR-0001 Zinder-only ChainSource](0001-zinder-only-chain-source.md); [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally adopted Ironwood wallet support before the complete librustzcash family was published. The workspace therefore pinned a librustzcash Git commit and patched unreleased Orchard, shardtree, and incrementalmerkletree revisions. Those pins were necessary to keep one concrete type family while `zcash_client_backend`, `zcash_client_sqlite`, PCZT v2, Ironwood storage, and exact checkpoint truncation were unavailable together from crates.io.

That release gap has closed. The current published family contains the Ironwood and target-expiry PCZT paths Zally uses, owner-scoped proposal input locks, transaction v5 and v6 support, and the shardtree checkpoint fixes previously carried by the custom fork. Keeping the old Git and patch topology would now make Zally lag released upstream behavior and retain a parallel transparent-input selector that upstream no longer needs.

## Decision

1. **The wallet plane uses one released librustzcash family.** The workspace pins the concrete protocol types to `zcash_client_backend 0.24.0-rc.4`, `zcash_client_sqlite 0.22.0-rc.4`, `zcash_keys 0.16.0`, `zcash_primitives 0.30.0`, `zcash_protocol 0.10.1`, `zcash_address 0.13.0`, `zcash_proofs 0.30.0`, `zcash_transparent 0.10.0`, `pczt 0.9.1`, `zip321 0.9.0-rc.1`, and `orchard 0.15.4`. These versions are the latest mutually compatible published family at the time of this amendment. Exact requirements protect public and cross-crate concrete type identity.

2. **Released sources are authoritative.** Zally does not patch librustzcash, Orchard, shardtree, or incrementalmerkletree. `shardtree 0.7.1` and `incrementalmerkletree 0.8.2` replace the custom checkpoint fork. A future unpublished fix requires evidence that no released family contains it and a time-bounded ADR amendment naming the upstream exit condition.

3. **Proposal locking follows the upstream lifecycle.** Payment, shielding, and PCZT proposal creation pass a librustzcash `LockRequest`. Input selection uses the default `LockedInputPolicy::Exclude`; failed transaction or PCZT construction releases the proposal's locks; successful transaction storage clears them while its persisted spends keep the inputs unavailable. An exported PCZT retains its non-secret `LockOwner` in a proprietary global field while Zally stores the exact selected output references in `ext_zally_pending_pczt_inputs`. Explicit abandonment, failed composed roles, and cancelled creation replies release only that owner's outputs; successful extraction removes the lifecycle rows. Zally does not wrap `WalletDb` to maintain a second pending-outpoint selector. The PCZT lifecycle table records upstream lock identities but does not participate in input selection, and the pending-broadcast table remains an operator read model.

4. **The lower-level script and RustCrypto lines follow the current family.** `zcash_script` remains on `0.4` because `zcash_primitives 0.30.0` and `zcash_transparent 0.10.0` use that concrete line. Moving Zally alone to `zcash_script 0.5` would add a second script type family. Upstream also retains `bip32 0.6.0-pre.1` and exact prerelease RustCrypto dependencies, while `zcash_primitives 0.30.0` pins the same prerelease block-buffer and crypto-common line. `cargo update --dry-run -v` reports those packages as newer-available, but Zally does not patch transitive cryptography away from the versions selected by the published wallet family.

5. **PCZT support is a wire contract, not a crate-version label.** Zally parses PCZT v1 and v2 and serializes the minimal version capable of representing the transaction. Ironwood and transaction v6 require PCZT v2. `Capability::PcztV1AndV2` advertises the supported wire encodings without exposing the `pczt` crate release number as a product capability.

6. **Zinder remains a separately pinned product dependency.** `zally-chain` consumes the current Zinder `main` contract through a commit pin because those client contracts are not yet available in a newer published Zinder release. Zinder domain and protobuf values terminate at the chain adapter; wallet, storage, and PCZT interfaces use Zally's released librustzcash family. The Zinder pin is the only allowed Git source in the dependency policy.

7. **Network activation remains explicit.** Regtest controls NU6.3 through Zally's local network parameters. Testnet follows the activation height in upstream `TestNetwork`. Mainnet support is enabled only when the upstream consensus parameters define the activation and the live wallet round trip passes against that network; this ADR does not invent an activation height.

8. **Downstream consumers align at concrete-type seams.** Consumers that use only Zally types do not need direct librustzcash dependencies. A consumer that imports PCZT or Zcash concrete types directly must use the same published family, or isolate the differing types behind a byte or domain-value adapter. It must not restore the retired Git or patch topology.

## Consequences

- The wallet plane builds from registry releases and no longer depends on a librustzcash checkout or custom Orchard and Merkle-tree forks.
- Proposal concurrency, crash-bounded locks, and unmined spend exclusion follow the upstream wallet database implementation.
- Dependency refreshes move the concrete family together. `cargo update --dry-run -v`, the duplicate-package tree, the full all-targets check, and the repository validation gate are required evidence for each move.
- The remaining prerelease RustCrypto packages and `zcash_script 0.4` are intentional upstream family constraints. They are not independent Zally pins to update in isolation.
- The live NU6.3 round trip remains the integration proof for funding, shielding, owner-scoped PCZT abandonment, standard send, and PCZT-signed send.

## Alternatives considered

- **Continue tracking librustzcash `main`.** Rejected because the required Ironwood, PCZT, storage, and input-locking APIs are now published together. A moving Git commit would add source drift without providing a required capability.
- **Keep `FilteredWalletDb` as a defense in depth layer.** Rejected because it reimplements input selection, must duplicate each new `WalletRead`, `WalletWrite`, and `InputSource` method, and can disagree with upstream lock and spend state. Operator broadcast records do not need to participate in selection.
- **Patch the RustCrypto prerelease cluster to stable releases.** Rejected because those exact requirements are selected by current librustzcash members. Patching cryptographic transitive dependencies independently would test a type and behavior combination upstream did not publish.
- **Move only some concrete Zcash crates.** Rejected because types such as `ShieldedPool`, `TxId`, address values, scripts, PCZTs, and wallet traits cross Zally crate boundaries. A partial move creates incompatible duplicate types.

## Revision History

- 2026-07-05: Recorded Ironwood PCZT construction and the live testnet availability boundary.
- 2026-07-06: Extended the original single-commit family rule to every patched librustzcash workspace member and documented the upstream RustCrypto prerelease cluster.
- 2026-07-09: Moved the temporary family pin to upstream `zcash/librustzcash` commit `8e6864a3c67cab3c64a052dd20f83c553662e8b2` and added the temporary Orchard patch.
- 2026-07-10: Replaced global source patches with direct Git dependencies so Zinder's released lower-level family remained independently resolvable.
- 2026-07-27: Replaced the Git family and custom Orchard and Merkle-tree patches with the current published family, adopted upstream proposal input locking including exported-PCZT abandonment, and renamed the PCZT capability around wire versions.
