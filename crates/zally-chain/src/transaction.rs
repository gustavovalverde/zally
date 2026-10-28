//! Serialized transaction parsing at the chain-plane boundary, and whether a transaction can
//! still be mined.

use zally_core::{BlockHeight, BranchId, FailurePosture, Network, TxId};
use zcash_primitives::transaction::{Transaction, TxVersion};

/// Error returned by the transaction parsers in this module.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionParseError {
    /// The supplied bytes were not a parseable Zcash transaction.
    ///
    /// Posture: not retryable. The caller must provide different transaction
    /// bytes before retrying.
    #[error("transaction decode failed: {reason}")]
    Read {
        /// Underlying transaction decoder description.
        reason: String,
    },
}

impl TransactionParseError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub const fn posture(&self) -> FailurePosture {
        match self {
            Self::Read { .. } => FailurePosture::NotRetryable,
        }
    }
}

/// Parses serialized Zcash transaction bytes and returns the committed expiry height.
///
/// The linked librustzcash reader dispatches on the version header carried in
/// the bytes. Zcash v5 and v6 transactions carry the consensus branch id and
/// expiry height in their own header fragments, so the parser does not accept a
/// caller-selected network or version.
///
/// # Errors
///
/// Returns [`TransactionParseError::Read`] when the bytes are truncated,
/// malformed, or use an unsupported transaction format.
pub fn parse_transaction_expiry_height(
    raw_tx_bytes: &[u8],
) -> Result<BlockHeight, TransactionParseError> {
    Ok(read_transaction(raw_tx_bytes)?.expiry_height().into())
}

/// Parses serialized Zcash transaction bytes and returns the header fields that decide
/// whether the transaction can be mined: its version, its consensus branch, and its expiry.
///
/// # Errors
///
/// Returns [`TransactionParseError::Read`] when the bytes are truncated,
/// malformed, or use an unsupported transaction format.
pub fn parse_transaction_header(
    raw_tx_bytes: &[u8],
) -> Result<TransactionHeader, TransactionParseError> {
    let transaction = read_transaction(raw_tx_bytes)?;
    let version = transaction.version();
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "every format after v4 carries its consensus branch, including the \
                  cfg-gated v7"
    )]
    let consensus_branch_id = match version {
        TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => None,
        _ => Some(transaction.consensus_branch_id()),
    };
    Ok(TransactionHeader {
        version,
        consensus_branch_id,
        expiry_height: transaction.expiry_height().into(),
    })
}

/// The header fields that decide whether a transaction can be mined.
///
/// Signatures over a v4 or later transaction commit to a consensus branch (ZIP 243, ZIP 244),
/// so a transaction signed for one branch is invalid in every block of another, and signing it
/// again cannot change that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionHeader {
    version: TxVersion,
    consensus_branch_id: Option<BranchId>,
    expiry_height: BlockHeight,
}

impl TransactionHeader {
    /// Builds the header of a PCZT from its global fields.
    ///
    /// A PCZT carries the transaction version number without the `fOverwintered` flag, and
    /// every PCZT version is overwintered.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionParseError::Read`] when the version and version group or the
    /// consensus branch ID are unknown.
    pub fn from_pczt_global(
        tx_version: u32,
        version_group_id: u32,
        consensus_branch_id: u32,
        expiry_height: u32,
    ) -> Result<Self, TransactionParseError> {
        let mut header_bytes = (tx_version | OVERWINTERED_FLAG).to_le_bytes().to_vec();
        header_bytes.extend_from_slice(&version_group_id.to_le_bytes());
        let version = TxVersion::read(header_bytes.as_slice()).map_err(|err| {
            TransactionParseError::Read {
                reason: format!("PCZT transaction version: {err}"),
            }
        })?;
        let consensus_branch_id =
            BranchId::try_from(consensus_branch_id).map_err(|err| TransactionParseError::Read {
                reason: format!("PCZT consensus branch ID: {err}"),
            })?;
        Ok(Self {
            version,
            consensus_branch_id: Some(consensus_branch_id),
            expiry_height: BlockHeight::from(expiry_height),
        })
    }

    /// Transaction version.
    #[must_use]
    pub const fn version(&self) -> TxVersion {
        self.version
    }

    /// Consensus branch the transaction commits to, or `None` for a version before v5,
    /// whose header does not carry it.
    #[must_use]
    pub const fn consensus_branch_id(&self) -> Option<BranchId> {
        self.consensus_branch_id
    }

    /// Height after which consensus rejects the transaction; zero when it never expires.
    #[must_use]
    pub const fn expiry_height(&self) -> BlockHeight {
        self.expiry_height
    }

    /// Checks that the transaction can still be mined, judging validity for the block after
    /// `tip_height` with [`BranchId::for_height`] and [`TxVersion::valid_in_branch`].
    ///
    /// A transaction committed to a consensus branch that activates after that block is not
    /// rejected: it becomes valid once the chain reaches its branch. A rejection holds for
    /// every later block, so the transaction must be recreated rather than signed again; only a
    /// reorg below `tip_height` can make it valid again.
    ///
    /// # Errors
    ///
    /// Returns the [`UnmineableReason`] that keeps the transaction out of every block after
    /// `tip_height`.
    pub fn check_mineable(
        &self,
        network: Network,
        tip_height: BlockHeight,
    ) -> Result<(), UnmineableReason> {
        let params = network.to_parameters();
        let next_height = BlockHeight::from(tip_height.as_u32().saturating_add(1));
        let next_branch_id = BranchId::for_height(&params, next_height.into());
        if self.expiry_height.as_u32() != 0 && next_height > self.expiry_height {
            return Err(UnmineableReason::Expired {
                expiry_height: self.expiry_height,
                next_height,
            });
        }
        if let Some(signed_branch_id) = self.consensus_branch_id
            && signed_branch_id != next_branch_id
        {
            let activates_later =
                signed_branch_id
                    .height_bounds(&params)
                    .is_some_and(|(activation_height, _)| {
                        BlockHeight::from(activation_height) > next_height
                    });
            return if activates_later {
                Ok(())
            } else {
                Err(UnmineableReason::ConsensusBranchMismatch {
                    signed_branch_id,
                    next_branch_id,
                })
            };
        }
        if self.version.valid_in_branch(next_branch_id) {
            Ok(())
        } else {
            Err(UnmineableReason::VersionNotAccepted {
                version: self.version,
                next_branch_id,
            })
        }
    }
}

/// Why a transaction can never be mined.
///
/// Returned by [`TransactionHeader::check_mineable`]. The transaction must be recreated from
/// a new proposal; signing it again cannot repair it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum UnmineableReason {
    /// The transaction expired before the next block.
    #[error("expired at height {expiry_height}, before the next block at height {next_height}")]
    Expired {
        /// Last height at which the transaction could be mined.
        expiry_height: BlockHeight,
        /// Height of the next block.
        next_height: BlockHeight,
    },
    /// The transaction commits to a consensus branch that the next block no longer follows,
    /// for example because a network upgrade activated while it was pending.
    #[error(
        "signed for consensus branch {signed_branch_id:?}, but the next block follows \
         {next_branch_id:?}"
    )]
    ConsensusBranchMismatch {
        /// Branch the transaction's signatures commit to.
        signed_branch_id: BranchId,
        /// Branch of the next block.
        next_branch_id: BranchId,
    },
    /// The next block's consensus branch no longer accepts the transaction version, such as
    /// v4 once NU7 is active (ZIP 2003).
    #[error("version {version:?} is not accepted under consensus branch {next_branch_id:?}")]
    VersionNotAccepted {
        /// Transaction version.
        version: TxVersion,
        /// Branch of the next block.
        next_branch_id: BranchId,
    },
}

/// `fOverwintered`, the high bit of an overwintered transaction's header word.
const OVERWINTERED_FLAG: u32 = 1 << 31;

/// Reads a transaction whose version dispatches its own format. A v5 or later transaction
/// carries its consensus branch; the branch supplied here only parses earlier versions.
fn read_transaction(raw_tx_bytes: &[u8]) -> Result<Transaction, TransactionParseError> {
    Transaction::read(raw_tx_bytes, BranchId::Nu5).map_err(|err| TransactionParseError::Read {
        reason: err.to_string(),
    })
}

/// Parses serialized Zcash transaction bytes and returns their canonical transaction id.
///
/// The linked librustzcash reader applies the version-appropriate transaction-id digest,
/// including ZIP-244 for v5 and later transactions.
///
/// # Errors
///
/// Returns [`TransactionParseError::Read`] when the bytes are truncated,
/// malformed, or use an unsupported transaction format.
pub fn parse_transaction_id(raw_tx_bytes: &[u8]) -> Result<TxId, TransactionParseError> {
    Ok(TxId::from_bytes(
        *read_transaction(raw_tx_bytes)?.txid().as_ref(),
    ))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test helpers panic on impossible failures when building minimal transactions"
)]
mod tests {
    use super::{
        TransactionHeader, TransactionParseError, UnmineableReason,
        parse_transaction_expiry_height, parse_transaction_header, parse_transaction_id,
    };
    use zally_core::{BlockHeight, BranchId, Network};
    use zcash_primitives::transaction::{Transaction, TransactionData, TxVersion};
    use zcash_protocol::consensus::BlockHeight as UpstreamBlockHeight;
    use zcash_protocol::local_consensus::LocalNetwork;

    const NU7_ACTIVATION_HEIGHT: u32 = 100;

    fn regtest_with_nu7_at_one_hundred() -> Network {
        let height_one = Some(UpstreamBlockHeight::from_u32(1));
        Network::Regtest(LocalNetwork {
            overwinter: height_one,
            sapling: height_one,
            blossom: height_one,
            heartwood: height_one,
            canopy: height_one,
            nu5: height_one,
            nu6: height_one,
            nu6_1: height_one,
            nu6_2: height_one,
            nu6_3: height_one,
            nu7: Some(UpstreamBlockHeight::from_u32(NU7_ACTIVATION_HEIGHT)),
        })
    }

    fn header_of(raw_tx_bytes: &[u8]) -> TransactionHeader {
        parse_transaction_header(raw_tx_bytes).expect("parse minimal transaction header")
    }

    fn build_minimal_v4(expiry_height: u32) -> Vec<u8> {
        let transaction = TransactionData::from_parts(
            TxVersion::V4,
            BranchId::Nu6_3,
            0,
            UpstreamBlockHeight::from(expiry_height),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .expect("freeze minimal v4 transaction");
        let mut raw_tx_bytes = Vec::new();
        transaction
            .write(&mut raw_tx_bytes)
            .expect("write minimal v4 transaction");
        raw_tx_bytes
    }

    fn build_v6_on(branch_id: BranchId, expiry_height: u32) -> Vec<u8> {
        let transaction = TransactionData::from_parts_v6(
            branch_id,
            0,
            UpstreamBlockHeight::from(expiry_height),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .expect("freeze minimal v6 transaction");
        let mut raw_tx_bytes = Vec::new();
        transaction
            .write(&mut raw_tx_bytes)
            .expect("write minimal v6 transaction");
        raw_tx_bytes
    }

    #[test]
    fn header_carries_the_version_branch_and_expiry() {
        let header = header_of(&build_v6_on(BranchId::Nu6_3, 140));
        assert_eq!(header.version(), TxVersion::V6);
        assert_eq!(header.consensus_branch_id(), Some(BranchId::Nu6_3));
        assert_eq!(header.expiry_height(), BlockHeight::from(140));
    }

    #[test]
    fn header_before_v5_carries_no_consensus_branch() {
        let header = header_of(&build_minimal_v4(140));
        assert_eq!(header.version(), TxVersion::V4);
        assert_eq!(header.consensus_branch_id(), None);
    }

    #[test]
    fn transaction_signed_before_nu7_is_unmineable_once_nu7_is_next() {
        let network = regtest_with_nu7_at_one_hundred();
        let header = header_of(&build_v6_on(BranchId::Nu6_3, 140));
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(NU7_ACTIVATION_HEIGHT - 2)),
            Ok(())
        );
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(NU7_ACTIVATION_HEIGHT - 1)),
            Err(UnmineableReason::ConsensusBranchMismatch {
                signed_branch_id: BranchId::Nu6_3,
                next_branch_id: BranchId::Nu7,
            })
        );
    }

    #[test]
    fn mainnet_nu7_activation_comes_from_zcash_protocol() {
        const MAINNET_NU7_ACTIVATION_HEIGHT: u32 = 3_543_405;
        let header = header_of(&build_v6_on(BranchId::Nu6_3, 3_543_440));
        assert_eq!(
            header.check_mineable(
                Network::Mainnet,
                BlockHeight::from(MAINNET_NU7_ACTIVATION_HEIGHT - 2)
            ),
            Ok(())
        );
        assert!(matches!(
            header.check_mineable(
                Network::Mainnet,
                BlockHeight::from(MAINNET_NU7_ACTIVATION_HEIGHT - 1)
            ),
            Err(UnmineableReason::ConsensusBranchMismatch { .. })
        ));
    }

    #[test]
    fn transaction_for_a_later_branch_is_not_rejected() {
        let header = header_of(&build_v6_on(BranchId::Nu7, 200));
        assert_eq!(
            header.check_mineable(regtest_with_nu7_at_one_hundred(), BlockHeight::from(50)),
            Ok(())
        );
    }

    #[test]
    fn transaction_is_unmineable_once_the_next_block_is_past_its_expiry() {
        let network = regtest_with_nu7_at_one_hundred();
        let header = header_of(&build_v6_on(BranchId::Nu7, 120));
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(119)),
            Ok(())
        );
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(120)),
            Err(UnmineableReason::Expired {
                expiry_height: BlockHeight::from(120),
                next_height: BlockHeight::from(121),
            })
        );
    }

    #[test]
    fn zero_expiry_never_expires() {
        let header = header_of(&build_v6_on(BranchId::Nu7, 0));
        assert_eq!(
            header.check_mineable(
                regtest_with_nu7_at_one_hundred(),
                BlockHeight::from(1_000_000)
            ),
            Ok(())
        );
    }

    #[test]
    fn v4_is_unmineable_once_nu7_is_next() {
        let network = regtest_with_nu7_at_one_hundred();
        let header = header_of(&build_minimal_v4(140));
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(NU7_ACTIVATION_HEIGHT - 2)),
            Ok(())
        );
        assert_eq!(
            header.check_mineable(network, BlockHeight::from(NU7_ACTIVATION_HEIGHT - 1)),
            Err(UnmineableReason::VersionNotAccepted {
                version: TxVersion::V4,
                next_branch_id: BranchId::Nu7,
            })
        );
    }

    #[test]
    fn pczt_global_fields_form_a_header() {
        let header = TransactionHeader::from_pczt_global(
            zcash_protocol::constants::V6_TX_VERSION,
            zcash_protocol::constants::V6_VERSION_GROUP_ID,
            u32::from(BranchId::Nu6_3),
            140,
        )
        .expect("v6 PCZT global fields");
        assert_eq!(header, header_of(&build_v6_on(BranchId::Nu6_3, 140)));
    }

    #[test]
    fn pczt_global_fields_with_an_unknown_version_are_rejected() {
        assert!(matches!(
            TransactionHeader::from_pczt_global(
                9,
                zcash_protocol::constants::V6_VERSION_GROUP_ID,
                u32::from(BranchId::Nu7),
                140,
            ),
            Err(TransactionParseError::Read { .. })
        ));
        assert!(matches!(
            TransactionHeader::from_pczt_global(
                zcash_protocol::constants::V6_TX_VERSION,
                zcash_protocol::constants::V6_VERSION_GROUP_ID,
                0x1234_5678,
                140,
            ),
            Err(TransactionParseError::Read { .. })
        ));
    }

    fn build_minimal_v5(expiry_height: u32) -> Vec<u8> {
        let transaction = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            0,
            UpstreamBlockHeight::from(expiry_height),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .expect("freeze minimal v5 transaction");
        let mut raw_tx_bytes = Vec::new();
        transaction
            .write(&mut raw_tx_bytes)
            .expect("write minimal v5 transaction");
        raw_tx_bytes
    }

    #[test]
    fn transaction_id_matches_the_canonical_transaction_digest() {
        let raw_tx_bytes = build_minimal_v5(123_456);
        let parsed = Transaction::read(raw_tx_bytes.as_slice(), BranchId::Nu5)
            .expect("parse minimal v5 transaction");

        assert_eq!(
            parse_transaction_id(&raw_tx_bytes).expect("derive transaction id"),
            zally_core::TxId::from_bytes(*parsed.txid().as_ref())
        );
    }

    #[test]
    fn parses_v5_expiry_height() {
        let expiry_height = 4_321_098;
        let raw_tx_bytes = build_minimal_v5(expiry_height);
        let parsed_height =
            parse_transaction_expiry_height(&raw_tx_bytes).expect("parse minimal v5 transaction");
        assert_eq!(parsed_height, BlockHeight::from(expiry_height));
    }

    #[test]
    fn parses_v6_expiry_height() {
        let expiry_height = 4_321_098;
        let raw_tx_bytes = build_v6_on(BranchId::Nu6_3, expiry_height);
        let parsed_height =
            parse_transaction_expiry_height(&raw_tx_bytes).expect("parse minimal v6 transaction");
        assert_eq!(parsed_height, BlockHeight::from(expiry_height));
    }

    #[test]
    fn rejects_malformed_transaction_bytes() {
        let error = parse_transaction_expiry_height(&[0_u8; 4])
            .expect_err("malformed transaction bytes must fail");
        let TransactionParseError::Read { reason } = error;
        assert!(!reason.is_empty());
    }

    #[test]
    fn malformed_transaction_is_not_retryable() {
        let error = parse_transaction_expiry_height(&[0_u8; 4])
            .expect_err("malformed transaction bytes must fail");
        assert_eq!(error.posture(), zally_core::FailurePosture::NotRetryable);
    }
}
