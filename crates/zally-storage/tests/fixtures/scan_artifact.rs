#![allow(
    clippy::expect_used,
    reason = "checked-in protocol fixtures must have exact field widths"
)]
#![allow(
    dead_code,
    reason = "each integration-test crate uses a different subset of shared fixture conversions"
)]

use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactSaplingOutput,
    CompactSaplingSpend, CompactShieldedAction, CompactTransaction, CompactTransparentInput,
    CompactTransparentOutput, Network, TreeStateArtifact, TxId, Zatoshis,
};
use zcash_client_backend::proto::compact_formats::{CompactBlock, CompactTx};
use zcash_client_backend::proto::service::TreeState;

pub(crate) fn genesis_tree_state(
    network: Network,
    height: u32,
    hash_bytes: [u8; 32],
) -> TreeStateArtifact {
    TreeStateArtifact {
        network,
        height: BlockHeight::from(height),
        block_hash: BlockHash::from_bytes(hash_bytes),
        block_time_seconds: 0,
        sapling_final_state_bytes: Vec::new(),
        orchard_final_state_bytes: Vec::new(),
        ironwood_final_state_bytes: Vec::new(),
    }
}

pub(crate) fn tree_state_from_upstream(
    network: Network,
    tree_state: TreeState,
) -> TreeStateArtifact {
    TreeStateArtifact {
        network,
        height: BlockHeight::from(u32::try_from(tree_state.height).expect("height fits u32")),
        block_hash: tree_state.hash.parse().expect("fixture hash is RPC hex"),
        block_time_seconds: tree_state.time,
        sapling_final_state_bytes: hex::decode(tree_state.sapling_tree)
            .expect("fixture Sapling frontier is hex"),
        orchard_final_state_bytes: hex::decode(tree_state.orchard_tree)
            .expect("fixture Orchard frontier is hex"),
        ironwood_final_state_bytes: hex::decode(tree_state.ironwood_tree)
            .expect("fixture Ironwood frontier is hex"),
    }
}

pub(crate) fn compact_block_from_upstream(block: CompactBlock) -> CompactBlockArtifact {
    CompactBlockArtifact {
        height: BlockHeight::from(u32::try_from(block.height).expect("height fits u32")),
        block_hash: BlockHash::from_bytes(block.hash.try_into().expect("block hash is 32 bytes")),
        previous_block_hash: BlockHash::from_bytes(
            block.prev_hash.try_into().expect("parent hash is 32 bytes"),
        ),
        block_time_seconds: block.time,
        transactions: block
            .vtx
            .into_iter()
            .map(compact_transaction_from_upstream)
            .collect(),
        chain_metadata: block.chain_metadata.map_or(
            CompactChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            },
            |metadata| CompactChainMetadata {
                sapling_commitment_tree_size: metadata.sapling_commitment_tree_size,
                orchard_commitment_tree_size: metadata.orchard_commitment_tree_size,
                ironwood_commitment_tree_size: metadata.ironwood_commitment_tree_size,
            },
        ),
    }
}

fn compact_transaction_from_upstream(transaction: CompactTx) -> CompactTransaction {
    CompactTransaction {
        index: transaction.index,
        tx_id: TxId::from_bytes(transaction.txid.try_into().expect("txid is 32 bytes")),
        fee_zat: (transaction.fee != 0)
            .then(|| Zatoshis::try_from(u64::from(transaction.fee)).expect("fee is valid")),
        sapling_spends: transaction
            .spends
            .into_iter()
            .map(|spend| CompactSaplingSpend {
                nullifier_bytes: spend.nf.try_into().expect("nullifier is 32 bytes"),
            })
            .collect(),
        sapling_outputs: transaction
            .outputs
            .into_iter()
            .map(|output| CompactSaplingOutput {
                commitment_bytes: output.cmu.try_into().expect("commitment is 32 bytes"),
                ephemeral_key_bytes: output
                    .ephemeral_key
                    .try_into()
                    .expect("ephemeral key is 32 bytes"),
                ciphertext_bytes: output
                    .ciphertext
                    .try_into()
                    .expect("ciphertext is 52 bytes"),
            })
            .collect(),
        orchard_actions: transaction
            .actions
            .into_iter()
            .map(compact_action_from_upstream)
            .collect(),
        ironwood_actions: transaction
            .ironwood_actions
            .into_iter()
            .map(compact_action_from_upstream)
            .collect(),
        transparent_inputs: transaction
            .vin
            .into_iter()
            .map(|input| CompactTransparentInput {
                previous_tx_id: TxId::from_bytes(
                    input
                        .prevout_txid
                        .try_into()
                        .expect("prevout txid is 32 bytes"),
                ),
                previous_output_index: input.prevout_index,
            })
            .collect(),
        transparent_outputs: transaction
            .vout
            .into_iter()
            .map(|output| CompactTransparentOutput {
                value_zat: Zatoshis::try_from(output.value).expect("output is valid"),
                script_pub_key_bytes: output.script_pub_key,
            })
            .collect(),
    }
}

fn compact_action_from_upstream(
    action: zcash_client_backend::proto::compact_formats::CompactOrchardAction,
) -> CompactShieldedAction {
    CompactShieldedAction {
        nullifier_bytes: action.nullifier.try_into().expect("nullifier is 32 bytes"),
        commitment_bytes: action.cmx.try_into().expect("commitment is 32 bytes"),
        ephemeral_key_bytes: action
            .ephemeral_key
            .try_into()
            .expect("ephemeral key is 32 bytes"),
        ciphertext_bytes: action
            .ciphertext
            .try_into()
            .expect("ciphertext is 52 bytes"),
    }
}
