# Error Vocabulary

Zally public errors are typed, posture-classified, and part of the public API. New variants must be added here in the same change that introduces them.

## Failure Posture

Every chain-source and wallet error exposes a `posture()` method returning a [`FailurePosture`](../../crates/zally-core/src/failure_posture.rs) value drawn from this four-class taxonomy. See [ADR-0002](../adrs/0002-source-failure-posture.md) for the architectural contract.

| Posture | Meaning | Wallet behaviour |
|---------|---------|------------------|
| `retryable` | The same call with the same arguments may succeed later. | `with_retry` retries with backoff; `CircuitBreaker` counts consecutive failures and trips at threshold. |
| `restartable` | The pinned source boundary expired; the same call with the same arguments can never succeed. | `with_retry` re-invokes an operation that re-acquires the boundary; the breaker is left untouched. |
| `requires_operator` | An operator must repair configuration, storage, or runtime state. | Surface immediately; do not retry; leave the breaker untouched. |
| `not_retryable` | The caller must change input or state before retrying. | Surface immediately; do not retry; leave the breaker untouched. |

Only `retryable` moves the breaker's failure counter, and only a call that completed closes a half-open probe. A failure in any other class leaves the breaker exactly as it found it: it neither advances the counter, nor clears a streak, nor decides a probe.

## WalletError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `Sealing` | Mirrors `SealingError` | Seed sealing or unsealing failed. |
| `Storage` | Mirrors `StorageError` | Wallet storage failed. |
| `KeyDerivation` | Mirrors `KeyDerivationError` | Key derivation failed. |
| `NoSealedSeed` | `not_retryable` | `Wallet::open` or `Wallet::open_or_create_account` had no sealed seed. |
| `AccountAlreadyExists` | `not_retryable` | `Wallet::create` found an existing account. |
| `AccountNotFound` | `not_retryable` | The sealed seed does not match any storage account. |
| `NetworkMismatch` | `requires_operator` | Two configured boundaries disagree on network. |
| `ChainSource(ChainSourceError)` | Mirrors `ChainSourceError` | A chain-read operation failed. |
| `Submitter(SubmitterError)` | Mirrors `SubmitterError` | Transaction broadcast failed. |
| `MemoOnTransparentRecipient` | `not_retryable` | ZIP-302 forbids memos on transparent recipients, including a Unified Address whose only receiver is transparent. |
| `ShieldedInputsOnTexRecipient` | `not_retryable` | ZIP-320 requires transparent-only inputs for TEX. |
| `InsufficientBalance` | `not_retryable` | The wallet lacks spendable funds for a reservation or a send, including a proposal librustzcash rejected for insufficient funds. |
| `PaymentRequestParseFailed` | `not_retryable` | ZIP-321 URI parsing failed. |
| `ProposalRejected` | Follows `ProposalRejectionKind` | The proposal or its transaction could not be built; `kind` says why. |
| `SubmissionRejected` | `not_retryable` | The node rejected the submitted transaction. The send's idempotency key stays bound to it, so a retry with the same key returns it, until the pending-broadcast verdict records it as unmineable (CORE-5). |
| `TransactionUnmineable` | `not_retryable` | A PCZT can never be mined, so it was refused before signing, proving, or extraction and its inputs were released; `reason` is the `UnmineableReason`. Recreate it; signing it again cannot repair it. The verdict assumes the wallet is the PCZT's sole broadcaster. |
| `SubmittedTransactionIdMismatch` | `requires_operator` | A success-kind submitter response returned a transaction ID different from the submitted transaction bytes. |
| `Pczt` | Mirrors `PcztError` | A PCZT role failed. |
| `PaymentDisclosureExport` | Mirrors `PaymentDisclosureExportError` | A retained PCZT could not produce the requested disclosure profile. |
| `PaymentDisclosureSourceMissing` | `not_retryable` | No finalized PCZT was retained for the transaction. |
| `CircuitBroken` | `retryable` | The wallet IO circuit breaker is open. |
| `ChainSourceHalted` | `retryable` | The sync driver found the chain source stopped following the chain, so new spends are refused; retry once the driver reports the source recovered. |
| `SourceReorgWindowTooShallow` | `requires_operator` | The sync driver read a chain source reorg window below `SyncDriverOptions::minimum_source_reorg_window_blocks`, so new spends are refused; raise the source's window (Zinder `reorg_window_blocks`) or lower the minimum. |
| `SourceReorgWindowUnverified` | `retryable` | The sync driver requires a minimum chain source reorg window and has not yet read the source's window, so new spends are refused; retry once it has, normally right after the driver starts. |
| `PendingBroadcastUnsettled` | `retryable` | A retried send's idempotency key names a transaction the next block can no longer include, and the pending-broadcast verdict has not settled it. Do not pay again under another key; retry the same key after `PendingBroadcastUnmineable` (the key then builds a replacement) or `TransactionConfirmed`. |
| `SyncDriverFailed` | Carries `FailurePosture` | The sync driver failed outside a wallet operation. Cancellation surfaces `Retryable`; panic surfaces `RequiresOperator`. |

## ChainSourceError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `Unavailable` | `retryable` | Generic source-agnostic transient backend stall (used by mocks and future non-zinder sources). |
| `BlockHeightBelowFloor` | `not_retryable` | Requested height is below the source's earliest available block. |
| `BlockHeightAboveVisibleTip` | `not_retryable` | Requested height is above the epoch's visible tip; acquire a new `ChainEpoch`. |
| `NetworkMismatch` | `requires_operator` | The source serves a different network than the caller asked for. |
| `CapabilitiesUnavailable` | `requires_operator` | The endpoint does not advertise every capability required for event-driven native sync. |
| `ContractRevisionUnsupported` | `requires_operator` | The endpoint contract revision is older than revision 2. |
| `ChainEpochPinUnavailable` | `restartable` | The pinned epoch expired; restart the wallet sync against a fresh epoch. |
| `ChainEventCursorExpired` | `restartable` | Reconcile the wallet against a fresh epoch, then subscribe from the earliest retained event. |
| `ChainEventCursorInvalid` | `not_retryable` | The source did not issue the resume cursor; discard it and subscribe from the earliest retained event. |
| `MalformedCompactBlock` | `requires_operator` | The source returned bytes that did not decode; investigate the upstream version. |
| `CompactBlockStreamOrder` | `requires_operator` | A compact-block range was incomplete, duplicated, or out of order. |
| `TreeStateAnchorHeightMismatch` | `requires_operator` | A tree-state artifact did not match the exact requested anchor height. |
| `MalformedSubtreeRootPage` | `requires_operator` | A subtree-root page exceeded its requested count or violated index or height ordering. |
| `MalformedTransparentUtxoSet` | `requires_operator` | A UTXO set contained a future-height output, duplicate outpoint, or receiver mismatch. |
| `BlockingTaskFailed` | `retryable` | A `spawn_blocking` task panicked or was cancelled. |
| `ShieldedPoolUnsupported` | `requires_operator` | The requested shielded pool has no subtree-root query path on this chain source. |
| `Indexer(IndexerError)` | Mirrors `IndexerError::retry_policy` | Lossless pass-through of a `zinder-client` error. |

## SubmitterError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `Unavailable` | `retryable` | Generic source-agnostic transient broadcast unavailability. |
| `RequestRejected` | `not_retryable` | The submitter rejected request metadata or bytes before broadcast. |
| `NetworkMismatch` | `requires_operator` | Submitter network disagrees with the transaction network. |
| `CapabilitiesUnavailable` | `requires_operator` | The endpoint does not advertise transaction broadcast support. |
| `UnsupportedResponse` | `requires_operator` | The submitter returned an unknown response variant. |
| `BlockingTaskFailed` | `retryable` | A `spawn_blocking` task panicked or was cancelled. |
| `Indexer(IndexerError)` | Mirrors `IndexerError::retry_policy` | Lossless pass-through of a `zinder-client` broadcast error. |

## TransactionParseError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `Read` | `not_retryable` | Serialized transaction bytes could not be decoded. |

## UnmineableReason

Returned by `TransactionHeader::check_mineable` and carried by `WalletError::TransactionUnmineable`. Every reason means the same thing to the caller: recreate the transaction from a new proposal. When `Wallet::sync` reaches the same verdict for a pending broadcast, it reports `WalletEvent::PendingBroadcastUnmineable` instead of an error, and the broadcast's idempotency key then builds the replacement.

| Reason | Meaning |
|--------|---------|
| `Expired` | The next block is above the transaction's expiry height. |
| `ConsensusBranchMismatch` | The transaction commits to a consensus branch the chain has left, for example NU6.3 once NU7 is active. |
| `VersionNotAccepted` | The next block's consensus branch rejects the transaction version, such as v4 once NU7 is active (ZIP 2003). |

## StorageError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `NotOpened` | `not_retryable` | Storage was used before `open_or_create`. |
| `MigrationFailed` | `requires_operator` | SQLite schema migration failed. |
| `DatabaseLocked` | `requires_operator` | Another storage handle, in this process or another, holds the wallet database open for writing; a database admits one writer (CORE-7). Stop the other writer, or share one handle by cloning it. |
| `SqliteFailed` | Carries `is_retryable` | SQLite returned an implementation error. |
| `AccountNotFound` | `not_retryable` | The requested account was absent from storage. |
| `AccountAlreadyExists` | `not_retryable` | The wallet already has its single supported account. |
| `BlockingTaskFailed` | `retryable` | A blocking storage task was cancelled or panicked. |
| `KeyDerivationFailed` | `not_retryable` | Deterministic key derivation failed inside storage. |
| `ProverUnavailable` | `requires_operator` | Sapling proving parameters are missing from the configured paths or the platform-default location. |
| `IdempotencyKeyConflict` | `not_retryable` | A send idempotency key already maps to a different transaction that can still be mined. A key whose transaction was recorded as unmineable is rebound instead. |
| `FinalizedPcztConflict` | `not_retryable` | A transaction ID is already bound to different finalized PCZT bytes. |
| `PcztLockOwnerMissing` | `not_retryable` | A PCZT supplied for abandonment was not created by this Zally storage and lacks its owner-scoped recovery token. |
| `PcztLockOwnerMalformed` | `not_retryable` | A PCZT carries a Zally lock-owner field that is not exactly 32 bytes. |
| `ChainReorgDetected` | `retryable` | Scanner input diverged from persisted chain state and needs rollback. |
| `TransparentOutputNotRecognized` | `not_retryable` | A chain source returned a transparent output script Zally cannot map. |
| `TransparentOutputValueOutOfRange` | `not_retryable` | A chain source returned a transparent output value outside the zatoshis range. |
| `ProposalRejected` | Follows `ProposalRejectionKind` | librustzcash refused to build a proposal or transaction; `kind` says why. |
| `ProposalBuildFailed` | Carries `FailurePosture` | Zally could not finish a proposal artifact librustzcash built, such as serializing a PCZT. |
| `ShieldedPoolUnsupported` | `requires_operator` | The requested shielded pool has no subtree-root write path in this storage backend. |

## ProposalRejectionKind

Carried by `WalletError::ProposalRejected` and `StorageError::ProposalRejected`. Kinds are matched from the typed librustzcash error variants, never from error text; an unrecognized upstream variant is `Other`. `ShieldedLimitExceeded` is the exception: Zally checks each proposal against the ZIP 218 limits itself until librustzcash ships that check.

| Kind | Posture | Meaning |
|------|---------|---------|
| `InsufficientFunds` | `not_retryable` | The spendable balance does not cover the payment and its fee. The wallet reports it as `WalletError::InsufficientBalance`. |
| `InputsLocked` | `retryable` | A concurrent proposal or PCZT locked a selected input first; a retry selects around it. |
| `TransactionTooLarge` | `not_retryable` | The transaction would exceed the block size; send less or consolidate notes. |
| `ExpiryConflictsWithCanonicalCrossing` | `not_retryable` | A target expiry height was supplied for a ZIP 318 canonical crossing, whose expiry comes from the ZIP 318 rolling window; retry without one. |
| `RecipientAddressExpired` | `not_retryable` | A ZIP 316 Revision 2 recipient address has expired; ask for a current one. Also returned when the address only expires before the transaction would; an earlier target expiry height passes, but choosing it to fit the address reveals the address's expiry on chain (ZIP 316), so prefer a longer-lived address. |
| `OrchardRecipientNeedsIronwood` | `not_retryable` | After NU6.3, an Orchard-receiver payment can only be delivered through the Ironwood pool, which a transaction of the requested version cannot carry. |
| `SelectionContractViolated` | `requires_operator` | librustzcash reported a broken internal contract while selecting inputs or building a step, such as a selected note it was asked to exclude or an Orchard-pool payment after Ironwood activation. |
| `ShieldedLimitExceeded` | `not_retryable` | Once NU7 is active, a transaction of the proposal would carry more shielded components than ZIP 218 allows in one block, so no block could include it. `limit` names the bound (`ShieldedBlockLimit`: Orchard actions, Ironwood actions, Sapling spends and outputs, or all shielded pools together) and `component_count` the padded count. Send a smaller amount, or consolidate notes first. |
| `Other` | `not_retryable` | Any other rejection; the reason carries the detail. |

## PcztError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `ParseFailed` | `not_retryable` | PCZT bytes could not be parsed. |
| `SerializeFailed` | `not_retryable` | PCZT or transaction bytes could not be serialized. |
| `NetworkMismatch` | `requires_operator` | A PCZT's network did not match the configured role network. |
| `NoMatchingKeys` | `not_retryable` | The supplied seed could not authorize any PCZT spend. |
| `NotFinalized` | `requires_operator` | Extraction found a PCZT that still lacked required authorizations or proofs. |
| `CombineConflict` | `not_retryable` | Combining multiple PCZTs found incompatible contents. |
| `UpstreamFailed` | Carries `is_retryable` | The upstream `pczt` role rejected the operation. |
| `ProverUnavailable` | `requires_operator` | Sapling proving parameters are missing from the platform-default location. |

## PaymentDisclosureExportError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `NetworkMismatch` | `requires_operator` | Recipient and retained PCZT networks disagree. |
| `RecipientUnsupported` | `not_retryable` | The recipient lacks the receiver required by the selected profile: Sapling for Draft1 or Orchard for the Zally Ironwood extension. |
| `ProfileUnsupported` | `not_retryable` | This build does not implement the requested disclosure profile. |
| `PcztMalformed` | `requires_operator` | Retained finalized PCZT bytes cannot be parsed. |
| `PcztSectionMalformed` | `requires_operator` | One retained PCZT protocol section cannot be parsed. |
| `TransparentInputsUnsupported` | `not_retryable` | Draft1 does not support transparent spend authority. |
| `SaplingActionsUnsupported` | `not_retryable` | The Zally Ironwood extension does not support Sapling sections. |
| `OrchardActionsUnsupported` | `not_retryable` | Draft1 does not support Orchard actions. |
| `IronwoodActionsUnsupported` | `not_retryable` | Draft1 does not support Ironwood actions. |
| `IronwoodSpendInvalid` | `requires_operator` | A retained Ironwood spend fails its nullifier or randomized-key consistency check. |
| `IronwoodSpendIncomplete` | `requires_operator` | A retained Ironwood spend lacks its action randomizer or another required field. |
| `IronwoodActionIndexOutOfRange` | `not_retryable` | An Ironwood action index exceeds the extension's `u32` range. |
| `IronwoodSpendsMissing` | `not_retryable` | The transaction proves no real Ironwood spend authority. |
| `IronwoodOutputNotFound` | `not_retryable` | No Ironwood output matches the requested recipient and amount. |
| `IronwoodOutputAmbiguous` | `not_retryable` | More than one Ironwood output matches the requested recipient and amount. |
| `IronwoodOutputOckMissing` | `requires_operator` | The selected Ironwood output has no retained or derivable outgoing cipher key. |
| `SaplingSpendInvalid` | `requires_operator` | A retained Sapling spend fails its nullifier consistency check. |
| `SaplingSpendIncomplete` | `requires_operator` | A retained Sapling spend lacks proving material. |
| `SaplingSpendIndexOutOfRange` | `not_retryable` | A Sapling spend index exceeds the Draft1 range. |
| `SaplingSpendsMissing` | `not_retryable` | The transaction proves no Sapling spend authority. |
| `SaplingOutputNotFound` | `not_retryable` | No Sapling output matches the requested recipient and amount. |
| `SaplingOutputAmbiguous` | `not_retryable` | More than one Sapling output matches the requested recipient and amount. |
| `SaplingOutputOckMissing` | `requires_operator` | The selected output has no retained outgoing cipher key. |
| `SaplingOutputIndexOutOfRange` | `not_retryable` | A Sapling output index exceeds the Draft1 range. |
| `KeyDerivationFailed` | `requires_operator` | ZIP-32 key derivation failed. |
| `ProverUnavailable` | `requires_operator` | Sapling proving parameters are unavailable. |
| `Codec` | `not_retryable` | The portable codec rejected the selected profile's production plan. |
| `Production` | Mirrors `PaymentDisclosureProductionError` | Profile-specific proof or signature production failed. |

## Portable Payment Disclosure Errors

`zcash-payment-disclosure` is Zally-independent and does not depend on `FailurePosture`. Its public
variant rustdoc records the equivalent posture used by Zally adapters.

| Error | Variants |
|-------|----------|
| `PaymentDisclosureCodecError` | `ProfileUnsupported`, `ProfileShapeMismatch`, `MessageTooLong`, `NoProvenInput`, `TransparentInputsUnsupported`, `AddressProofUnsupported`, `IndexNotIncreasing`, `Truncated`, `CompactSizeNonMinimal`, `SizeOutOfRange`, and `TrailingBytes`: all `not_retryable`. |
| `PaymentDisclosureProductionError` | `IronwoodSpendAuthorizingKeyMismatch`, `SpendAuthorizingKeyMismatch`, `SpendCircuitInvalid`, and `Codec`: `not_retryable`; `IronwoodSpendSignatureInvalid`, `RandomizedVerificationKeyMismatch`, and `SpendSignatureInvalid`: `requires_operator`. |
| `PaymentDisclosureVerificationError` | All malformed proof, transaction, Sapling index, Ironwood action index, authority, and output-recovery variants: `not_retryable`; `TransactionIdMismatch`: `requires_operator`. |
