//! Operator-defined receiver purpose vocabulary.

/// Purpose for which an operator allocates a receiver inside their wallet.
///
/// Used to configure confirmation depth per receiver: coinbase receives need 100-block
/// maturity per ZIP-213; other receives may use a smaller depth.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ReceiverPurpose {
    /// Mining-pool coinbase. ZIP-213 mandates 100-block confirmation depth.
    Mining,
    /// Donation receive.
    Donations,
    /// Hot-dispense (faucet payouts, exchange withdrawals).
    HotDispense,
    /// Cold-reserve / treasury.
    ColdReserve,
    /// Operator-defined identifier.
    Custom(String),
}

impl ReceiverPurpose {
    /// Default confirmation depth in blocks for this purpose.
    ///
    /// `Mining` returns 100 (ZIP-213). All other purposes return 1. Operators may override.
    #[must_use]
    pub fn default_confirmation_depth_blocks(&self) -> u32 {
        match self {
            Self::Mining => 100,
            Self::Donations | Self::HotDispense | Self::ColdReserve | Self::Custom(_) => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mining_defaults_to_zip_213_depth() {
        assert_eq!(
            ReceiverPurpose::Mining.default_confirmation_depth_blocks(),
            100
        );
    }

    #[test]
    fn non_mining_defaults_to_one_block() {
        assert_eq!(
            ReceiverPurpose::Donations.default_confirmation_depth_blocks(),
            1
        );
        assert_eq!(
            ReceiverPurpose::Custom("treasury".into()).default_confirmation_depth_blocks(),
            1
        );
    }
}
