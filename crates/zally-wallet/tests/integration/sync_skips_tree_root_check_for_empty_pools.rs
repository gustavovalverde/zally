//! `Wallet::sync` skips the tree-root check for a wallet pool whose commitment tree holds
//! no scanned leaves, instead of faulting `TreeRootsDiverged` against the chain's root.

use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zcash_client_backend::proto::service::TreeState;

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
    handle.serve_tree_state(TreeState {
        network: String::new(),
        height: u64::from(CHAIN_TIP),
        hash: "00".repeat(32),
        time: 0,
        sapling_tree: String::new(),
        orchard_tree: single_leaf_tree_hex(),
        ironwood_tree: String::new(),
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
