//! Recipient of a payment.

use crate::network::Network;

/// Recipient of a payment.
///
/// Variants name what the operator and the wallet need to handle differently: `TexAddress`
/// triggers ZIP-320 enforcement (no shielded inputs); transparent recipients reject memos per
/// ZIP-302; shielded recipients accept memos.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PaymentRecipient {
    /// Encoded Unified Address (ZIP-316).
    UnifiedAddress {
        /// Bech32-encoded address.
        encoded: String,
        /// Network the address is bound to.
        network: Network,
    },
    /// Encoded Sapling z-address. Legacy operator support; UAs are preferred.
    SaplingAddress {
        /// Bech32-encoded address.
        encoded: String,
        /// Network the address is bound to.
        network: Network,
    },
    /// Transparent P2PKH or P2SH address.
    TransparentAddress {
        /// Base58Check-encoded address.
        encoded: String,
        /// Network the address is bound to.
        network: Network,
    },
    /// TEX address per ZIP-320. Refuses shielded inputs at proposal time.
    TexAddress {
        /// Bech32m-encoded address.
        encoded: String,
        /// Network the address is bound to.
        network: Network,
    },
}

impl PaymentRecipient {
    /// Network the recipient is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        match self {
            Self::UnifiedAddress { network, .. }
            | Self::SaplingAddress { network, .. }
            | Self::TransparentAddress { network, .. }
            | Self::TexAddress { network, .. } => *network,
        }
    }

    /// Whether this recipient is a transparent address (P2PKH, P2SH, or TEX).
    ///
    /// Transparent recipients reject memos at the API boundary per ZIP-302.
    #[must_use]
    pub fn is_transparent(&self) -> bool {
        matches!(
            self,
            Self::TransparentAddress { .. } | Self::TexAddress { .. }
        )
    }

    /// Whether this recipient is a TEX address.
    ///
    /// TEX recipients additionally require an all-transparent input set per ZIP-320.
    #[must_use]
    pub fn is_tex(&self) -> bool {
        matches!(self, Self::TexAddress { .. })
    }

    /// Encoded form of the address.
    #[must_use]
    pub fn encoded(&self) -> &str {
        match self {
            Self::UnifiedAddress { encoded, .. }
            | Self::SaplingAddress { encoded, .. }
            | Self::TransparentAddress { encoded, .. }
            | Self::TexAddress { encoded, .. } => encoded.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_recipient_classifications() {
        let ua = PaymentRecipient::UnifiedAddress {
            encoded: "uregtest1example".into(),
            network: Network::regtest(),
        };
        assert!(!ua.is_transparent());
        assert!(!ua.is_tex());

        let tex = PaymentRecipient::TexAddress {
            encoded: "tex-mainnet-example".into(),
            network: Network::Mainnet,
        };
        assert!(tex.is_transparent());
        assert!(tex.is_tex());

        let tr = PaymentRecipient::TransparentAddress {
            encoded: "t1example".into(),
            network: Network::Mainnet,
        };
        assert!(tr.is_transparent());
        assert!(!tr.is_tex());
    }
}
