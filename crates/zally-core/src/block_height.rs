//! Zcash block height.

/// A Zcash block height.
///
/// `BlockHeight` is a `u32` newtype. The unit suffix (`_height`) is required on field and
/// parameter names by the Public Interfaces spine so heights are never confused with seconds,
/// counts, or block deltas.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHeight(u32);

impl BlockHeight {
    /// The genesis block height.
    pub const GENESIS: Self = Self(0);

    /// Returns the inner height.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Returns the height that is `delta_blocks` below this one, saturating at genesis.
    #[must_use]
    pub const fn saturating_sub(self, delta_blocks: u32) -> Self {
        Self(self.0.saturating_sub(delta_blocks))
    }
}

impl From<u32> for BlockHeight {
    fn from(height: u32) -> Self {
        Self(height)
    }
}

impl std::fmt::Display for BlockHeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<BlockHeight> for u32 {
    fn from(height: BlockHeight) -> Self {
        height.0
    }
}

impl From<zcash_protocol::consensus::BlockHeight> for BlockHeight {
    fn from(height: zcash_protocol::consensus::BlockHeight) -> Self {
        Self(u32::from(height))
    }
}

impl From<BlockHeight> for zcash_protocol::consensus::BlockHeight {
    fn from(height: BlockHeight) -> Self {
        Self::from_u32(height.as_u32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_height_round_trips_u32() {
        let h = BlockHeight::from(2_500_000_u32);
        assert_eq!(u32::from(h), 2_500_000_u32);
    }

    #[test]
    fn block_height_saturates_at_genesis() {
        let h = BlockHeight::from(5);
        assert_eq!(h.saturating_sub(100), BlockHeight::GENESIS);
    }

    #[test]
    fn block_height_round_trips_zcash_protocol() {
        let h = BlockHeight::from(42_u32);
        let upstream = zcash_protocol::consensus::BlockHeight::from(h);
        assert_eq!(BlockHeight::from(upstream), h);
    }
}
