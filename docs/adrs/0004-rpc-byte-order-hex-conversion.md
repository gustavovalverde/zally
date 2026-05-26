# ADR-0004: RPC Byte Order is the Text Form for Hash Newtypes

| Field   | Value                                                                                                                                                                                                                                                                                                                  |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status  | Accepted on 2026-05-26                                                                                                                                                                                                                                                                                                 |
| Product | Zally                                                                                                                                                                                                                                                                                                                  |
| Domain  | `zally-core` domain types; cross-repo wire-format vocabulary alignment                                                                                                                                                                                                                                                 |
| Related | [Public interfaces §Byte order for hash newtypes](../architecture/public-interfaces.md#byte-order-for-hash-newtypes); [ADR-0001 Workspace crate boundaries](./0001-workspace-crate-boundaries.md); [Zinder ADR-0024 Wire Format Uses RPC Byte Order for Hashes](https://github.com/gustavovalverde/zinder/blob/main/docs/adrs/0024-wire-format-rpc-byte-order.md) |

## Context

A 32-byte Zcash hash (txid, block hash) exists in two byte orders. The first is the raw SHA-256d output, used in consensus serialization and stored verbatim in `[u8; 32]`. The Zcash protocol specification calls this **internal byte order** at protocol.tex:13560-13564. The second is the byte-reversed form every consumer-facing surface uses: `zcash-cli` replies (`getrawtransaction`, `getblock`, `getbestblockhash`), every wallet UI, every block explorer URL, and the protocol specification itself when it prints block hashes. The spec defines this as **RPC byte order** at protocol.tex:1127 (`\newcommand{\rpcByteOrder}{\term{RPC byte order}}`) and uses it in normative sentences such as protocol.tex:4036: *"All block hashes given in this section are in RPC byte order (that is, byte-reversed relative to the normal order for a SHA-256d hash)."* ZIP 308 applies the same term to txid presentation.

Until this change, `zally-core::TxId` was `pub struct TxId([u8; 32])` with only `from_bytes` / `as_bytes`. No `Display`, no `FromStr`, no byte-order conversion. There was also no `BlockHash` newtype anywhere in zally; only `TxId`. Every consumer hand-rolled the conversion:

- fauzec's wallet adapter (`crates/fauzec-wallet/src/librustzcash/mod.rs`) carried `display_txid_from_zally` and `zally_txid_from_display_hex` (40 lines plus tests) that reversed bytes and called `hex::encode` / `hex::decode` inline, returning `WalletFailure::UnexpectedReply` strings instead of typed errors.
- Every future consumer of `zally-core::TxId` would either rediscover the same fix or, more often, ship the same kind of regression Zexplorer hit when its BFF forwarded user-supplied RPC-form hex into a `bytes` field that expected internal-form bytes.

The upstream Zinder repository already moved its public proto contract and its `wire/` Rust helpers to the same vocabulary (`encode_rpc_*_hex` / `decode_rpc_*_hex`, see [Zinder ADR-0024](https://github.com/gustavovalverde/zinder/blob/main/docs/adrs/0024-wire-format-rpc-byte-order.md)). `zally-core` is the natural home for the same vocabulary on the wallet primitive plane: every wallet, every dispense path, every observability surface that reads a Zally `TxId` or `BlockHash` flows through these types eventually.

## Decision

1. **Every `[u8; 32]` hash newtype in `zally-core` ships with the same six-item surface.** Today that means [`TxId`](../../crates/zally-core/src/txid.rs) and [`BlockHash`](../../crates/zally-core/src/block_hash.rs); future siblings (auth digest, wtxid, merkle root) get the same shape. The six items are:
   - `from_bytes([u8; 32]) -> Self` and `as_bytes(&self) -> &[u8; 32]`: the storage and consensus-serialization seam, carrying internal byte order.
   - `to_rpc_hex(&self) -> String` and `from_rpc_hex(&str) -> Result<Self, FromRpcHexError>`: the text seam, carrying RPC byte order (64 lowercase ASCII hex characters).
   - `impl Display`: renders the same 64-character RPC byte order hex form as `to_rpc_hex`.
   - `impl FromStr`: parses the same form, delegating to `from_rpc_hex`.
   - `impl Debug`: renders as `TxId("<rpc-hex>")` / `BlockHash("<rpc-hex>")` so log records and test failures are grep-searchable against any block explorer or `zcash-cli` output.

2. **`FromRpcHexError` is shared across hash newtypes.** Two variants: `InvalidLength { expected, actual }` (rejects every non-64-character input) and `InvalidHex { source: hex::FromHexError }` (rejects non-hex bytes). The error follows the Zally retry-posture convention: both variants carry `not_retryable` rustdoc and `FromRpcHexError::is_retryable` returns false. The type lives in [`crates/zally-core/src/hash_hex.rs`](../../crates/zally-core/src/hash_hex.rs), alongside the private decode/encode primitives `TxId` and `BlockHash` share. A new hash kind imports the shared primitives, not a copy.

3. **Acceptance is case-insensitive; emission is lowercase.** `from_rpc_hex` delegates to `hex::decode_to_slice`, which accepts lowercase, uppercase, and mixed-case hex. `to_rpc_hex` always emits lowercase to match `zcash-cli` and every block explorer convention. The asymmetry is deliberate: producers stay canonical; consumers stay forgiving of clipboard variants.

4. **The bytes form is unchanged.** `from_bytes` / `as_bytes` keep their current `[u8; 32]` shape and current semantics (internal byte order). Adding text helpers does not move the storage or consensus-serialization boundary; the wire-format seam was never the issue. Consumers that already pass bytes between storage and `librustzcash` see no diff.

5. **Serde behaviour is unchanged.** The existing `Serialize` / `Deserialize` derives under the `serde` feature continue to round-trip through the byte array. The new `Display` / `FromStr` give the canonical RPC byte order text form when callers want it; they do not silently change the serialised wire shape. A test in each of `txid.rs` and `block_hash.rs` pins both behaviours (`serde_json` round-trip through bytes plus `to_rpc_hex` equality on the same value).

## Consequences

- **Consumers stop reinventing the conversion.** fauzec's `display_txid_from_zally` and `zally_txid_from_display_hex` become single-line calls (`zally_tx_id.to_rpc_hex()`, `zally_core::TxId::from_rpc_hex(input)`), with typed errors and no string-formatted `WalletFailure::UnexpectedReply` round-trip. Future Zally consumers (zexplorer, zinder integration code, downstream wallets) inherit the same surface.
- **Cross-repo grep on "RPC byte order" lands consistently.** The vocabulary now matches the protocol spec, the Zinder wire helpers (`encode_rpc_*_hex` / `decode_rpc_*_hex`), the Zinder ADR-0024, and Zally's `to_rpc_hex` / `from_rpc_hex`. A reviewer auditing what crosses a wire boundary finds the same term in every layer.
- **Debug stability is not a public-API guarantee, but the `Debug` change is observable.** The old derived `Debug` printed `TxId([0xAB, 0xAB, ...])`; the new impl prints `TxId("ab...")` in RPC byte order. Snapshot tests in downstream consumers that captured the derived form will need a regenerate. This is intentional: the derived form was actively misleading because the byte order it printed was the opposite of every other tool the operator might compare against.
- **Zero runtime cost.** `to_rpc_hex` allocates one 64-byte string (the hex output). `from_rpc_hex` reuses a stack buffer. `Display` writes through the formatter without heap allocation. `from_bytes` / `as_bytes` are unchanged.
- **The new `BlockHash` newtype is additive.** No existing code path took a `BlockHash` from `zally-core`; adding it is purely a new export.

## Alternatives considered

- **Keep the conversion in consumers.** Rejected: the failure mode is the single most common cross-tool footgun in Zcash (a copied txid from a wallet that misses on lookup against a service that stored internal-form bytes). Pushing the conversion outward to every consumer leaves the trap in place. Putting it on the type that owns the data closes the trap once.
- **A free-function `encode_rpc_hex(&TxId) -> String` module.** Rejected: the inherent method composes with `Display` and `FromStr`, which is what idiomatic Rust callers reach for first. A free function would always be a second-best path, and would leave operators wondering whether to use the free function or `format!("{txid}")` (which would otherwise still print the derived debug form). Inherent methods plus `Display` is the convention every well-behaved Rust hash newtype uses (`bitcoin_hashes::Txid`, `librustzcash`'s internal types, etc.).
- **Reject uppercase hex.** Rejected: clipboard sources sometimes uppercase hex (legacy `Bitcoin-Qt` exports, some hardware wallets, doc renderers). The `hex` crate already accepts both at zero cost; the lowercase emission still guarantees a single canonical form on the producer side. The cost of rejecting uppercase would be an avoidable class of operator confusion.
