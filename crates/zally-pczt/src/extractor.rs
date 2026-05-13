//! `Extractor` role: pulls the final transaction out of a fully-signed PCZT.

use pczt::roles::tx_extractor::TransactionExtractor;
use zally_core::{Network, TxId};
use zcash_proofs::prover::LocalTxProver;

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

/// Final transaction bytes plus the network and txid the operator submits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractedTransaction {
    /// Wire bytes for `Submitter::submit`.
    pub raw_bytes: Vec<u8>,
    /// ZIP-244 transaction identifier.
    pub tx_id: TxId,
    /// Network the transaction is for.
    pub network: Network,
}

/// Extracts a finalised PCZT into submittable transaction bytes.
///
/// Loads the Sapling verifying keys from the platform-default `ZcashParams` directory
/// via [`LocalTxProver::with_default_location`]; an Orchard verifying key is generated
/// on the fly by the upstream extractor if needed.
#[derive(Debug, Default)]
pub struct Extractor;

impl Extractor {
    /// Constructs a new extractor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Extracts the transaction.
    ///
    /// # Errors
    ///
    /// Returns [`PcztError::ProverUnavailable`] when Sapling verifying keys cannot be loaded,
    /// [`PcztError::ParseFailed`] when the bytes are malformed, [`PcztError::NotFinalized`]
    /// when the PCZT lacks required authorizations, and [`PcztError::SerializeFailed`] when
    /// the extracted transaction fails to encode.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "extraction is single-use; consuming the bytes prevents accidental re-extraction"
    )]
    pub fn extract(&self, pczt: PcztBytes) -> Result<ExtractedTransaction, PcztError> {
        let network = pczt.network();
        let parsed = pczt.parse()?;

        let prover = LocalTxProver::with_default_location().ok_or(PcztError::ProverUnavailable)?;
        let (spend_vk, output_vk) = prover.verifying_keys();

        let tx = TransactionExtractor::new(parsed)
            .with_sapling(&spend_vk, &output_vk)
            .extract()
            .map_err(|err| PcztError::NotFinalized {
                reason: format!("upstream extraction failed: {err:?}"),
            })?;

        let txid_bytes = *tx.txid().as_ref();
        let mut raw_bytes = Vec::new();
        tx.write(&mut raw_bytes)
            .map_err(|err| PcztError::SerializeFailed {
                reason: format!("transaction encode failed: {err}"),
            })?;

        Ok(ExtractedTransaction {
            raw_bytes,
            tx_id: TxId::from_bytes(txid_bytes),
            network,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractor_rejects_malformed_pczt() {
        let extractor = Extractor::new();
        let outcome = extractor.extract(PcztBytes::from_serialized(
            vec![0_u8; 4],
            Network::regtest_all_at_genesis(),
        ));
        assert!(
            matches!(
                outcome,
                Err(PcztError::ParseFailed { .. } | PcztError::ProverUnavailable)
            ),
            "malformed PCZT (or missing params) must error, got {outcome:?}"
        );
    }
}
