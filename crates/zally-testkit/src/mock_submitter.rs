//! Programmable in-memory `Submitter` fixture.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{RejectionReason, SubmitOutcome, Submitter, SubmitterError};
use zally_core::{Network, TxId};

/// Outcome the mock should return for the next submission.
#[derive(Clone, Debug)]
enum MockOutcome {
    Accepting,
    Duplicating,
    Queuing,
    Rejecting {
        reason: RejectionReason,
        detail: String,
    },
}

struct MockState {
    network: Network,
    outcome: MockOutcome,
    submitted: Vec<Vec<u8>>,
    submit_failures: Vec<SubmitterError>,
    failures_consumed: u32,
    queued_answers_remaining: u32,
    derive_tx_id: fn(&[u8]) -> TxId,
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

    /// Constructs a submitter that always returns `SubmitOutcome::Queued`, as a node that
    /// keeps a transaction queued for verification and never reports it held.
    #[must_use]
    pub fn queuing(network: Network) -> Self {
        Self::with_outcome(network, MockOutcome::Queuing)
    }

    /// Constructs a submitter that always returns `SubmitOutcome::Rejected` with the
    /// supplied typed reason and operator-facing detail.
    #[must_use]
    pub fn rejecting(network: Network, reason: RejectionReason, detail: impl Into<String>) -> Self {
        Self::with_outcome(
            network,
            MockOutcome::Rejecting {
                reason,
                detail: detail.into(),
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
                queued_answers_remaining: 0,
                derive_tx_id: derive_txid,
            })),
        }
    }

    /// Returns the submitter with `derive_tx_id` computing the transaction id it reports for
    /// each submission, so a test that submits real transactions sees their real ids.
    #[must_use]
    pub fn with_tx_id_derivation(self, derive_tx_id: fn(&[u8]) -> TxId) -> Self {
        self.state.lock().derive_tx_id = derive_tx_id;
        self
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
    /// [`crate::MockChainSourceHandle::fail_current_epoch_next`] for the closure-factory rationale.
    pub fn fail_submit_next(&self, count: u32, mut produce_error: impl FnMut() -> SubmitterError) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.submit_failures.push(produce_error());
        }
    }

    /// Answers the next `count` submissions with `SubmitOutcome::Queued`, as a node still
    /// verifying the transaction, before the configured outcome resumes. Queued failures from
    /// [`Self::fail_submit_next`] are consumed first.
    pub fn queue_next_submissions(&self, count: u32) {
        self.state.lock().queued_answers_remaining = count;
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
            let outcome = if guard.queued_answers_remaining > 0 {
                guard.queued_answers_remaining = guard.queued_answers_remaining.saturating_sub(1);
                MockOutcome::Queuing
            } else {
                guard.outcome.clone()
            };
            (outcome, guard.derive_tx_id)
        };
        let (outcome, derive_tx_id) = outcome;
        let tx_id = derive_tx_id(raw_tx);
        Ok(match outcome {
            MockOutcome::Accepting => SubmitOutcome::Accepted { tx_id },
            MockOutcome::Duplicating => SubmitOutcome::Duplicate { tx_id },
            MockOutcome::Queuing => SubmitOutcome::Queued { tx_id },
            MockOutcome::Rejecting { reason, detail } => SubmitOutcome::Rejected { reason, detail },
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
        let submitter = MockSubmitter::rejecting(
            Network::Mainnet,
            RejectionReason::MempoolFull,
            "fee too low",
        );
        let outcome = submitter.submit(&[0; 4]).await?;
        assert!(matches!(
            outcome,
            SubmitOutcome::Rejected { reason: RejectionReason::MempoolFull, ref detail }
                if detail == "fee too low"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_submitter_queues_returns_caller_tx_id() -> Result<(), SubmitterError> {
        let submitter = MockSubmitter::queuing(Network::Testnet);
        let outcome = submitter.submit(b"raw").await?;
        assert!(matches!(outcome, SubmitOutcome::Queued { .. }));
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
