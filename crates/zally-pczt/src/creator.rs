//! `Creator` role: builds an unsigned PCZT from a Zally proposal.
//!
//! Slice 4 ships the role wrapper. Real proposal-to-PCZT construction lands in Slice 5
//! when balance and note selection are in place; until then the wallet-side helper
//! `Wallet::propose_pczt` short-circuits with `WalletError::ProposalRejected` per
//! RFC-0003 §5 OQ-1.

use zally_core::Network;

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

/// Builds a PCZT from a Zally proposal.
#[derive(Debug)]
pub struct Creator {
    network: Network,
}

impl Creator {
    /// Constructs a creator for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self { network }
    }

    /// Returns the network this creator is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    /// Wraps an already-built `pczt::Pczt` as `PcztBytes` for this network.
    ///
    /// Slice 4 exposes this as the entry point for callers (typically `Wallet::propose_pczt`)
    /// that have constructed a `pczt::Pczt` via the upstream `pczt::roles::creator::Creator`.
    #[must_use]
    pub fn wrap(&self, pczt: &pczt::Pczt) -> PcztBytes {
        PcztBytes::from_pczt(pczt, self.network)
    }
}

impl PcztError {
    /// Returns a `NetworkMismatch` error from the embedded vs configured network.
    pub(crate) fn mismatch(pczt_network: Network, configured_network: Network) -> Self {
        Self::NetworkMismatch {
            pczt_network,
            configured_network,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creator_round_trips_network() {
        let creator = Creator::new(Network::regtest_all_at_genesis());
        assert_eq!(creator.network(), Network::regtest_all_at_genesis());
    }
}
