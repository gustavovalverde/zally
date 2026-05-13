//! In-memory [`BlockSource`] adapter built from a vector of compact blocks.
//!
//! Zally's [`ChainSource`] streams compact blocks asynchronously, but
//! [`zcash_client_backend::data_api::chain::scan_cached_blocks`] consumes a synchronous
//! [`BlockSource`]. `BufferedBlockSource` is the bridge: callers fetch compact blocks via
//! `ChainSource::compact_blocks`, drain them into a `Vec<CompactBlock>`, and hand the
//! vector to the scanner through this adapter.

use zcash_client_backend::data_api::chain::{BlockSource, error::Error as ScanError};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_protocol::consensus::BlockHeight as ProtocolBlockHeight;

/// In-memory block source that yields compact blocks in ascending height order.
///
/// The vector is consumed in `with_blocks` calls; consumers that re-scan must re-buffer.
#[derive(Debug)]
pub struct BufferedBlockSource {
    blocks: Vec<CompactBlock>,
}

impl BufferedBlockSource {
    /// Constructs a buffered source from `blocks`. Blocks must already be sorted in
    /// ascending height order; the scanner relies on this ordering.
    #[must_use]
    pub fn new(blocks: Vec<CompactBlock>) -> Self {
        Self { blocks }
    }

    /// Returns the number of buffered blocks.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

/// Error variant returned by [`BufferedBlockSource`]. Always `Infallible` because the
/// source is in-memory; the variant exists so `BlockSource::Error` has a concrete type.
#[derive(Debug, thiserror::Error)]
pub enum BufferedBlockSourceError {
    /// Unreachable; kept to give the `BlockSource::Error` type a name.
    #[error("buffered block source has no error variants")]
    Unreachable,
}

impl BlockSource for BufferedBlockSource {
    type Error = BufferedBlockSourceError;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<ProtocolBlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), ScanError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ScanError<WalletErrT, Self::Error>>,
    {
        let mut taken = 0_usize;
        let cap = limit.unwrap_or(usize::MAX);
        for block in &self.blocks {
            if let Some(start) = from_height {
                let block_height =
                    ProtocolBlockHeight::from_u32(u32::try_from(block.height).unwrap_or(u32::MAX));
                if block_height < start {
                    continue;
                }
            }
            if taken >= cap {
                break;
            }
            with_block(block.clone())?;
            taken += 1;
        }
        Ok(())
    }
}
