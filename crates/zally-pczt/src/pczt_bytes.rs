//! Serialised PCZT bytes plus the network they are bound to.

use zally_core::Network;

use crate::pczt_error::PcztError;

/// Serialised PCZT bytes plus the network they are bound to.
///
/// Every role wrapper validates the embedded network against the caller's configured network
/// before any signing or extraction work. Misrouted PCZTs are rejected before any secret
/// material is touched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcztBytes {
    bytes: Vec<u8>,
    network: Network,
}

impl PcztBytes {
    /// Wraps pre-serialised bytes plus their network.
    #[must_use]
    pub fn from_serialized(bytes: Vec<u8>, network: Network) -> Self {
        Self { bytes, network }
    }

    /// Returns the wire bytes for transport.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the wrapper and returns the underlying bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Returns the network this PCZT is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    /// Parses the bytes into a [`pczt::Pczt`].
    pub fn parse(&self) -> Result<pczt::Pczt, PcztError> {
        pczt::Pczt::parse(&self.bytes).map_err(|err| PcztError::ParseFailed {
            reason: format!("{err:?}"),
        })
    }

    /// Wraps a freshly built `pczt::Pczt` for the given network.
    #[must_use]
    pub fn from_pczt(pczt: &pczt::Pczt, network: Network) -> Self {
        Self {
            bytes: pczt.serialize(),
            network,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pczt_bytes_round_trip_metadata() {
        let raw = vec![0_u8, 1, 2, 3];
        let pczt = PcztBytes::from_serialized(raw.clone(), Network::regtest());
        assert_eq!(pczt.as_bytes(), raw.as_slice());
        assert_eq!(pczt.network(), Network::regtest());
        assert_eq!(pczt.into_bytes(), raw);
    }
}
