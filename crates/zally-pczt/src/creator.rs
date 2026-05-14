//! `Creator` role: wraps a pre-built `pczt::Pczt` as a network-tagged [`PcztBytes`].
//!
//! Operators construct the inner `pczt::Pczt` through `pczt::roles::creator::Creator` or
//! through `Wallet::propose_pczt`, which composes the upstream `create_pczt_from_proposal`
//! flow.

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
    /// Entry point for callers (typically `Wallet::propose_pczt`) that have constructed a
    /// `pczt::Pczt` via the upstream `pczt::roles::creator::Creator`.
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
