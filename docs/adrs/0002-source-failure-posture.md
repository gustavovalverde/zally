# ADR-0002: Source Failure Posture

| Field | Value |
|-------|-------|
| Status | Accepted on 2026-05-16 |
| Product | Zally |
| Domain | Chain-read error classification, wallet retry, circuit breaker |
| Related | [zinder ADR-0013 source-failure recovery topology](https://github.com/gustavovalverde/zinder/blob/main/docs/adrs/0013-source-failure-recovery-topology.md); [Error vocabulary](../reference/error-vocabulary.md); [Public interfaces](../architecture/public-interfaces.md) |

## Context

`zally-chain` exposed two retry-classification primitives:

1. A boolean field `is_retryable: bool` carried on the catchall variants `ChainSourceError::Unavailable`, `ChainSourceError::UpstreamFailed`, `SubmitterError::Unavailable`, `SubmitterError::UpstreamFailed`, plus mirror copies on `WalletError::ChainSource` and `WalletError::SyncDriverFailed`.
2. A 50-line mapping table `zinder_chain_source::zinder_error_to_chain_source` that collapsed thirteen typed `IndexerError` variants into the two zally-side catchalls, hand-picking `is_retryable` per variant.

This design is the same shape that produced the zinder 2026-05-15 production incident (writer exits on an unknown JSON-RPC error code). The bool cannot describe operator-action failures distinctly from caller-bug failures, and the mapping table loses every typed identity the upstream client already provides. New `IndexerError` variants default to `UpstreamFailed { is_retryable: false }` until the table is updated, which means a future zinder release can silently degrade.

In parallel, `zinder-client` already exposes `IndexerError::retry_policy() -> RetryPolicy` with three variants (`RetryWithBackoff`, `OperatorActionRequired`, `ClientError`) that is documented to be stable across zinder releases. Zally was re-deriving this classification with strictly less information.

## Decision

1. **`FailurePosture` replaces `is_retryable: bool`.** A new enum `zally_chain::FailurePosture { Retryable, RequiresOperator, NotRetryable }` is the single operator-facing classification used at the wallet boundary. Labels (`retryable`, `requires_operator`, `not_retryable`) match the posture vocabulary already used in zally documentation and align 1:1 with zinder's `RetryPolicy`.

2. **`ChainSourceError` and `SubmitterError` drop the catchall variants.** `Unavailable { reason }` is retained as the generic Retryable signal for source-agnostic consumers (the testkit mock, any future non-zinder `ChainSource`). `UpstreamFailed { reason, is_retryable }` is deleted. A new variant `Indexer(#[from] IndexerError)` carries the typed zinder error lossless when the `zinder` feature is enabled. Posture is derived via a `posture()` method that delegates to `IndexerError::retry_policy()` for the `Indexer` variant and statically classifies the other variants.

3. **`WalletError::ChainSource` becomes a tuple variant `ChainSource(#[from] ChainSourceError)`.** Same shape for `Submitter(#[from] SubmitterError)`. The `reason: String` and `is_retryable: bool` fields are removed; consumers read the inner error directly. `WalletError::SyncDriverFailed` keeps its `reason` field but replaces `is_retryable: bool` with `posture: FailurePosture` so a panicked driver surfaces as `RequiresOperator` rather than `is_retryable: true | false`.

4. **One typed-error path, no string adapters.** The standalone mapper functions `zinder_chain_source::zinder_error_to_chain_source`, `zinder_submitter::zinder_error_to_submitter_error`, `sync::map_chain_source_error`, `wallet::map_chain_source_to_wallet_error`, `pczt::map_submitter_error`, and `spend::map_submitter_error` are deleted. The `#[from]` attributes on the new tuple variants let `?` carry zinder errors directly to the wallet boundary with the typed `IndexerError` preserved. The lossy `IndexerError::DataLoss` → `MalformedCompactBlock { block_height: BlockHeight::from(0), ... }` placeholder is gone.

5. **`IsRetryable` becomes `HasFailurePosture`.** The retry trait in `zally-wallet::retry` exposes `failure_posture() -> FailurePosture` instead of `is_retryable() -> bool`. `with_retry` retries only on `Retryable`; `with_breaker_and_retry` trips the circuit breaker only on `Retryable` failures. Operator-action and caller-bug failures bypass the breaker since neither is a symptom of a flaky backend.

6. **Boundary-error types keep their bool.** `StorageError`, `SealingError`, `KeyDerivationError`, and `PcztError` retain their `is_retryable()` method because their domains only need a binary retry signal. The wallet-side `HasFailurePosture` impl maps their bool to `{Retryable, NotRetryable}` mechanically.

7. **Testkit injects errors via a closure factory.** `MockChainSource::fail_chain_tip_next` and `MockSubmitter::fail_submit_next` take `impl FnMut() -> ChainSourceError` rather than a single cloneable error. This removes the `Clone` derive requirement on error types (which would otherwise force an `Arc<IndexerError>` indirection because zinder-client's `IndexerError` does not implement `Clone`).

## Consequences

- A new `IndexerError` variant lands in zally with its correct `RetryPolicy`-derived posture as soon as zinder ships it. There is no zally-side adapter to forget.
- Operator dashboards have access to the typed cause (`zinder indexer error: invalid request: …`) instead of the previous adapter-rewritten string (`zinder rejected the request: …`).
- The circuit breaker is no longer poisoned by operator-action failures. A misconfigured upstream (`IndexerError::FailedPrecondition`) or a malformed compact block (`ChainSourceError::MalformedCompactBlock`) keeps the breaker closed; only genuine transient backend trouble flips it open.
- `WalletError::ChainSource` and `WalletError::Submitter` change from struct variants to tuple variants. Consumers that pattern-match must rewrite `Upstream::ChainSource { .. }` to `Upstream::ChainSource(_)`. There is no compatibility shim: the previous shape was the source of the bug class this ADR removes.
- The testkit failure-injection API takes a closure. Call sites change from `fail_chain_tip_next(2, ChainSourceError::Unavailable { reason: "…".into() })` to `fail_chain_tip_next(2, || ChainSourceError::Unavailable { reason: "…".into() })`. New tests can inject distinct errors per attempt without API friction.
- `SyncDriverStatus::Failed { is_retryable: bool }` becomes `SyncDriverStatus::Failed { posture: FailurePosture }`; `SyncErrorSnapshot` carries the same posture.
- `IsRetryable` is removed from the public surface; consumers that took `IsRetryable` as a trait bound import `HasFailurePosture` and call `failure_posture()`.

## Alternatives considered

- **Keep the catchall variants and translate posture in place.** Rejected: this preserves the bool as the load-bearing contract field and keeps zally's surface drifting from zinder-client's typed error. The mapping table would still need to be edited on every new `IndexerError` variant.
- **Re-export `zinder_client::RetryPolicy` directly as `zally_chain::RetryPolicy`.** Rejected: zally already names a struct `RetryPolicy` in `zally_wallet::retry` for retry-attempt configuration. Re-exporting the zinder name would collide on the word "policy" and confuse consumers about what kind of policy they are looking at.
- **Mirror zinder's seven-class `SourceFailureClass` taxonomy.** Rejected: `SourceFailureClass` lives in `zinder-source` (a server-side crate) and is designed for ingest-loop backoff cadence selection. The wallet boundary needs only three classes; the seven labels would add labels the wallet has no policy for.
- **Wrap `IndexerError` in `Arc` to keep `Clone` on `ChainSourceError`.** Rejected after weighing the API ergonomics: `WalletError` does not derive `Clone` either, so cloning was never the cross-cutting invariant the testkit alone implied. Changing the mock factory to a closure is a smaller, more honest API change than adding an `Arc` indirection on every chain error.
