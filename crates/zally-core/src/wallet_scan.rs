//! Source-neutral wallet scan artifacts.

use crate::{BlockHash, BlockHeight, Network, TxId, Zatoshis};

/// Commitment-tree sizes after one compact block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactChainMetadata {
    /// Sapling note-commitment tree size.
    pub sapling_commitment_tree_size: u32,
    /// Orchard note-commitment tree size.
    pub orchard_commitment_tree_size: u32,
    /// Ironwood note-commitment tree size.
    pub ironwood_commitment_tree_size: u32,
}

/// One compact Sapling spend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactSaplingSpend {
    /// Spend nullifier in consensus byte order.
    pub nullifier_bytes: [u8; 32],
}

/// One compact Sapling output.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactSaplingOutput {
    /// Note commitment in consensus byte order.
    pub commitment_bytes: [u8; 32],
    /// Ephemeral key encoding.
    pub ephemeral_key_bytes: [u8; 32],
    /// First 52 bytes of encrypted note ciphertext.
    #[cfg_attr(feature = "serde", serde(with = "bytes_52"))]
    pub ciphertext_bytes: [u8; 52],
}

/// One compact Orchard or Ironwood action.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactShieldedAction {
    /// Action nullifier in consensus byte order.
    pub nullifier_bytes: [u8; 32],
    /// Note commitment in consensus byte order.
    pub commitment_bytes: [u8; 32],
    /// Ephemeral key encoding.
    pub ephemeral_key_bytes: [u8; 32],
    /// First 52 bytes of encrypted note ciphertext.
    #[cfg_attr(feature = "serde", serde(with = "bytes_52"))]
    pub ciphertext_bytes: [u8; 52],
}

/// One compact transparent input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactTransparentInput {
    /// Transaction that created the spent output.
    pub previous_tx_id: TxId,
    /// Output index in the creating transaction.
    pub previous_output_index: u32,
}

/// One transparent output included in a compact transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactTransparentOutput {
    /// Output value in zatoshis.
    pub value_zat: Zatoshis,
    /// Consensus scriptPubKey bytes.
    pub script_pub_key_bytes: Vec<u8>,
}

/// Wallet-relevant fields from one mined transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactTransaction {
    /// Position within the block.
    pub index: u64,
    /// Consensus transaction identifier.
    pub tx_id: TxId,
    /// Transaction fee when the source can derive it.
    pub fee_zat: Option<Zatoshis>,
    /// Sapling spends in consensus order.
    pub sapling_spends: Vec<CompactSaplingSpend>,
    /// Sapling outputs in consensus order.
    pub sapling_outputs: Vec<CompactSaplingOutput>,
    /// Orchard actions in consensus order.
    pub orchard_actions: Vec<CompactShieldedAction>,
    /// Ironwood actions in consensus order.
    pub ironwood_actions: Vec<CompactShieldedAction>,
    /// Transparent inputs in consensus order.
    pub transparent_inputs: Vec<CompactTransparentInput>,
    /// Transparent outputs in consensus order.
    pub transparent_outputs: Vec<CompactTransparentOutput>,
}

/// Source-authenticated compact block consumed by wallet scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompactBlockArtifact {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash in consensus byte order.
    pub block_hash: BlockHash,
    /// Parent block hash in consensus byte order.
    pub previous_block_hash: BlockHash,
    /// Block time in Unix seconds.
    pub block_time_seconds: u32,
    /// Wallet-relevant transactions in block order.
    pub transactions: Vec<CompactTransaction>,
    /// Commitment-tree sizes after the block.
    pub chain_metadata: CompactChainMetadata,
}

/// Source-authenticated commitment-tree frontiers after one block.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TreeStateArtifact {
    /// Network authenticated by the source.
    pub network: Network,
    /// Height of the block that produced these frontiers.
    pub height: BlockHeight,
    /// Hash of the block that produced these frontiers.
    pub block_hash: BlockHash,
    /// Block time in Unix seconds.
    pub block_time_seconds: u32,
    /// Encoded Sapling frontier, empty only before activation.
    pub sapling_final_state_bytes: Vec<u8>,
    /// Encoded Orchard frontier, empty only before activation.
    pub orchard_final_state_bytes: Vec<u8>,
    /// Encoded Ironwood frontier, empty only before activation.
    pub ironwood_final_state_bytes: Vec<u8>,
}

#[cfg(feature = "serde")]
mod bytes_52 {
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S>(bytes: &[u8; 52], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(bytes)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 52], D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| serde::de::Error::invalid_length(bytes.len(), &"52 bytes"))
    }
}
