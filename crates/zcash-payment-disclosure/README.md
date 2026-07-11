# Zcash Payment Disclosure

This crate incubates payment-disclosure production, encoding, and verification while
[ZIP-311](https://zips.z.cash/zip-0311) remains a draft. It is independent of Zally wallet,
storage, key-sealing, and chain-source types so the protocol implementation can move upstream
without pulling those product boundaries with it.

## Draft1 profile

ZIP-311 defines the Sapling proof and verification algorithms but leaves versioning and wire
encoding as TODOs. `Zip311Draft1` is therefore an explicit experimental profile, not a claim
that the following encoding is standardized by the ZIP.

The canonical signed encoding is:

| Field | Encoding |
|-------|----------|
| profile | `0x01` |
| transaction ID | 32 bytes in RPC/display order |
| message | canonical `CompactSize` byte length, then bytes |
| transparent input count | canonical `CompactSize`; must be zero |
| Sapling spend count | canonical `CompactSize` |
| each Sapling spend | index as `CompactSize`, `cv[32]`, `rk[32]`, spend proof `[192]`, address-proof marker `0x00`, spend authorization signature `[64]` |
| Sapling output count | canonical `CompactSize` |
| each Sapling output | index as `CompactSize`, outgoing cipher key `[32]` |

The unsigned encoding omits each spend authorization signature but is otherwise identical.
The signature digest is the ZIP-311 BLAKE2b-256 digest with personalization
`ZIP311Signed || coinType_LE32`.

Draft1 requires strictly increasing spend and output indices, at least one Sapling spend,
messages no larger than 65,535 bytes, and no more than 4,096 entries in either sequence. It
does not support transparent inputs, ZIP-304 address proofs, Orchard, or Ironwood.

Zally's adapter can select a Sapling output addressed either to a bare Sapling address or to a
Unified Address containing the same Sapling receiver. The portable evidence always exposes the
recovered Sapling receiver.

Consumers must select this profile explicitly and treat profile bytes they do not understand
as unsupported. A future ZIP encoding gets a new profile; it does not silently reinterpret
Draft1 bytes.

## Zally Ironwood profile

`ZallyIronwood` is an explicitly nonstandard extension for transactions that use the Ironwood
pool after NU6.3 activation. ZIP-311 does not currently define Ironwood disclosures, so this
profile uses byte `0x02` and must never be presented as ZIP-311 compliance. It can be ported or
replaced independently when an upstream specification matures.

The profile shares the transaction ID, message, and network-bound digest header with Draft1. Its
body contains a canonical sequence of Ironwood action indices and a message-bound RedPallas spend
authorization signature for each real spend, followed by selected action indices and outgoing
cipher keys for output recovery. The unsigned encoding omits the signatures.

Ironwood does not recreate the Sapling Groth16 disclosure proof. The mined v6 transaction's
consensus-verified Ironwood proof already binds each action's randomized verification key to its
nullifier. Production uses the retained PCZT randomizer and wallet spend-authorizing key to
reproduce that exact randomized key, then signs the disclosure digest. Verification checks the
new signature against the key in the mined action and recovers the selected output with its
outgoing cipher key. Dummy padding actions are not disclosed as spends.

This chain-anchored construction means verification requires the canonical mined transaction. It
authenticates the payer's authority over the real Ironwood spends and discloses the selected
recipient, amount, and memo without inventing a second proof system outside the current protocol.
