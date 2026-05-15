//! `Combiner` role: merges multiple authorized PCZTs.
//!
//! Used for FROST quorum signatures and multi-party signer flows. Validates that every
//! input PCZT shares the same network before delegating to the upstream
//! `pczt::roles::combiner::Combiner`.

use zally_core::Network;

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

/// Merges multiple authorized PCZTs.
#[derive(Debug, Default)]
pub struct Combiner;

impl Combiner {
    /// Constructs a new combiner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Combines `pczts`. Every input must share the same network; the upstream combiner
    /// merges signatures and detects input/output disagreements.
    pub fn combine(&self, pczts: Vec<PcztBytes>) -> Result<PcztBytes, PcztError> {
        let Some(first) = pczts.first() else {
            return Err(PcztError::CombineConflict {
                reason: "combiner requires at least one PCZT".into(),
            });
        };
        let network = first.network();
        let mut parsed = Vec::with_capacity(pczts.len());
        for pczt in pczts {
            validate_shared_network(&pczt, network)?;
            parsed.push(pczt.parse()?);
        }
        let combined = pczt::roles::combiner::Combiner::new(parsed)
            .combine()
            .map_err(|err| PcztError::CombineConflict {
                reason: format!("{err:?}"),
            })?;
        Ok(PcztBytes::from_pczt(&combined, network))
    }
}

fn validate_shared_network(pczt: &PcztBytes, expected: Network) -> Result<(), PcztError> {
    if pczt.network() == expected {
        Ok(())
    } else {
        Err(PcztError::mismatch(pczt.network(), expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combiner_rejects_empty_input() {
        let combiner = Combiner::new();
        let outcome = combiner.combine(Vec::new());
        assert!(matches!(outcome, Err(PcztError::CombineConflict { .. })));
    }
}
