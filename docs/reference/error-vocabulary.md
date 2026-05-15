# Error Vocabulary

Zally public errors are typed, retry-classified, and part of the public API. New variants must be added here in the same change that introduces them.

## Retry Posture

| Posture | Meaning |
|---------|---------|
| `retryable` | The same call with the same arguments may succeed later. |
| `not_retryable` | The caller must change input or state before retrying. |
| `requires_operator` | An operator must repair configuration, storage, or runtime state. |

## WalletError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `Sealing` | Mirrors `SealingError` | Seed sealing or unsealing failed. |
| `Storage` | Mirrors `StorageError` | Wallet storage failed. |
| `KeyDerivation` | Mirrors `KeyDerivationError` | Key derivation failed. |
| `NoSealedSeed` | `requires_operator` | `Wallet::open` or `Wallet::open_or_create_account` had no sealed seed. |
| `AccountAlreadyExists` | `requires_operator` | `Wallet::create` found an existing account. |
| `AccountNotFound` | `requires_operator` | The sealed seed does not match any storage account. |
| `NetworkMismatch` | `requires_operator` | Two configured boundaries disagree on network. |
| `ChainSource` | Carries `is_retryable` | A chain-read operation failed. |
| `Submitter` | Mirrors `SubmitterError` | Transaction broadcast failed. |
| `MemoOnTransparentRecipient` | `not_retryable` | ZIP-302 forbids memos on transparent recipients. |
| `ShieldedInputsOnTexRecipient` | `not_retryable` | ZIP-320 requires transparent-only inputs for TEX. |
| `InsufficientBalance` | `not_retryable` | The wallet lacks spendable funds. |
| `PaymentRequestParseFailed` | `not_retryable` | ZIP-321 URI parsing failed. |
| `ProposalRejected` | `not_retryable` or `requires_operator` | Proposal construction failed. |
| `SubmissionRejected` | `not_retryable` | The node rejected the submitted transaction. |
| `Pczt` | Mirrors `PcztError` | A PCZT role failed. |
| `CircuitBroken` | `retryable` | The wallet IO circuit breaker is open. |
| `SyncDriverFailed` | Carries `is_retryable` | The sync driver failed outside a wallet operation. |

## StorageError

| Variant | Posture | Meaning |
|---------|---------|---------|
| `NotOpened` | `not_retryable` | Storage was used before `open_or_create`. |
| `MigrationFailed` | `requires_operator` | SQLite schema migration failed. |
| `SqliteFailed` | Carries `is_retryable` | SQLite returned an implementation error. |
| `AccountNotFound` | `not_retryable` | The requested account was absent from storage. |
| `AccountAlreadyExists` | `not_retryable` | The wallet already has its single supported account. |
| `BlockingTaskFailed` | `retryable` | A blocking storage task was cancelled or panicked. |
| `KeyDerivationFailed` | `not_retryable` | Deterministic key derivation failed inside storage. |
| `ProverUnavailable` | `requires_operator` | Sapling proving parameters are missing from the platform-default location. |
| `IdempotencyKeyConflict` | `not_retryable` | A send idempotency key already maps to a different transaction. |
| `ChainReorgDetected` | `retryable` | Scanner input diverged from persisted chain state and needs rollback. |
| `TransparentOutputNotRecognized` | `not_retryable` | A chain source returned a transparent output script Zally cannot map. |
| `TransparentOutputValueOutOfRange` | `not_retryable` | A chain source returned a transparent output value outside the zatoshis range. |

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
