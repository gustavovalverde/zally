//! Programmable in-memory `Submitter` fixture.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{SubmitOutcome, Submitter, SubmitterError};
use zally_core::{Network, TxId};

/// Outcome the mock should return for the next submission.
#[derive(Clone, Debug)]
enum MockOutcome {
    Accepting,
    Duplicating,
    Rejecting { reason: String },
}

struct MockState {
    network: Network,
    outcome: MockOutcome,
    submitted: Vec<Vec<u8>>,
    submit_failures: Vec<SubmitterError>,
    failures_consumed: u32,
}

/// Programmable [`Submitter`] for tests.
pub struct MockSubmitter {
    state: Arc<Mutex<MockState>>,
}

impl MockSubmitter {
    /// Constructs a submitter that always returns `SubmitOutcome::Accepted` for `network`.
    /// The returned `TxId` is derived from a fold-hash of the raw bytes so test assertions
    /// can pin a deterministic id without coupling to a real signing path.
    #[must_use]
    pub fn accepting(network: Network) -> Self {
        Self::with_outcome(network, MockOutcome::Accepting)
    }

    /// Constructs a submitter that always returns `SubmitOutcome::Duplicate`.
    #[must_use]
    pub fn duplicating(network: Network) -> Self {
        Self::with_outcome(network, MockOutcome::Duplicating)
    }

    /// Constructs a submitter that always returns `SubmitOutcome::Rejected` with `reason`.
    #[must_use]
    pub fn rejecting(network: Network, reason: impl Into<String>) -> Self {
        Self::with_outcome(
            network,
            MockOutcome::Rejecting {
                reason: reason.into(),
            },
        )
    }

    fn with_outcome(network: Network, outcome: MockOutcome) -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                network,
                outcome,
                submitted: Vec::new(),
                submit_failures: Vec::new(),
                failures_consumed: 0,
            })),
        }
    }

    /// Returns a handle that lets the test inspect submitted bytes.
    #[must_use]
    pub fn handle(&self) -> MockSubmitterHandle {
        MockSubmitterHandle {
            state: Arc::clone(&self.state),
        }
    }
}

/// Handle that exposes inspection helpers without requiring a `&MockSubmitter`.
#[derive(Clone)]
pub struct MockSubmitterHandle {
    state: Arc<Mutex<MockState>>,
}

impl MockSubmitterHandle {
    /// Returns a snapshot of the raw transaction bytes submitted so far.
    #[must_use]
    pub fn submitted_bytes(&self) -> Vec<Vec<u8>> {
        self.state.lock().submitted.clone()
    }

    /// Returns the number of transactions submitted so far.
    #[must_use]
    pub fn submission_count(&self) -> usize {
        self.state.lock().submitted.len()
    }

    /// Queues `count` consecutive failures for `submit` calls. Each subsequent call pops one
    /// failure off the queue; once empty, calls return the configured outcome.
    ///
    /// `produce_error` is invoked once per queued failure; see
    /// [`crate::MockChainSourceHandle::fail_chain_tip_next`] for the closure-factory rationale.
    pub fn fail_submit_next(&self, count: u32, mut produce_error: impl FnMut() -> SubmitterError) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.submit_failures.push(produce_error());
        }
    }

    /// Number of failures consumed since the submitter was constructed.
    #[must_use]
    pub fn failures_consumed(&self) -> u32 {
        self.state.lock().failures_consumed
    }
}

#[async_trait]
impl Submitter for MockSubmitter {
    fn network(&self) -> Network {
        self.state.lock().network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let outcome = {
            let mut guard = self.state.lock();
            if !guard.submit_failures.is_empty() {
                let injected = guard.submit_failures.remove(0);
                guard.failures_consumed = guard.failures_consumed.saturating_add(1);
                return Err(injected);
            }
            guard.submitted.push(raw_tx.to_vec());
            guard.outcome.clone()
        };
        let tx_id = derive_txid(raw_tx);
        Ok(match outcome {
            MockOutcome::Accepting => SubmitOutcome::Accepted { tx_id },
            MockOutcome::Duplicating => SubmitOutcome::Duplicate { tx_id },
            MockOutcome::Rejecting { reason } => SubmitOutcome::Rejected { reason },
        })
    }
}

fn derive_txid(bytes: &[u8]) -> TxId {
    // Fold-hash by XORing the input into 32-byte buckets; deterministic and stable for
    // test assertions without pulling a cryptographic hash in for fixture-only use.
    let mut buf = [0_u8; 32];
    for (idx, byte) in bytes.iter().enumerate() {
        buf[idx % 32] ^= *byte;
    }
    TxId::from_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_submitter_accepts_and_records() -> Result<(), SubmitterError> {
        let submitter = MockSubmitter::accepting(Network::regtest());
        let handle = submitter.handle();
        let outcome = submitter.submit(&[1, 2, 3]).await?;
        assert!(matches!(outcome, SubmitOutcome::Accepted { .. }));
        assert_eq!(handle.submission_count(), 1);
        assert_eq!(handle.submitted_bytes(), vec![vec![1, 2, 3]]);
        Ok(())
    }

    #[tokio::test]
    async fn mock_submitter_rejects_with_reason() -> Result<(), SubmitterError> {
        let submitter = MockSubmitter::rejecting(Network::Mainnet, "fee too low");
        let outcome = submitter.submit(&[0; 4]).await?;
        assert!(matches!(
            outcome,
            SubmitOutcome::Rejected { ref reason } if reason == "fee too low"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_submitter_duplicate_returns_txid() -> Result<(), SubmitterError> {
        let submitter = MockSubmitter::duplicating(Network::Testnet);
        let outcome = submitter.submit(b"raw").await?;
        assert!(matches!(outcome, SubmitOutcome::Duplicate { .. }));
        Ok(())
    }
}
