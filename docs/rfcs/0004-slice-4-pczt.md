# RFC 0004: Slice 4 — PCZT Roles

| Field | Value |
|---|---|
| Status | Accepted |
| Product | Zally |
| Slice | 4 |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [ADR-0001](../adrs/0001-workspace-crate-boundaries.md), [RFC-0003](0003-slice-3-spend.md) |
| Created | 2026-05-13 |

## Summary

Slice 4 lands `zally-pczt`, the typed wrapper around the upstream `pczt` crate that exposes the four operator-facing roles Zally guarantees in v1: `Creator`, `Signer`, `Combiner`, `Extractor`. The slice satisfies REQ-PCZT-1 through REQ-PCZT-5: build an unsigned PCZT from a Zally `Proposal`, serialise it to bytes for transport to an HSM / FROST coordinator / air-gapped signer, sign in-process (USK derived from sealed seed at signing time, zeroized after), combine multiple signed PCZTs, and extract the final transaction for `Submitter::submit`.

Slice 4 also exposes the spend-side bridge on `Wallet`: `Wallet::propose_pczt`, `Wallet::sign_pczt`, `Wallet::combine_pczts`, `Wallet::extract_and_submit_pczt`. ADR-0001's "speculative" `zally-pczt` bet is validated as the operator-facing crate that wallet-services and signer-only services both depend on.

The end-to-end PCZT flow against a live wallet requires Slice 5's real proposal construction; Slice 4 ships the trait surface and the wrap, with tests against fixture PCZTs.

---

## 1. Crate layout (`zally-pczt`)

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/creator.rs` | `Creator`, `Creator::for_proposal` |
| `src/signer.rs` | `Signer`, `Signer::new`, `Signer::sign_with_seed` |
| `src/combiner.rs` | `Combiner`, `Combiner::combine` |
| `src/extractor.rs` | `Extractor`, `Extractor::extract`, `ExtractedTransaction` |
| `src/pczt_bytes.rs` | `PcztBytes` wrapper, `PcztBytes::serialize`, `PcztBytes::parse` |
| `src/pczt_error.rs` | `PcztError` |

Cargo features:

- `serde` — gates serde derives on public types where applicable.

Dependencies: `pczt` (with `signer`, `prover` features), `zally-core`, `zally-keys`, `zcash_client_backend`, `zcash_primitives`, `zcash_protocol`, `thiserror`, `async-trait`.

---

## 2. Public surface

### 2.1 `PcztBytes`

```rust
/// Serialised PCZT bytes. Carries the network for cross-role validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcztBytes {
    bytes: Vec<u8>,
    network: zally_core::Network,
}

impl PcztBytes {
    /// Wraps already-serialised bytes plus the network the PCZT is for.
    #[must_use]
    pub fn from_serialized(bytes: Vec<u8>, network: zally_core::Network) -> Self;

    /// Returns the wire bytes for transport.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8];

    /// Returns the network this PCZT is bound to.
    #[must_use]
    pub fn network(&self) -> zally_core::Network;
}
```

### 2.2 `Creator`

```rust
/// Builds a PCZT from a Zally `Proposal`.
pub struct Creator { /* private */ }

impl Creator {
    pub fn for_proposal(
        proposal: &zally_wallet::Proposal,
        network: zally_core::Network,
    ) -> Result<PcztBytes, PcztError>;
}
```

### 2.3 `Signer`

```rust
/// Signs a PCZT using the sealed seed held by the wallet handle.
pub struct Signer { /* private */ }

impl Signer {
    /// Constructs a signer for the given network. Constructor validates network match
    /// against the PCZT bytes' embedded network later, at `sign_with_seed` call time.
    pub fn new(network: zally_core::Network) -> Self;

    /// Signs the PCZT using `seed`. The USK is derived inside this call, used to sign
    /// every Sapling / Orchard / transparent input the seed controls, and zeroized before
    /// the function returns.
    pub async fn sign_with_seed(
        &self,
        pczt: PcztBytes,
        seed: &zally_keys::SeedMaterial,
    ) -> Result<PcztBytes, PcztError>;
}
```

### 2.4 `Combiner`

```rust
/// Merges multiple signed PCZTs (e.g., signatures from multiple FROST quorum members).
pub struct Combiner;

impl Combiner {
    pub fn combine(pczts: Vec<PcztBytes>) -> Result<PcztBytes, PcztError>;
}
```

### 2.5 `Extractor`

```rust
/// Extracts the final transaction bytes from a fully-signed PCZT.
pub struct Extractor;

#[derive(Clone, Debug)]
pub struct ExtractedTransaction {
    pub raw_bytes: Vec<u8>,
    pub tx_id: zally_core::TxId,
    pub network: zally_core::Network,
}

impl Extractor {
    pub fn extract(pczt: PcztBytes) -> Result<ExtractedTransaction, PcztError>;
}
```

### 2.6 `PcztError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PcztError {
    /// not_retryable: malformed PCZT bytes.
    #[error("PCZT parse failed: {reason}")]
    ParseFailed { reason: String },

    /// requires_operator: network mismatch between PCZT and the configured caller.
    #[error("PCZT network mismatch: pczt={pczt:?}, configured={configured:?}")]
    NetworkMismatch {
        pczt: zally_core::Network,
        configured: zally_core::Network,
    },

    /// not_retryable: signing role requires inputs that none of the seed-derived keys can sign.
    #[error("PCZT signing role: no keys derivable from the supplied seed match any of the inputs")]
    NoMatchingKeys,

    /// requires_operator: the PCZT is not finalised; `Extractor` cannot run.
    #[error("PCZT is not finalised; signer/combiner roles must run first: {reason}")]
    NotFinalized { reason: String },

    /// not_retryable: combiner saw PCZTs that disagree on inputs/outputs.
    #[error("PCZTs cannot be combined: {reason}")]
    CombineConflict { reason: String },

    /// Posture varies; the field disambiguates.
    #[error("upstream PCZT error: {reason}")]
    UpstreamFailed { reason: String, is_retryable: bool },
}

impl PcztError {
    pub const fn is_retryable(&self) -> bool { /* uniform per-variant per ADR-0002 Decision 5 */ }
}
```

### 2.7 Wallet-side surface

```rust
impl Wallet {
    /// Builds an unsigned PCZT from a `ProposalPlan`. Honors the same ZIP-302/320 guards as
    /// `Wallet::propose` and returns a `PcztBytes` ready for transport.
    pub async fn propose_pczt(&self, plan: ProposalPlan) -> Result<PcztBytes, WalletError>;

    /// Signs a PCZT in-process. The wallet's sealed seed is unsealed inside the call and the
    /// resulting USK is zeroized before return.
    pub async fn sign_pczt(&self, pczt: PcztBytes) -> Result<PcztBytes, WalletError>;

    /// Combines multiple PCZTs (e.g., from a FROST quorum).
    pub async fn combine_pczts(&self, pczts: Vec<PcztBytes>) -> Result<PcztBytes, WalletError>;

    /// Extracts the final transaction and submits it via `submitter`. Returns a `SendOutcome`.
    pub async fn extract_and_submit_pczt(
        &self,
        pczt: PcztBytes,
        submitter: &dyn Submitter,
    ) -> Result<SendOutcome, WalletError>;
}
```

`WalletError` gains a `Pczt(#[from] PcztError)` variant.

### 2.8 Capability additions

`Capability::PcztV06` (matching `pczt = 0.6.x`).

---

## 3. Cross-role network validation

Per ADR-0002 OQ-4: `pczt`'s `Global.coin_type` is a `u32` with no Zally-typed network check. `Signer::new(network)` and `Extractor::extract` both validate the PCZT's `coin_type` against the configured `network` and return `PcztError::NetworkMismatch` on mismatch. The validation happens before any signature application, so a misrouted PCZT is rejected before the USK is derived.

---

## 4. Tests

T0 unit:
- `pczt_bytes_round_trip` — serialize then parse yields bit-identical bytes.
- `signer_rejects_mismatched_network` — a mainnet `Signer` rejects a testnet PCZT before unsealing.
- `extractor_rejects_unfinalized` — extractor errors on a PCZT that still has unsigned spends.
- `pczt_error_retryable_match_complete`.

T1 integration:
- `pczt_round_trip_creator_signer_extractor.rs` — build a fixture proposal (Slice 5 dep stubs to `WalletError::InsufficientBalance` for now), assert the propose-pczt path emits `ProposalRejected` (the surface is exercised; live signing is deferred).
- `pczt_combine_two_signers.rs` — two `Signer::sign_with_seed` outputs, combined by `Combiner::combine`, parse without error.

T3 live:
- `live_pczt_round_trip_against_zinder_regtest.rs` — gated by `ZALLY_TEST_LIVE=1`; ignored until live infra lands.

---

## 5. Open questions

### OQ-1: USK in PCZT signer

Sapling and Orchard spend authorization signatures need the spend authorization key, not the full USK. The Zally `Signer::sign_with_seed` derives the USK in-call, pulls the per-pool authorization keys, signs every spend the seed controls, and zeroizes the USK + the authorization keys before the function returns. The duration the secret material lives in memory is bounded by the `sign_with_seed` async body, which runs entirely inside `tokio::task::spawn_blocking`. **Decision lean: ship this shape.**

### OQ-2: FROST integration

PRD-0001 REQ-PCZT design hooks call out ZIP-312 (FROST). Slice 4 does not ship FROST; the `Signer` trait shape is single-key. A FROST-aware signer lands as an alternative impl in `zally-pczt` once ZIP-312 is ratified. The `Combiner` role already supports multi-signature combination. **Decision: defer.**

### OQ-3: Verifier role

The upstream `pczt::roles::verifier` validates a PCZT's structural integrity. Slice 4 calls `Verifier` internally inside `Signer` and `Extractor` for cross-role safety; it is not exposed as a public Zally role. **Decision: hidden inside the wrap.**

---

## 6. Acceptance

1. RFC-0004 accepted (three open questions resolved).
2. Implementation builds against this RFC; T0 + T1 tests pass.
3. Slice 4 PR cites RFC-0004.

---

## 7. Follow-up

- ADR-0001's "speculative bet" on `zally-pczt` is *validated* by Slice 4 landing the surface here. Document the validation in a follow-up ADR amendment once a real signer-only consumer lands.
