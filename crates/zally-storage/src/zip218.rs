//! Pre-check of proposals against the ZIP 218 per-block limits on shielded components.
//!
//! Once NU7 is active, ZIP 218 bounds the Orchard actions, the Ironwood actions, and the Sapling
//! spends and outputs of a block, and their total. Zebra 7 rejects a transaction that alone
//! exceeds a bound at mempool admission, but `zcash_client_backend` 0.25 proposes and builds one
//! without complaint. This check is temporary: delete it once `zcash_client_backend` ships its own
//! guard (`Proposal::check_shielded_limits` on librustzcash branch
//! `michal/zip-218-shielded-limits`), and classify that guard's
//! `ProposalError::ShieldedLimitExceeded` as [`ProposalRejectionKind::ShieldedLimitExceeded`].

use zcash_client_backend::proposal::{Proposal, Step};
use zcash_primitives::transaction::builder::BundlePadding;
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_protocol::PoolType;
use zcash_protocol::consensus::{BlockHeight, BranchId, NetworkUpgrade, Parameters};

use crate::error::StorageError;
use crate::proposal_rejection_kind::{ProposalRejectionKind, ShieldedBlockLimit};

const ORCHARD_PROTOCOL_BLOCK_ACTION_LIMIT: usize = 330;
const SAPLING_BLOCK_IO_LIMIT: usize = 300;
const GLOBAL_SHIELDED_BUDGET: usize = 330;

/// Rejects a proposal with a step whose transaction would exceed a ZIP 218 limit.
///
/// The limits bind only where NU7 is active at the proposal's target height. Each step is
/// counted as the transaction builder builds it: the Sapling bundle padded to two outputs, the
/// Orchard bundle with the default padding Zally builds with, and the Ironwood bundle with the
/// padding the step directs.
pub(crate) fn check_zip218_limits<P: Parameters, FeeRuleT, NoteRef>(
    params: &P,
    proposal: &Proposal<FeeRuleT, NoteRef>,
) -> Result<(), StorageError> {
    let target_height = BlockHeight::from(proposal.min_target_height());
    if !params.is_nu_active(NetworkUpgrade::Nu7, target_height) {
        return Ok(());
    }
    let branch_id = BranchId::for_height(params, target_height);
    for (step_index, step) in proposal.steps().iter().enumerate() {
        let counts = ShieldedComponentCounts::of(step, branch_id).map_err(|reason| {
            StorageError::ProposalRejected {
                kind: ProposalRejectionKind::Other,
                reason: format!("proposal step {step_index} cannot form a bundle: {reason}"),
            }
        })?;
        if let Some((limit, component_count, maximum)) = counts.first_exceeded() {
            return Err(StorageError::ProposalRejected {
                kind: ProposalRejectionKind::ShieldedLimitExceeded {
                    limit,
                    component_count,
                },
                reason: format!(
                    "proposal step {step_index} has {component_count} {limit}, more than the \
                     {maximum} ZIP 218 allows in a block once NU7 is active"
                ),
            });
        }
    }
    Ok(())
}

/// The shielded components of one step's transaction, as ZIP 218 counts them.
struct ShieldedComponentCounts {
    orchard_actions: usize,
    ironwood_actions: usize,
    sapling_spends_and_outputs: usize,
}

impl ShieldedComponentCounts {
    fn of<NoteRef>(step: &Step<NoteRef>, branch_id: BranchId) -> Result<Self, &'static str> {
        let sapling_bundle = sapling::builder::BundleType::DEFAULT;
        let sapling_spends = step.input_count_in_pool(PoolType::SAPLING);
        let sapling_outputs = step.output_count_in_pool(PoolType::SAPLING)
            + step.change_count_in_pool(PoolType::SAPLING);
        let orchard_version = bundle_version_for_branch(branch_id, orchard::ValuePool::Orchard)
            .ok_or("the Orchard pool does not exist at the target height")?;
        let ironwood_version = bundle_version_for_branch(branch_id, orchard::ValuePool::Ironwood)
            .ok_or("the Ironwood pool does not exist at the target height")?;
        Ok(Self {
            orchard_actions: step.orchard_action_count(BundlePadding::DEFAULT, orchard_version)?,
            ironwood_actions: step
                .ironwood_action_count(step.ironwood_bundle_padding(), ironwood_version)?,
            sapling_spends_and_outputs: sapling_bundle.num_spends(sapling_spends)?
                + sapling_bundle.num_outputs(sapling_spends, sapling_outputs)?,
        })
    }

    /// Returns the first exceeded limit, in ZIP 218's order, with the count and its maximum.
    fn first_exceeded(&self) -> Option<(ShieldedBlockLimit, usize, usize)> {
        let total = self.orchard_actions + self.ironwood_actions + self.sapling_spends_and_outputs;
        [
            (
                ShieldedBlockLimit::OrchardActions,
                self.orchard_actions,
                ORCHARD_PROTOCOL_BLOCK_ACTION_LIMIT,
            ),
            (
                ShieldedBlockLimit::IronwoodActions,
                self.ironwood_actions,
                ORCHARD_PROTOCOL_BLOCK_ACTION_LIMIT,
            ),
            (
                ShieldedBlockLimit::SaplingSpendsAndOutputs,
                self.sapling_spends_and_outputs,
                SAPLING_BLOCK_IO_LIMIT,
            ),
            (
                ShieldedBlockLimit::AllShieldedPools,
                total,
                GLOBAL_SHIELDED_BUDGET,
            ),
        ]
        .into_iter()
        .find(|(_, count, maximum)| count > maximum)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "synthetic proposal fixtures panic on impossible construction failures"
)]
mod tests {
    use std::collections::BTreeMap;

    use incrementalmerkletree::Position;
    use nonempty::NonEmpty;
    use zcash_client_backend::data_api::wallet::{ConfirmationsPolicy, TargetHeight};
    use zcash_client_backend::fees::{ChangeValue, StandardFeeRule, TransactionBalance};
    use zcash_client_backend::proposal::{Proposal, ShieldedInputs};
    use zcash_client_backend::wallet::{Note, ReceivedNote};
    use zcash_client_backend::zip321::TransactionRequest;
    use zcash_protocol::consensus::BlockHeight;
    use zcash_protocol::local_consensus::LocalNetwork;
    use zcash_protocol::value::Zatoshis;

    use super::check_zip218_limits;
    use crate::error::StorageError;
    use crate::proposal_rejection_kind::{ProposalRejectionKind, ShieldedBlockLimit};

    const NU7_ACTIVATION_HEIGHT: u32 = 50;
    const NOTE_ZAT: u64 = 10_000;

    #[derive(Clone, Copy)]
    enum NotePool {
        Sapling,
        Orchard,
        Ironwood,
    }

    fn regtest_with_nu7_at_fifty() -> LocalNetwork {
        let height_one = Some(BlockHeight::from_u32(1));
        LocalNetwork {
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
            nu7: Some(BlockHeight::from_u32(NU7_ACTIVATION_HEIGHT)),
        }
    }

    fn note(pool: NotePool) -> Note {
        match pool {
            NotePool::Sapling => {
                let recipient = sapling::zip32::ExtendedSpendingKey::master(&[0x2a; 32])
                    .expect("the seed derives a Sapling key")
                    .default_address()
                    .1;
                Note::Sapling(sapling::Note::from_parts(
                    recipient,
                    sapling::value::NoteValue::from_raw(NOTE_ZAT),
                    sapling::Rseed::AfterZip212([0x1b; 32]),
                ))
            }
            NotePool::Orchard | NotePool::Ironwood => {
                let (value_pool, version) = if matches!(pool, NotePool::Orchard) {
                    (orchard::ValuePool::Orchard, orchard::NoteVersion::V2)
                } else {
                    (orchard::ValuePool::Ironwood, orchard::NoteVersion::V3)
                };
                let spending_key: orchard::keys::SpendingKey =
                    Option::from(orchard::keys::SpendingKey::from_bytes([0x2a; 32]))
                        .expect("the bytes are a valid Orchard spending key");
                let recipient = orchard::keys::FullViewingKey::from(&spending_key)
                    .address_at(0_u32, zip32::Scope::External);
                let rho: orchard::note::Rho =
                    Option::from(orchard::note::Rho::from_bytes(&[0; 32]))
                        .expect("zero is a valid rho");
                let rseed: orchard::note::RandomSeed =
                    Option::from(orchard::note::RandomSeed::from_bytes([0x1b; 32], &rho))
                        .expect("the bytes are a valid rseed");
                let orchard_note: orchard::Note = Option::from(orchard::Note::from_parts(
                    recipient,
                    orchard::value::NoteValue::from_raw(NOTE_ZAT),
                    rho,
                    rseed,
                    version,
                ))
                .expect("the parts form a valid note");
                Note::Orchard {
                    note: orchard_note,
                    pool: value_pool,
                }
            }
        }
    }

    fn zatoshis(zat: u64) -> Zatoshis {
        Zatoshis::from_u64(zat).expect("the value is in range")
    }

    /// A single-step proposal targeting `target_height` that spends `spends` notes of each
    /// pool and returns all but one note's value as one change output per spent pool.
    fn proposal_spending(
        spends: &[(NotePool, usize)],
        target_height: u32,
    ) -> Proposal<StandardFeeRule, u32> {
        let mut received = Vec::new();
        let mut change = Vec::new();
        for &(pool, count) in spends {
            for _ in 0..count {
                let index = u32::try_from(received.len()).expect("few notes");
                received.push(ReceivedNote::from_parts(
                    index,
                    zcash_protocol::TxId::from_bytes([0; 32]),
                    u16::try_from(index).expect("few notes"),
                    note(pool),
                    zip32::Scope::External,
                    Position::from(u64::from(index)),
                    Some(BlockHeight::from_u32(1)),
                    None,
                ));
            }
            let change_zat = zatoshis(NOTE_ZAT * (u64::try_from(count).expect("few notes") - 1));
            change.push(match pool {
                NotePool::Sapling => ChangeValue::sapling(change_zat, None),
                NotePool::Orchard => ChangeValue::orchard(change_zat, None),
                NotePool::Ironwood => ChangeValue::ironwood(change_zat, None),
            });
        }
        let fee = zatoshis(NOTE_ZAT * u64::try_from(spends.len()).expect("few pools"));
        Proposal::single_step(
            TransactionRequest::empty(),
            BTreeMap::new(),
            Vec::new(),
            NonEmpty::from_vec(received).map(ShieldedInputs::from_parts),
            BlockHeight::from_u32(target_height - 1),
            TransactionBalance::new(change, fee).expect("the balance is in range"),
            StandardFeeRule::Zip317,
            TargetHeight::from(BlockHeight::from_u32(target_height)),
            ConfirmationsPolicy::default(),
            false,
            true,
        )
        .expect("the proposal balances")
    }

    fn check_after_nu7(spends: &[(NotePool, usize)]) -> Result<(), StorageError> {
        check_zip218_limits(
            &regtest_with_nu7_at_fifty(),
            &proposal_spending(spends, NU7_ACTIVATION_HEIGHT + 10),
        )
    }

    fn exceeded(checked: &Result<(), StorageError>) -> Option<(ShieldedBlockLimit, usize)> {
        match checked {
            Err(StorageError::ProposalRejected {
                kind:
                    ProposalRejectionKind::ShieldedLimitExceeded {
                        limit,
                        component_count,
                    },
                ..
            }) => Some((*limit, *component_count)),
            _ => None,
        }
    }

    #[test]
    fn three_hundred_and_thirty_orchard_actions_pass_and_one_more_is_rejected() {
        // From NU6.3 each Orchard spend and output takes its own action: 329 spends and one
        // change output are 330 actions.
        assert!(check_after_nu7(&[(NotePool::Orchard, 329)]).is_ok());
        assert_eq!(
            exceeded(&check_after_nu7(&[(NotePool::Orchard, 330)])),
            Some((ShieldedBlockLimit::OrchardActions, 331))
        );
    }

    #[test]
    fn three_hundred_and_thirty_ironwood_actions_pass_and_one_more_is_rejected() {
        // An Ironwood action carries a spend and an output, so the change output shares one.
        assert!(check_after_nu7(&[(NotePool::Ironwood, 330)]).is_ok());
        assert_eq!(
            exceeded(&check_after_nu7(&[(NotePool::Ironwood, 331)])),
            Some((ShieldedBlockLimit::IronwoodActions, 331))
        );
    }

    #[test]
    fn three_hundred_sapling_spends_and_outputs_pass_and_one_more_is_rejected() {
        // The Sapling bundle pads its single change output to two outputs.
        assert!(check_after_nu7(&[(NotePool::Sapling, 298)]).is_ok());
        assert_eq!(
            exceeded(&check_after_nu7(&[(NotePool::Sapling, 299)])),
            Some((ShieldedBlockLimit::SaplingSpendsAndOutputs, 301))
        );
    }

    #[test]
    fn components_across_pools_share_one_budget_of_three_hundred_and_thirty() {
        // 163 Sapling spends with two outputs, plus 165 Ironwood actions.
        assert!(check_after_nu7(&[(NotePool::Sapling, 163), (NotePool::Ironwood, 165)]).is_ok());
        assert_eq!(
            exceeded(&check_after_nu7(&[
                (NotePool::Sapling, 163),
                (NotePool::Ironwood, 166)
            ])),
            Some((ShieldedBlockLimit::AllShieldedPools, 331))
        );
    }

    #[test]
    fn limits_do_not_bind_before_nu7() {
        assert!(
            check_zip218_limits(
                &regtest_with_nu7_at_fifty(),
                &proposal_spending(&[(NotePool::Orchard, 330)], NU7_ACTIVATION_HEIGHT - 1),
            )
            .is_ok()
        );
    }

    #[test]
    fn exceeding_a_limit_is_not_retryable() {
        assert_eq!(
            ProposalRejectionKind::ShieldedLimitExceeded {
                limit: ShieldedBlockLimit::OrchardActions,
                component_count: 331,
            }
            .posture(),
            zally_core::FailurePosture::NotRetryable
        );
    }
}
