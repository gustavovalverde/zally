//! Pre-selection filter that drops UTXOs locked by an in-flight wallet broadcast.
//!
//! `FilteredWalletDb` wraps the sqlite `WalletDb` passed to
//! [`zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address`]
//! and [`zcash_client_backend::data_api::wallet::propose_shielding`]. Only
//! [`InputSource::get_spendable_transparent_outputs`] is overridden; every
//! other [`InputSource`] and [`WalletRead`] method delegates to the inner
//! `WalletDb`. The override removes outpoints already committed to a
//! still-unconfirmed wallet-owned transaction so upstream proposal
//! construction cannot select them a second time.
//!
//! `get_spendable_transparent_outputs_for_addresses` (the batched gather
//! `propose_shielding` actually calls) is deliberately left unoverridden: the
//! trait default fans out to `get_spendable_transparent_outputs` per address,
//! which routes through this type's filtered override. Overriding it to
//! delegate straight to `inner` would bypass the exclusion filter.
//! `select_spendable_transparent_outputs` is left unoverridden too; it panics
//! via the trait default, which is safe today because Zally only proposes
//! `TransparentSpendPolicy::ShieldedOnly` transfers and that policy never
//! reaches this gather.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;

use rand::rngs::OsRng;
use secrecy::SecretVec;
use tracing::info;
use zally_core::{AccountId, Network, NetworkParameters, OutPoint, TxId as ZallyTxId};
use zcash_client_backend::data_api::error::FindAccountForAddressError;
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::data_api::wallet::{ConfirmationsPolicy, TargetHeight};
use zcash_client_backend::data_api::{
    AccountMeta, AddressInfo, BlockMetadata, CoinbaseFilter, InputSource, NoteFilter,
    NullifierQuery, ReceivedNotes, ReceivedTransactionOutput, SeedRelevance, TargetValue,
    TransactionDataRequest, TransparentBalances, WalletRead, WalletSummary, Zip32Derivation,
};
use zcash_client_backend::wallet::{
    Note, NoteId, ReceivedNote, TransparentAddressMetadata, WalletTransparentOutput,
};
use zcash_client_sqlite::WalletDb;
use zcash_client_sqlite::util::SystemClock;
use zcash_keys::address::{Address, UnifiedAddress};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedFullViewingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::{self, BlockHeight};
use zcash_protocol::memo::Memo;
use zcash_protocol::{ShieldedPool, TxId};
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::bundle::OutPoint as UpstreamOutPoint;

type Db = WalletDb<rusqlite::Connection, NetworkParameters, SystemClock, OsRng>;

/// `WalletDb` wrapper that hides outpoints reserved by a pending broadcast from
/// upstream input selection.
///
/// The excluded set uses [`zally_core::OutPoint`] because the upstream
/// `zcash_transparent::bundle::OutPoint` does not implement `Hash`.
pub(crate) struct FilteredWalletDb<'inner> {
    pub(crate) inner: &'inner mut Db,
    pub(crate) excluded_outpoints: HashSet<OutPoint>,
    pub(crate) network: Network,
    pub(crate) account_id: AccountId,
}

impl InputSource for FilteredWalletDb<'_> {
    type Error = <Db as InputSource>::Error;
    type AccountId = <Db as InputSource>::AccountId;
    type NoteRef = <Db as InputSource>::NoteRef;

    fn get_spendable_note(
        &self,
        txid: &TxId,
        protocol: ShieldedPool,
        index: u32,
        target_height: TargetHeight,
    ) -> Result<Option<ReceivedNote<Self::NoteRef, Note>>, Self::Error> {
        <Db as InputSource>::get_spendable_note(self.inner, txid, protocol, index, target_height)
    }

    fn select_spendable_notes(
        &self,
        account: Self::AccountId,
        target_value: TargetValue,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        exclude: &[Self::NoteRef],
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        <Db as InputSource>::select_spendable_notes(
            self.inner,
            account,
            target_value,
            sources,
            target_height,
            confirmations_policy,
            exclude,
        )
    }

    fn select_unspent_notes(
        &self,
        account: Self::AccountId,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        <Db as InputSource>::select_unspent_notes(
            self.inner,
            account,
            sources,
            target_height,
            exclude,
        )
    }

    fn get_account_metadata(
        &self,
        account: Self::AccountId,
        selector: &NoteFilter,
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
    ) -> Result<AccountMeta, Self::Error> {
        <Db as InputSource>::get_account_metadata(
            self.inner,
            account,
            selector,
            target_height,
            exclude,
        )
    }

    fn get_unspent_transparent_output(
        &self,
        outpoint: &UpstreamOutPoint,
        target_height: TargetHeight,
    ) -> Result<Option<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        <Db as InputSource>::get_unspent_transparent_output(self.inner, outpoint, target_height)
    }

    fn get_spendable_transparent_outputs(
        &self,
        address: &TransparentAddress,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        output_filter: CoinbaseFilter,
    ) -> Result<Vec<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        let all = <Db as InputSource>::get_spendable_transparent_outputs(
            self.inner,
            address,
            target_height,
            confirmations_policy,
            output_filter,
        )?;
        if self.excluded_outpoints.is_empty() {
            return Ok(all);
        }
        let total_count = all.len();
        let filtered: Vec<WalletTransparentOutput<Self::AccountId>> = all
            .into_iter()
            .filter(|utxo| {
                let upstream = utxo.outpoint();
                let zally_outpoint =
                    OutPoint::new(ZallyTxId::from_bytes(*upstream.hash()), upstream.n());
                !self.excluded_outpoints.contains(&zally_outpoint)
            })
            .collect();
        let excluded_count = total_count - filtered.len();
        if excluded_count > 0 {
            let excluded = u32::try_from(excluded_count).unwrap_or(u32::MAX);
            let pending = u32::try_from(self.excluded_outpoints.len()).unwrap_or(u32::MAX);
            info!(
                target: "zally::storage",
                event = "transparent_inputs_filtered_pending_broadcast",
                network = ?self.network,
                account_id = %self.account_id.as_uuid(),
                excluded_count = excluded,
                pending_broadcast_count = pending,
                "filtered transparent inputs locked by pending broadcasts",
            );
        }
        Ok(filtered)
    }
}

impl WalletRead for FilteredWalletDb<'_> {
    type Error = <Db as WalletRead>::Error;
    type AccountId = <Db as WalletRead>::AccountId;
    type Account = <Db as WalletRead>::Account;

    fn get_account_ids(&self) -> Result<Vec<Self::AccountId>, Self::Error> {
        <Db as WalletRead>::get_account_ids(self.inner)
    }

    fn get_account(
        &self,
        account_id: Self::AccountId,
    ) -> Result<Option<Self::Account>, Self::Error> {
        <Db as WalletRead>::get_account(self.inner, account_id)
    }

    fn get_derived_account(
        &self,
        derivation: &Zip32Derivation,
    ) -> Result<Option<Self::Account>, Self::Error> {
        <Db as WalletRead>::get_derived_account(self.inner, derivation)
    }

    fn validate_seed(
        &self,
        account_id: Self::AccountId,
        seed: &SecretVec<u8>,
    ) -> Result<bool, Self::Error> {
        <Db as WalletRead>::validate_seed(self.inner, account_id, seed)
    }

    fn seed_relevance_to_derived_accounts(
        &self,
        seed: &SecretVec<u8>,
    ) -> Result<SeedRelevance<Self::AccountId>, Self::Error> {
        <Db as WalletRead>::seed_relevance_to_derived_accounts(self.inner, seed)
    }

    fn get_account_for_ufvk(
        &self,
        ufvk: &UnifiedFullViewingKey,
    ) -> Result<Option<Self::Account>, Self::Error> {
        <Db as WalletRead>::get_account_for_ufvk(self.inner, ufvk)
    }

    fn list_addresses(&self, account: Self::AccountId) -> Result<Vec<AddressInfo>, Self::Error> {
        <Db as WalletRead>::list_addresses(self.inner, account)
    }

    fn find_account_for_address<P: consensus::Parameters>(
        &self,
        params: &P,
        address: &Address,
    ) -> Result<Option<Self::AccountId>, FindAccountForAddressError<Self::Error>> {
        <Db as WalletRead>::find_account_for_address(self.inner, params, address)
    }

    fn get_last_generated_address_matching(
        &self,
        account: Self::AccountId,
        address_filter: UnifiedAddressRequest,
    ) -> Result<Option<UnifiedAddress>, Self::Error> {
        <Db as WalletRead>::get_last_generated_address_matching(self.inner, account, address_filter)
    }

    fn get_account_birthday(&self, account: Self::AccountId) -> Result<BlockHeight, Self::Error> {
        <Db as WalletRead>::get_account_birthday(self.inner, account)
    }

    fn get_wallet_birthday(&self) -> Result<Option<BlockHeight>, Self::Error> {
        <Db as WalletRead>::get_wallet_birthday(self.inner)
    }

    fn get_wallet_summary(
        &self,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Option<WalletSummary<Self::AccountId>>, Self::Error> {
        <Db as WalletRead>::get_wallet_summary(self.inner, confirmations_policy)
    }

    fn chain_height(&self) -> Result<Option<BlockHeight>, Self::Error> {
        <Db as WalletRead>::chain_height(self.inner)
    }

    fn get_block_hash(&self, block_height: BlockHeight) -> Result<Option<BlockHash>, Self::Error> {
        <Db as WalletRead>::get_block_hash(self.inner, block_height)
    }

    fn block_metadata(&self, height: BlockHeight) -> Result<Option<BlockMetadata>, Self::Error> {
        <Db as WalletRead>::block_metadata(self.inner, height)
    }

    fn block_fully_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        <Db as WalletRead>::block_fully_scanned(self.inner)
    }

    fn get_max_height_hash(&self) -> Result<Option<(BlockHeight, BlockHash)>, Self::Error> {
        <Db as WalletRead>::get_max_height_hash(self.inner)
    }

    fn block_max_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        <Db as WalletRead>::block_max_scanned(self.inner)
    }

    fn suggest_scan_ranges(&self) -> Result<Vec<ScanRange>, Self::Error> {
        <Db as WalletRead>::suggest_scan_ranges(self.inner)
    }

    fn get_target_and_anchor_heights(
        &self,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<(TargetHeight, BlockHeight)>, Self::Error> {
        <Db as WalletRead>::get_target_and_anchor_heights(self.inner, min_confirmations)
    }

    fn get_tx_height(&self, txid: TxId) -> Result<Option<BlockHeight>, Self::Error> {
        <Db as WalletRead>::get_tx_height(self.inner, txid)
    }

    fn get_unified_full_viewing_keys(
        &self,
    ) -> Result<HashMap<Self::AccountId, UnifiedFullViewingKey>, Self::Error> {
        <Db as WalletRead>::get_unified_full_viewing_keys(self.inner)
    }

    fn get_memo(&self, note_id: NoteId) -> Result<Option<Memo>, Self::Error> {
        <Db as WalletRead>::get_memo(self.inner, note_id)
    }

    fn get_transaction(&self, txid: TxId) -> Result<Option<Transaction>, Self::Error> {
        <Db as WalletRead>::get_transaction(self.inner, txid)
    }

    fn get_sapling_nullifiers(
        &self,
        query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, sapling::Nullifier)>, Self::Error> {
        <Db as WalletRead>::get_sapling_nullifiers(self.inner, query)
    }

    fn get_orchard_nullifiers(
        &self,
        query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, orchard::note::Nullifier)>, Self::Error> {
        <Db as WalletRead>::get_orchard_nullifiers(self.inner, query)
    }

    fn get_transparent_receivers(
        &self,
        account: Self::AccountId,
        include_change: bool,
        include_standalone: bool,
    ) -> Result<HashMap<TransparentAddress, TransparentAddressMetadata>, Self::Error> {
        <Db as WalletRead>::get_transparent_receivers(
            self.inner,
            account,
            include_change,
            include_standalone,
        )
    }

    fn get_ephemeral_transparent_receivers(
        &self,
        account: Self::AccountId,
        exposure_depth: u32,
        exclude_used: bool,
    ) -> Result<HashMap<TransparentAddress, TransparentAddressMetadata>, Self::Error> {
        <Db as WalletRead>::get_ephemeral_transparent_receivers(
            self.inner,
            account,
            exposure_depth,
            exclude_used,
        )
    }

    fn get_transparent_balances(
        &self,
        account: Self::AccountId,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<TransparentBalances, Self::Error> {
        <Db as WalletRead>::get_transparent_balances(
            self.inner,
            account,
            target_height,
            confirmations_policy,
        )
    }

    fn get_transparent_address_metadata(
        &self,
        account: Self::AccountId,
        address: &TransparentAddress,
    ) -> Result<Option<TransparentAddressMetadata>, Self::Error> {
        <Db as WalletRead>::get_transparent_address_metadata(self.inner, account, address)
    }

    fn utxo_query_height(&self, account: Self::AccountId) -> Result<BlockHeight, Self::Error> {
        <Db as WalletRead>::utxo_query_height(self.inner, account)
    }

    fn transaction_data_requests(&self) -> Result<Vec<TransactionDataRequest>, Self::Error> {
        <Db as WalletRead>::transaction_data_requests(self.inner)
    }

    fn get_received_outputs(
        &self,
        txid: TxId,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Vec<ReceivedTransactionOutput>, Self::Error> {
        <Db as WalletRead>::get_received_outputs(
            self.inner,
            txid,
            target_height,
            confirmations_policy,
        )
    }
}
