//! Zcash network variant and the opaque consensus-parameters wrapper Zally hands to
//! librustzcash.

use zcash_protocol::consensus::{
    MainNetwork, NetworkType, NetworkUpgrade, Parameters, TestNetwork,
};
use zcash_protocol::local_consensus::LocalNetwork;

use crate::block_height::BlockHeight;

/// Zcash network variant.
///
/// `Regtest` carries [`zcash_protocol::local_consensus::LocalNetwork`] directly, which records
/// per-upgrade activation heights as `Option<BlockHeight>`. Use [`Network::regtest_all_at_genesis`]
/// for the common "every upgrade active at height 1" regtest topology.
///
/// Every public type that names an address, key, balance, or transaction carries a `Network`
/// value. Constructors that touch chain state fail closed on network mismatch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "NetworkWire", from = "NetworkWire"))]
#[non_exhaustive]
pub enum Network {
    /// The Zcash main network.
    Mainnet,
    /// The Zcash test network.
    Testnet,
    /// A local regtest network with operator-controlled activation heights.
    Regtest(LocalNetwork),
}

impl Network {
    /// A regtest network with every upgrade activated at height 1.
    ///
    /// Matches the common `nuparams=...:1` regtest configuration.
    #[must_use]
    pub const fn regtest_all_at_genesis() -> Self {
        let height = zcash_protocol::consensus::BlockHeight::from_u32(1);
        Self::Regtest(LocalNetwork {
            overwinter: Some(height),
            sapling: Some(height),
            blossom: Some(height),
            heartwood: Some(height),
            canopy: Some(height),
            nu5: Some(height),
            nu6: Some(height),
            nu6_1: Some(height),
        })
    }

    /// Returns the opaque [`NetworkParameters`] this network maps to.
    ///
    /// Treat the return value as opaque: pass it where a `Parameters` bound is required;
    /// do not match on its internals.
    #[must_use]
    pub fn to_parameters(self) -> NetworkParameters {
        let inner = match self {
            Self::Mainnet => NetworkParametersInner::Mainnet,
            Self::Testnet => NetworkParametersInner::Testnet,
            Self::Regtest(local) => NetworkParametersInner::Regtest(local),
        };
        NetworkParameters { inner }
    }

    /// SLIP-44 coin type. `133` for mainnet; `1` for testnet and regtest.
    #[must_use]
    pub const fn coin_type(self) -> u32 {
        match self {
            Self::Mainnet => 133,
            Self::Testnet | Self::Regtest(_) => 1,
        }
    }
}

/// Opaque Zcash consensus parameters.
///
/// Implements [`zcash_protocol::consensus::Parameters`] by delegating to the variant carried
/// by [`Network`]. Construct via [`Network::to_parameters`].
#[derive(Clone, Copy, Debug)]
pub struct NetworkParameters {
    inner: NetworkParametersInner,
}

#[derive(Clone, Copy, Debug)]
enum NetworkParametersInner {
    Mainnet,
    Testnet,
    Regtest(LocalNetwork),
}

impl Parameters for NetworkParameters {
    fn network_type(&self) -> NetworkType {
        match self.inner {
            NetworkParametersInner::Mainnet => MainNetwork.network_type(),
            NetworkParametersInner::Testnet => TestNetwork.network_type(),
            NetworkParametersInner::Regtest(local) => local.network_type(),
        }
    }

    fn activation_height(
        &self,
        nu: NetworkUpgrade,
    ) -> Option<zcash_protocol::consensus::BlockHeight> {
        match self.inner {
            NetworkParametersInner::Mainnet => MainNetwork.activation_height(nu),
            NetworkParametersInner::Testnet => TestNetwork.activation_height(nu),
            NetworkParametersInner::Regtest(local) => local.activation_height(nu),
        }
    }
}

impl NetworkParameters {
    /// Returns the Sapling activation height for this network, if set.
    ///
    /// Convenience accessor for callers (notably storage backends) that need to construct a
    /// `ChainState` near Sapling activation.
    #[must_use]
    pub fn sapling_activation_height(&self) -> Option<BlockHeight> {
        self.activation_height(NetworkUpgrade::Sapling)
            .map(BlockHeight::from)
    }
}

// Serde wire format. `LocalNetwork` is not serde-deriving even with `local-consensus`, so
// `Network` round-trips through `NetworkWire` whenever the `serde` feature is enabled.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NetworkWire {
    Mainnet,
    Testnet,
    Regtest(RegtestActivations),
}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RegtestActivations {
    overwinter: Option<u32>,
    sapling: Option<u32>,
    blossom: Option<u32>,
    heartwood: Option<u32>,
    canopy: Option<u32>,
    nu5: Option<u32>,
    nu6: Option<u32>,
    nu6_1: Option<u32>,
}

#[cfg(feature = "serde")]
impl From<Network> for NetworkWire {
    fn from(network: Network) -> Self {
        match network {
            Network::Mainnet => Self::Mainnet,
            Network::Testnet => Self::Testnet,
            Network::Regtest(local) => Self::Regtest(RegtestActivations {
                overwinter: local.overwinter.map(u32::from),
                sapling: local.sapling.map(u32::from),
                blossom: local.blossom.map(u32::from),
                heartwood: local.heartwood.map(u32::from),
                canopy: local.canopy.map(u32::from),
                nu5: local.nu5.map(u32::from),
                nu6: local.nu6.map(u32::from),
                nu6_1: local.nu6_1.map(u32::from),
            }),
        }
    }
}

#[cfg(feature = "serde")]
impl From<NetworkWire> for Network {
    fn from(wire: NetworkWire) -> Self {
        match wire {
            NetworkWire::Mainnet => Self::Mainnet,
            NetworkWire::Testnet => Self::Testnet,
            NetworkWire::Regtest(activations) => Self::Regtest(LocalNetwork {
                overwinter: activations
                    .overwinter
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                sapling: activations
                    .sapling
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                blossom: activations
                    .blossom
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                heartwood: activations
                    .heartwood
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                canopy: activations
                    .canopy
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                nu5: activations
                    .nu5
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                nu6: activations
                    .nu6
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
                nu6_1: activations
                    .nu6_1
                    .map(zcash_protocol::consensus::BlockHeight::from_u32),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regtest_all_at_genesis_activates_every_upgrade_at_height_one() {
        let params = Network::regtest_all_at_genesis().to_parameters();
        assert_eq!(
            params.activation_height(NetworkUpgrade::Sapling),
            Some(zcash_protocol::consensus::BlockHeight::from_u32(1))
        );
        assert_eq!(
            params.activation_height(NetworkUpgrade::Nu5),
            Some(zcash_protocol::consensus::BlockHeight::from_u32(1))
        );
    }

    #[test]
    fn coin_type_matches_slip_44() {
        assert_eq!(Network::Mainnet.coin_type(), 133);
        assert_eq!(Network::Testnet.coin_type(), 1);
        assert_eq!(Network::regtest_all_at_genesis().coin_type(), 1);
    }

    #[test]
    fn network_type_round_trip() {
        let mainnet = Network::Mainnet.to_parameters();
        let testnet = Network::Testnet.to_parameters();
        assert_eq!(mainnet.network_type(), NetworkType::Main);
        assert_eq!(testnet.network_type(), NetworkType::Test);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn network_serde_round_trip() -> Result<(), serde_json::Error> {
        for net in [
            Network::Mainnet,
            Network::Testnet,
            Network::regtest_all_at_genesis(),
        ] {
            let encoded = serde_json::to_string(&net)?;
            let decoded: Network = serde_json::from_str(&encoded)?;
            assert_eq!(decoded, net);
        }
        Ok(())
    }
}
