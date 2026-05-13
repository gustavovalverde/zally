//! Live [`ZinderChainSource`] integration test.
//!
//! Gated behind `ZALLY_TEST_LIVE=1`. Connects to the zinder-query endpoint from
//! `ZINDER_ENDPOINT`, asserts the chain tip is positive, and asserts at least one
//! compact block is returned for a small range at the tip.
//!
//! Run with:
//! ```sh
//! ZALLY_TEST_LIVE=1 \
//!   ZALLY_NETWORK=regtest \
//!   ZINDER_ENDPOINT=http://127.0.0.1:9101 \
//!   cargo nextest run --profile=ci-live --features zinder --run-ignored=all \
//!     -p zally-chain --test live_zinder_chain_source
//! ```

#![cfg(feature = "zinder")]

use futures_util::StreamExt as _;
use zally_chain::{
    BlockHeightRange, ChainSource, ChainSourceError, ZinderChainSource, ZinderRemoteOptions,
};
use zally_core::BlockHeight;
use zally_testkit::{LiveTestError, init, require_live, require_network, require_zinder_endpoint};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn zinder_chain_source_reports_tip_and_streams_blocks() -> Result<(), TestError> {
    let _guard = init();
    require_live()?;

    let network = require_network()?;
    let endpoint = require_zinder_endpoint()?;

    let chain =
        ZinderChainSource::connect_remote(ZinderRemoteOptions { endpoint, network }).await?;

    let tip = chain.chain_tip().await?;
    assert!(tip.as_u32() > 0, "live zinder reported tip height 0");

    let span = 5_u32.min(tip.as_u32().saturating_sub(1));
    let start = BlockHeight::from(tip.as_u32().saturating_sub(span));
    let range = BlockHeightRange::new(start, tip).ok_or(TestError::RangeOrder { start, tip })?;

    let mut stream = chain.compact_blocks(range).await?;

    let mut count = 0_usize;
    while let Some(block) = stream.next().await {
        block?;
        count += 1;
    }
    assert!(count > 0, "compact_blocks returned an empty stream at tip");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("live gate error: {0}")]
    Live(#[from] LiveTestError),
    #[error("chain source error: {0}")]
    Chain(#[from] ChainSourceError),
    #[error("invalid range: start={start:?}, tip={tip:?}")]
    RangeOrder {
        start: BlockHeight,
        tip: BlockHeight,
    },
}
