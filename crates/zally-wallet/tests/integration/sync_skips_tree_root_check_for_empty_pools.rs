//! `Wallet::sync` skips the tree-root check for a wallet pool whose commitment tree holds
//! no scanned leaves, instead of faulting `TreeRootsDiverged` against the chain's root.

use zally_core::{BlockHash, BlockHeight, TreeStateArtifact};
use zally_testkit::MockChainSource;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

const CHAIN_TIP: u32 = 42;
const SYNC_STEP_BOUND: u32 = 16;

/// Serialized single-leaf commitment tree in the `z_gettreestate` `finalState` encoding:
/// one filled left leaf, no right leaf, no parents. Its root differs from the empty root.
fn single_leaf_tree_hex() -> String {
    format!("01{}0000", "00".repeat(32))
}

#[tokio::test]
async fn sync_skips_tree_root_check_for_empty_pools() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;

    let chain = MockChainSource::new(wallet.network());
    let handle = chain.handle();
    handle.serve_compact_blocks();
    handle.advance_tip(BlockHeight::from(CHAIN_TIP));
    let nonempty_frontier = hex::decode(single_leaf_tree_hex()).unwrap_or_default();
    handle.serve_tree_state(TreeStateArtifact {
        network: wallet.network(),
        height: BlockHeight::from(CHAIN_TIP),
        block_hash: BlockHash::from_bytes([0; 32]),
        block_time_seconds: 0,
        sapling_final_state_bytes: nonempty_frontier.clone(),
        orchard_final_state_bytes: nonempty_frontier.clone(),
        ironwood_final_state_bytes: nonempty_frontier,
    });

    let mut scanned_to_height = BlockHeight::GENESIS;
    for _ in 0..SYNC_STEP_BOUND {
        scanned_to_height = wallet.sync(&chain).await?.scanned_to_height;
        if scanned_to_height == BlockHeight::from(CHAIN_TIP) {
            break;
        }
    }
    assert_eq!(scanned_to_height, BlockHeight::from(CHAIN_TIP));
    Ok(())
}
