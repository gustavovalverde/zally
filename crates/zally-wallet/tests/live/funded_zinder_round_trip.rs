//! Funded Zinder-backed wallet round trip.

use std::env;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rand::rngs::OsRng;
use secp256k1::{PublicKey, Secp256k1};
use serde_json::{Value, json};
use zally_chain::{
    ChainSource, ShieldedPool, SubmitOutcome, Submitter, ZinderChainSource, ZinderRemoteOptions,
    ZinderSubmitter,
};
use zally_core::{
    AccountId, BlockHeight, IdempotencyKey, Network, PaymentRecipient, TxId, Zatoshis,
};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_testkit::{
    LiveTestError, TempWalletPath, init, require_live, require_network, require_zinder_endpoint,
};
use zally_wallet::{
    ExportPaymentDisclosurePlan, ProposalPlan, SendPaymentPlan, ShieldTransparentPlan, SyncDriver,
    SyncDriverOptions, SyncHandle, SyncSnapshotStream, SyncStatus, Wallet, WalletError,
};
use zcash_keys::address::Address;
use zcash_payment_disclosure::{PaymentDisclosureProfile, verify_disclosure};
use zcash_primitives::transaction::builder::{BuildConfig, Builder};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    consensus::{BlockHeight as ConsensusBlockHeight, NetworkType},
    local_consensus::LocalNetwork as ZallyLocalNetwork,
    value::Zatoshis as UpstreamZatoshis,
};
use zcash_transparent::{
    address::TransparentAddress as ZallyTransparentAddress,
    builder::TransparentSigningSet,
    bundle::{OutPoint, TxOut},
    keys::{AccountPrivKey, NonHardenedChildIndex},
};
use zip32::AccountId as TransparentAccountId;

const NODE_JSON_RPC_ADDR_ENV: &str = "ZALLY_TEST_NODE_JSON_RPC_ADDR";
const NODE_RPC_USER_ENV: &str = "ZALLY_TEST_NODE_RPC_USER";
const NODE_RPC_PASSWORD_ENV: &str = "ZALLY_TEST_NODE_RPC_PASSWORD";
const SHIELDING_THRESHOLD_ZAT_ENV: &str = "ZALLY_TEST_SHIELDING_THRESHOLD_ZAT";
const SEND_ZAT_ENV: &str = "ZALLY_TEST_SEND_ZAT";
const TRANSPARENT_FUNDING_TEST_SEED: [u8; 32] = [0x42_u8; 32];
const ZIP317_FEE_ONE_IN_ONE_OUT_ZAT: u64 = 10_000;
// The wallet's shielding policy treats every chain-ingested transparent input as
// untrusted and requires COINBASE_MATURITY (100) confirmations, so the funding
// output only becomes shieldable once this many blocks sit on top of it.
const TRANSPARENT_FUNDING_CONFIRMATION_BLOCKS: u32 = 100;
const SHIELDED_SPEND_CONFIRMATION_BLOCKS: u32 = 10;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn funded_wallet_syncs_sends_and_submits_pczt_with_zinder() -> Result<(), TestError> {
    let _guard = init();
    require_live()?;

    let mut round_trip = FundedZinderRoundTrip::open().await?;
    let funding_tx_id = round_trip.submit_transparent_funding().await?;
    let shield_tx_id = round_trip.shield_transparent_funds().await?;
    assert_ne!(funding_tx_id, shield_tx_id);

    let send_tx_id = round_trip.submit_shielded_payment().await?;
    let pczt_tx_id = round_trip.submit_pczt_payment().await?;
    assert_ne!(send_tx_id, pczt_tx_id);
    round_trip.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn shielding_excludes_pending_broadcast_inputs() -> Result<(), TestError> {
    let _guard = init();
    require_live()?;

    let mut round_trip = FundedZinderRoundTrip::open().await?;
    let _funding_tx_id = round_trip.submit_transparent_funding().await?;

    let first_shield = round_trip.shield_transparent_funds_no_mine().await?;

    let pending = round_trip
        .wallet
        .get_pending_transparent_inputs(round_trip.account_id)
        .await?;
    assert!(
        !pending.inputs.is_empty(),
        "after a successful broadcast the pending-broadcast snapshot must report at least one locked outpoint"
    );

    let second_outcome = round_trip
        .attempt_shield_with_idempotency("t3-duplicate-protect")
        .await;
    match second_outcome {
        Err(WalletError::InsufficientBalance { .. } | WalletError::ProposalRejected { .. }) => {}
        Err(other) => {
            return Err(TestError::Unexpected {
                reason: format!(
                    "expected InsufficientBalance or ProposalRejected on second immediate shield, got {other:?}"
                ),
            });
        }
        Ok(second_outcome) => {
            return Err(TestError::Unexpected {
                reason: format!(
                    "expected the second immediate shield to be refused; it produced a duplicate \
                     broadcast (first shield tx_id bytes {:?}, second {:?})",
                    first_shield.tx_id().as_bytes(),
                    second_outcome.tx_id().as_bytes(),
                ),
            });
        }
    }

    round_trip.close().await?;
    Ok(())
}

struct FundedZinderRoundTrip {
    _wallet_path: TempWalletPath,
    miner: JsonRpcClient,
    wallet: Wallet,
    account_id: AccountId,
    network: Network,
    submitter: ZinderSubmitter,
    sync_handle: SyncHandle,
    sync_snapshots: SyncSnapshotStream,
    funding_local_network: ZallyLocalNetwork,
}

impl FundedZinderRoundTrip {
    async fn open() -> Result<Self, TestError> {
        let miner = JsonRpcClient::from_node_env()?;
        let requested_network = require_network()?;
        require_regtest(requested_network)?;
        let (network, funding_local_network) = miner.regtest_networks_from_node()?;
        let endpoint = require_zinder_endpoint()?;

        let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions { endpoint, network })?;
        let submitter = chain.submitter();
        let tip = chain.safe_chain_tip().await?;
        let (wallet_path, wallet, account_id) = create_wallet_at_tip(&chain, network, tip).await?;

        let chain_source: Arc<dyn ChainSource> = Arc::new(chain.clone());
        let driver = SyncDriver::new(
            wallet.clone(),
            chain_source,
            SyncDriverOptions::default()
                .with_poll_interval_ms(250)
                .with_max_sync_iterations_per_wake_count(16),
        )?;
        let sync_handle = driver.sync_continuously();
        let sync_snapshots = sync_handle.observe_status();

        Ok(Self {
            _wallet_path: wallet_path,
            miner,
            wallet,
            account_id,
            network,
            submitter,
            sync_handle,
            sync_snapshots,
            funding_local_network,
        })
    }

    async fn submit_transparent_funding(&mut self) -> Result<TxId, TestError> {
        let receive_ua = self
            .wallet
            .derive_next_address_with_transparent(self.account_id)
            .await?;
        let receive_transparent = receive_ua
            .transparent()
            .copied()
            .ok_or(TestError::TransparentReceiverMissing)?;
        let funding_tx = build_regtest_funding_transaction(
            &self.miner,
            &receive_transparent,
            self.funding_local_network,
        )?;
        let funding_tx_id = require_accepted(self.submitter.submit(&funding_tx).await?, "funding")?;
        self.miner
            .generate_blocks(TRANSPARENT_FUNDING_CONFIRMATION_BLOCKS)?;
        let funded_height = self.miner.safe_chain_tip_height()?;
        wait_until_transparent_utxo_at_tip(&mut self.sync_snapshots, funded_height).await?;
        Ok(funding_tx_id)
    }

    async fn shield_transparent_funds(&mut self) -> Result<TxId, TestError> {
        let shielding_threshold_zat = shielding_threshold_zat_from_env()?;
        let shield_idempotency = IdempotencyKey::try_from("t3-funded-shield")?;
        let shield_outcome = self
            .wallet
            .shield_transparent_funds(
                ShieldTransparentPlan::new(
                    self.account_id,
                    shield_idempotency,
                    shielding_threshold_zat,
                    &self.submitter,
                )
                .with_destination_pool(ShieldedPool::Ironwood),
            )
            .await?;
        self.miner
            .generate_blocks(SHIELDED_SPEND_CONFIRMATION_BLOCKS)?;
        let shielded_height = self.miner.safe_chain_tip_height()?;
        wait_until_at_tip_at_or_above(&mut self.sync_snapshots, shielded_height).await?;
        let receives = self.wallet.list_shielded_receives(self.account_id).await?;
        assert!(
            !receives.is_empty(),
            "funded live test expected Zally to observe its shielded self-send"
        );
        Ok(shield_outcome.tx_id())
    }

    async fn submit_shielded_payment(&mut self) -> Result<TxId, TestError> {
        let send_zat = send_zat_from_env()?;
        let send_recipient =
            derive_unified_recipient(&self.wallet, self.account_id, self.network).await?;
        let send_idempotency = IdempotencyKey::try_from("t3-funded-send")?;
        let send_outcome = self
            .wallet
            .send_payment(SendPaymentPlan::conventional(
                self.account_id,
                send_idempotency,
                send_recipient,
                send_zat,
                &self.submitter,
            ))
            .await?;
        self.miner
            .generate_blocks(SHIELDED_SPEND_CONFIRMATION_BLOCKS)?;
        let send_height = self.miner.safe_chain_tip_height()?;
        wait_until_at_tip_at_or_above(&mut self.sync_snapshots, send_height).await?;
        Ok(send_outcome.tx_id())
    }

    async fn submit_pczt_payment(&mut self) -> Result<TxId, TestError> {
        let send_zat = send_zat_from_env()?;
        let pczt_recipient =
            derive_unified_recipient(&self.wallet, self.account_id, self.network).await?;
        let disclosure_recipient = pczt_recipient.clone();
        let pczt = self
            .wallet
            .propose_pczt(
                ProposalPlan::conventional(self.account_id, pczt_recipient, send_zat, None)
                    .with_source_pool(ShieldedPool::Ironwood),
                None,
            )
            .await?;
        let proven_pczt = self.wallet.prove_pczt(pczt).await?;
        let signed_pczt = self.wallet.sign_pczt(proven_pczt).await?;
        let pczt_outcome = self
            .wallet
            .extract_and_submit_pczt(signed_pczt, &self.submitter)
            .await?;
        self.miner.generate_blocks(1)?;
        let pczt_height = self.miner.safe_chain_tip_height()?;
        wait_until_at_tip_at_or_above(&mut self.sync_snapshots, pczt_height).await?;
        let transaction_id = pczt_outcome.tx_id();
        let disclosure = self
            .wallet
            .export_payment_disclosure(ExportPaymentDisclosurePlan::new(
                transaction_id,
                disclosure_recipient.clone(),
                send_zat,
                b"zally-regtest-ironwood-disclosure".to_vec(),
                PaymentDisclosureProfile::ZallyIronwood,
            ))
            .await?;
        let raw_transaction_bytes = self.miner.raw_transaction_bytes(transaction_id)?;
        let prover =
            LocalTxProver::with_default_location().ok_or(TestError::SaplingParametersMissing)?;
        let (spend_verifying_key, _) = prover.verifying_keys();
        let prepared_spend_verifying_key = spend_verifying_key.prepare();
        let evidence = verify_disclosure(
            disclosure.portable(),
            &raw_transaction_bytes,
            ConsensusBlockHeight::from_u32(pczt_height.as_u32()),
            &self.network.to_parameters(),
            &prepared_spend_verifying_key,
        )?;
        let Some(Address::Unified(expected_unified_address)) = Address::decode(
            &self.network.to_parameters(),
            disclosure_recipient.encoded(),
        ) else {
            return Err(TestError::Unexpected {
                reason: "Ironwood disclosure recipient did not decode as a Unified Address"
                    .to_owned(),
            });
        };
        let expected_recipient =
            expected_unified_address
                .orchard()
                .copied()
                .ok_or_else(|| TestError::Unexpected {
                    reason: "Ironwood disclosure recipient did not carry an Orchard receiver"
                        .to_owned(),
                })?;
        assert_eq!(
            evidence.transaction_id(),
            disclosure.portable().transaction_id()
        );
        assert!(!evidence.ironwood_spends().is_empty());
        assert_eq!(evidence.ironwood_outputs().len(), 1);
        assert_eq!(
            evidence.ironwood_outputs()[0].recipient(),
            expected_recipient
        );
        assert_eq!(
            evidence.ironwood_outputs()[0].amount_zat(),
            send_zat.as_u64()
        );
        Ok(transaction_id)
    }

    // &mut self stays even though no field is mutated; the live test holds a
    // SyncSnapshotStream which is !Sync, so &self would make this future not-Send.
    #[allow(
        clippy::needless_pass_by_ref_mut,
        reason = "Sync auto-trait coercion requires exclusive borrow on this !Sync struct"
    )]
    async fn shield_transparent_funds_no_mine(
        &mut self,
    ) -> Result<zally_wallet::SendOutcome, TestError> {
        let shielding_threshold_zat = shielding_threshold_zat_from_env()?;
        let shield_idempotency = IdempotencyKey::try_from("t3-duplicate-protect-first")?;
        let outcome = self
            .wallet
            .shield_transparent_funds(ShieldTransparentPlan::new(
                self.account_id,
                shield_idempotency,
                shielding_threshold_zat,
                &self.submitter,
            ))
            .await?;
        Ok(outcome)
    }

    #[allow(
        clippy::needless_pass_by_ref_mut,
        reason = "Sync auto-trait coercion requires exclusive borrow on this !Sync struct"
    )]
    async fn attempt_shield_with_idempotency(
        &mut self,
        idempotency_label: &str,
    ) -> Result<zally_wallet::SendOutcome, WalletError> {
        let shielding_threshold_zat = shielding_threshold_zat_from_env().unwrap_or_else(|_| {
            Zatoshis::try_from(1_000_000_u64).unwrap_or_else(|_| Zatoshis::zero())
        });
        let idempotency = IdempotencyKey::try_from(idempotency_label).map_err(|err| {
            WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            }
        })?;
        self.wallet
            .shield_transparent_funds(ShieldTransparentPlan::new(
                self.account_id,
                idempotency,
                shielding_threshold_zat,
                &self.submitter,
            ))
            .await
    }

    async fn close(self) -> Result<(), TestError> {
        self.sync_handle.close().await?;
        Ok(())
    }
}

async fn create_wallet_at_tip(
    chain: &ZinderChainSource,
    network: Network,
    tip_height: BlockHeight,
) -> Result<(TempWalletPath, Wallet, AccountId), TestError> {
    let temp = TempWalletPath::create()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let birthday = BlockHeight::from(tip_height.as_u32().saturating_sub(10).max(1));
    let (wallet, account_id, _mnemonic) = Wallet::builder(network, sealing, storage)
        .create(chain, birthday)
        .await?;
    Ok((temp, wallet, account_id))
}

async fn derive_unified_recipient(
    wallet: &Wallet,
    account_id: AccountId,
    network: Network,
) -> Result<PaymentRecipient, WalletError> {
    let encoded = wallet
        .derive_next_address(account_id)
        .await?
        .encode(&network.to_parameters());
    Ok(PaymentRecipient::UnifiedAddress { encoded, network })
}

async fn wait_until_at_tip_at_or_above(
    snapshots: &mut SyncSnapshotStream,
    min_tip_height: BlockHeight,
) -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(snapshot) = snapshots.next().await {
            if matches!(
                snapshot.sync_status,
                SyncStatus::AtTip { safe_chain_tip_height }
                    if safe_chain_tip_height.as_u32() >= min_tip_height.as_u32()
            ) {
                return Ok(());
            }
        }
        Err(TestError::SyncStreamClosed)
    })
    .await
    .map_err(|_| TestError::SyncTimeout)?
}

async fn wait_until_transparent_utxo_at_tip(
    snapshots: &mut SyncSnapshotStream,
    min_tip_height: BlockHeight,
) -> Result<(), TestError> {
    // Maturing the funding output to COINBASE_MATURITY means the wallet must scan
    // that many freshly mined blocks, and the Zinder backend surfaces them to its
    // secondary at roughly one regtest block per second, so this wait is sized well
    // above the shorter at-tip waits.
    tokio::time::timeout(Duration::from_mins(4), async {
        while let Some(snapshot) = snapshots.next().await {
            let is_at_target_tip = matches!(
                snapshot.sync_status,
                SyncStatus::AtTip { safe_chain_tip_height }
                    if safe_chain_tip_height.as_u32() >= min_tip_height.as_u32()
            );
            let has_transparent_utxo = snapshot
                .last_outcome
                .is_some_and(|outcome| outcome.transparent_utxo_count > 0);
            if is_at_target_tip && has_transparent_utxo {
                return Ok(());
            }
        }
        Err(TestError::SyncStreamClosed)
    })
    .await
    .map_err(|_| TestError::SyncTimeout)?
}

fn build_regtest_funding_transaction(
    miner: &JsonRpcClient,
    recipient: &ZallyTransparentAddress,
    funding_local_network: ZallyLocalNetwork,
) -> Result<Vec<u8>, TestError> {
    let account_key = AccountPrivKey::from_seed(
        &funding_local_network,
        &TRANSPARENT_FUNDING_TEST_SEED,
        TransparentAccountId::ZERO,
    )
    .map_err(|error| TestError::TransparentFunding {
        reason: format!("could not derive funding account key: {error}"),
    })?;
    let secret_key = account_key
        .derive_external_secret_key(NonHardenedChildIndex::ZERO)
        .map_err(|error| TestError::TransparentFunding {
            reason: format!("could not derive funding secret key: {error}"),
        })?;
    let secp = Secp256k1::new();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let funding_address = ZallyTransparentAddress::from_pubkey(&public_key)
        .to_zcash_address(NetworkType::Regtest)
        .encode();
    let coinbase = miner.locate_spendable_coinbase(&funding_address)?;
    if coinbase.value_zats <= ZIP317_FEE_ONE_IN_ONE_OUT_ZAT {
        return Err(TestError::TransparentFunding {
            reason: format!(
                "coinbase value {} does not exceed the ZIP-317 fee {ZIP317_FEE_ONE_IN_ONE_OUT_ZAT}",
                coinbase.value_zats
            ),
        });
    }
    let coinbase_amount = UpstreamZatoshis::from_u64(coinbase.value_zats).map_err(|error| {
        TestError::TransparentFunding {
            reason: format!("coinbase value was invalid: {error}"),
        }
    })?;
    let send_amount = UpstreamZatoshis::from_u64(
        coinbase.value_zats - ZIP317_FEE_ONE_IN_ONE_OUT_ZAT,
    )
    .map_err(|error| TestError::TransparentFunding {
        reason: format!("funding output value was invalid: {error}"),
    })?;
    let mut signing_set = TransparentSigningSet::new();
    let signing_public_key = signing_set.add_key(secret_key);
    let coin = TxOut::new(
        coinbase_amount,
        ZallyTransparentAddress::from_pubkey(&signing_public_key)
            .script()
            .into(),
    );
    let outpoint = OutPoint::new(coinbase.txid_be, coinbase.vout);
    let mut builder = Builder::new(
        funding_local_network,
        ConsensusBlockHeight::from_u32(coinbase.target_height),
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: None,
            ironwood_anchor: None,
            orchard_pool_bundle_type: orchard::builder::BundleType::DEFAULT,
        },
    );
    builder
        .add_transparent_p2pkh_input(signing_public_key, outpoint, coin)
        .map_err(|error| TestError::TransparentFunding {
            reason: format!("could not add funding input: {error}"),
        })?;
    builder
        .add_transparent_output(recipient, send_amount)
        .map_err(|error| TestError::TransparentFunding {
            reason: format!("could not add funding output: {error}"),
        })?;
    let built = builder
        .mock_build(&signing_set, &[], &[], OsRng)
        .map_err(|error| TestError::TransparentFunding {
            reason: format!("could not build funding transaction: {error}"),
        })?;
    let mut bytes = Vec::new();
    built.transaction().write(&mut bytes)?;
    Ok(bytes)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive submit outcomes map unknown variants to rejected test errors"
)]
fn require_accepted(outcome: SubmitOutcome, context: &'static str) -> Result<TxId, TestError> {
    match outcome {
        SubmitOutcome::Accepted { tx_id }
        | SubmitOutcome::Duplicate { tx_id }
        | SubmitOutcome::Queued { tx_id } => Ok(tx_id),
        SubmitOutcome::Rejected { reason, detail } => Err(TestError::SubmitRejected {
            context,
            reason: format!("{reason:?}: {detail}"),
        }),
        _ => Err(TestError::SubmitRejected {
            context,
            reason: "submitter returned an unrecognised outcome".to_owned(),
        }),
    }
}

fn shielding_threshold_zat_from_env() -> Result<Zatoshis, TestError> {
    let raw = env::var(SHIELDING_THRESHOLD_ZAT_ENV).unwrap_or_else(|_| "1000000".to_owned());
    zatoshis_from_env(SHIELDING_THRESHOLD_ZAT_ENV, &raw)
}

fn send_zat_from_env() -> Result<Zatoshis, TestError> {
    let raw = env::var(SEND_ZAT_ENV).unwrap_or_else(|_| "10000".to_owned());
    zatoshis_from_env(SEND_ZAT_ENV, &raw)
}

fn zatoshis_from_env(var: &'static str, raw: &str) -> Result<Zatoshis, TestError> {
    let zatoshis = raw.parse::<u64>().map_err(|err| TestError::InvalidEnv {
        var,
        reason: err.to_string(),
    })?;
    Zatoshis::try_from(zatoshis).map_err(|err| TestError::InvalidEnv {
        var,
        reason: err.to_string(),
    })
}

fn require_regtest(network: Network) -> Result<(), TestError> {
    if matches!(network, Network::Regtest(_)) {
        Ok(())
    } else {
        Err(TestError::RegtestRequired)
    }
}

struct TestCoinbase {
    txid_be: [u8; 32],
    vout: u32,
    value_zats: u64,
    target_height: u32,
}

struct AddressUtxo {
    txid: String,
    output_index: u32,
    satoshis: u64,
    height: u32,
}

struct NodeUpgrade {
    activation_height: u32,
    name: String,
}

struct JsonRpcClient {
    json_rpc_addr: String,
    rpc_auth: Option<JsonRpcAuth>,
}

struct JsonRpcAuth {
    rpc_user: String,
    rpc_password: String,
}

impl JsonRpcClient {
    fn from_node_env() -> Result<Self, TestError> {
        Ok(Self {
            json_rpc_addr: env::var(NODE_JSON_RPC_ADDR_ENV)
                .unwrap_or_else(|_| "http://127.0.0.1:39232/".to_owned()),
            rpc_auth: optional_node_auth()?,
        })
    }

    fn generate_blocks(&self, block_count: u32) -> Result<(), TestError> {
        let _hashes = self.call("generate", &json!([block_count]))?;
        Ok(())
    }

    fn safe_chain_tip_height(&self) -> Result<BlockHeight, TestError> {
        let rpc_result = self.call("getblockchaininfo", &json!([]))?;
        let blocks = rpc_result
            .get("blocks")
            .and_then(Value::as_u64)
            .ok_or_else(|| TestError::RpcShape {
                method: "getblockchaininfo",
                reason: "result.blocks was not an unsigned integer".to_owned(),
            })?;
        let height = u32::try_from(blocks).map_err(|err| TestError::RpcShape {
            method: "getblockchaininfo",
            reason: format!("result.blocks did not fit u32: {err}"),
        })?;
        Ok(BlockHeight::from(height))
    }

    fn raw_transaction_bytes(&self, tx_id: TxId) -> Result<Vec<u8>, TestError> {
        let rpc_result = self.call("getrawtransaction", &json!([tx_id.to_string(), 0]))?;
        let transaction_hex = rpc_result.as_str().ok_or_else(|| TestError::RpcShape {
            method: "getrawtransaction",
            reason: "result was not a hex string".to_owned(),
        })?;
        hex::decode(transaction_hex).map_err(|err| TestError::RpcShape {
            method: "getrawtransaction",
            reason: format!("result was not valid transaction hex: {err}"),
        })
    }

    fn regtest_networks_from_node(&self) -> Result<(Network, ZallyLocalNetwork), TestError> {
        let node_upgrades = self.node_upgrade_activations()?;
        let local_network = zally_local_network_from_upgrades(&node_upgrades);
        Ok((Network::Regtest(local_network), local_network))
    }

    fn node_upgrade_activations(&self) -> Result<Vec<NodeUpgrade>, TestError> {
        let rpc_result = self.call("getblockchaininfo", &json!([]))?;
        let upgrades = rpc_result
            .get("upgrades")
            .and_then(Value::as_object)
            .ok_or_else(|| TestError::RpcShape {
                method: "getblockchaininfo",
                reason: "result.upgrades was not an object".to_owned(),
            })?;
        upgrades
            .iter()
            .map(|(branch_id_hex, upgrade_json)| {
                node_upgrade_from_json(branch_id_hex, upgrade_json)
            })
            .collect()
    }

    fn locate_spendable_coinbase(&self, test_address: &str) -> Result<TestCoinbase, TestError> {
        let target_height = self.safe_chain_tip_height()?.as_u32().saturating_add(1);
        let maturity_cutoff = target_height.saturating_sub(100);
        let mut utxos = self.address_utxos(test_address)?;
        utxos.sort_by_key(|utxo| utxo.satoshis);
        utxos.reverse();

        for utxo in utxos {
            if utxo.height <= maturity_cutoff
                && utxo.satoshis > ZIP317_FEE_ONE_IN_ONE_OUT_ZAT
                && self.address_utxo_is_unspent(&utxo)?
            {
                return Ok(TestCoinbase {
                    txid_be: display_txid_to_wire_bytes(&utxo.txid)?,
                    vout: utxo.output_index,
                    value_zats: utxo.satoshis,
                    target_height,
                });
            }
        }

        Err(TestError::RegtestCoinbaseUnavailable {
            address: test_address.to_owned(),
        })
    }

    fn address_utxos(&self, address: &str) -> Result<Vec<AddressUtxo>, TestError> {
        let rpc_result = self.call(
            "getaddressutxos",
            &json!([{
                "addresses": [address]
            }]),
        )?;
        let entries = rpc_result.as_array().ok_or_else(|| TestError::RpcShape {
            method: "getaddressutxos",
            reason: "result was not an array".to_owned(),
        })?;
        entries
            .iter()
            .map(address_utxo_from_json)
            .collect::<Result<Vec<_>, _>>()
    }

    fn address_utxo_is_unspent(&self, utxo: &AddressUtxo) -> Result<bool, TestError> {
        let rpc_result = self.call("gettxout", &json!([utxo.txid, utxo.output_index]))?;
        Ok(!rpc_result.is_null())
    }

    fn call(&self, method: &'static str, params: &Value) -> Result<Value, TestError> {
        let request_json = json!({
            "jsonrpc": "2.0",
            "id": "zally-live",
            "method": method,
            "params": params,
        });
        let mut command = Command::new("curl");
        command.arg("-sS").arg("--fail").arg("--max-time").arg("30");
        if let Some(auth) = &self.rpc_auth {
            command
                .arg("--user")
                .arg(format!("{}:{}", auth.rpc_user, auth.rpc_password));
        }
        let output = command
            .arg("--data-binary")
            .arg(request_json.to_string())
            .arg("-H")
            .arg("content-type: application/json")
            .arg(&self.json_rpc_addr)
            .output()
            .map_err(TestError::Io)?;
        if !output.status.success() {
            return Err(TestError::RpcCommandFailed {
                method,
                status_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let response_json: Value =
            serde_json::from_slice(&output.stdout).map_err(|err| TestError::RpcShape {
                method,
                reason: err.to_string(),
            })?;
        if !response_json.get("error").is_none_or(Value::is_null) {
            return Err(TestError::RpcShape {
                method,
                reason: response_json["error"].to_string(),
            });
        }
        response_json
            .get("result")
            .cloned()
            .ok_or_else(|| TestError::RpcShape {
                method,
                reason: "response did not carry result".to_owned(),
            })
    }
}

fn optional_node_auth() -> Result<Option<JsonRpcAuth>, TestError> {
    match (
        env::var(NODE_RPC_USER_ENV).ok(),
        env::var(NODE_RPC_PASSWORD_ENV).ok(),
    ) {
        (Some(rpc_user), Some(rpc_password))
            if !rpc_user.is_empty() && !rpc_password.is_empty() =>
        {
            Ok(Some(JsonRpcAuth {
                rpc_user,
                rpc_password,
            }))
        }
        (None, None) => Ok(None),
        _ => Err(TestError::InvalidEnv {
            var: NODE_RPC_USER_ENV,
            reason: format!("{NODE_RPC_USER_ENV} and {NODE_RPC_PASSWORD_ENV} must be set together"),
        }),
    }
}

fn zally_local_network_from_upgrades(upgrades: &[NodeUpgrade]) -> ZallyLocalNetwork {
    let activation_height = |name: &'static str| {
        upgrades
            .iter()
            .find(|upgrade| upgrade.name.eq_ignore_ascii_case(name))
            .map(|upgrade| ConsensusBlockHeight::from_u32(upgrade.activation_height))
    };
    ZallyLocalNetwork {
        overwinter: activation_height("Overwinter"),
        sapling: activation_height("Sapling"),
        blossom: activation_height("Blossom"),
        heartwood: activation_height("Heartwood"),
        canopy: activation_height("Canopy"),
        nu5: activation_height("NU5"),
        nu6: activation_height("NU6"),
        nu6_1: activation_height("NU6.1"),
        nu6_2: activation_height("NU6.2"),
        nu6_3: activation_height("NU6.3"),
    }
}

fn node_upgrade_from_json(
    branch_id_hex: &str,
    upgrade_json: &Value,
) -> Result<NodeUpgrade, TestError> {
    let name = upgrade_json
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| TestError::RpcShape {
            method: "getblockchaininfo",
            reason: format!("upgrade {branch_id_hex} did not carry a name"),
        })?
        .to_owned();
    let raw_activation_height = upgrade_json
        .get("activationheight")
        .and_then(Value::as_u64)
        .ok_or_else(|| TestError::RpcShape {
            method: "getblockchaininfo",
            reason: format!("upgrade {name} did not carry an unsigned activationheight"),
        })?;
    let activation_height =
        u32::try_from(raw_activation_height).map_err(|err| TestError::RpcShape {
            method: "getblockchaininfo",
            reason: format!("upgrade {name} activationheight did not fit u32: {err}"),
        })?;
    Ok(NodeUpgrade {
        activation_height,
        name,
    })
}

fn address_utxo_from_json(utxo_json: &Value) -> Result<AddressUtxo, TestError> {
    let txid = utxo_json
        .get("txid")
        .and_then(Value::as_str)
        .ok_or_else(|| TestError::RpcShape {
            method: "getaddressutxos",
            reason: "entry.txid was not a string".to_owned(),
        })?
        .to_owned();
    let output_index = utxo_json
        .get("outputIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| TestError::RpcShape {
            method: "getaddressutxos",
            reason: "entry.outputIndex was not an unsigned integer".to_owned(),
        })
        .and_then(|raw| {
            u32::try_from(raw).map_err(|err| TestError::RpcShape {
                method: "getaddressutxos",
                reason: format!("entry.outputIndex did not fit u32: {err}"),
            })
        })?;
    let satoshis = utxo_json
        .get("satoshis")
        .and_then(Value::as_u64)
        .ok_or_else(|| TestError::RpcShape {
            method: "getaddressutxos",
            reason: "entry.satoshis was not an unsigned integer".to_owned(),
        })?;
    let height = utxo_json
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| TestError::RpcShape {
            method: "getaddressutxos",
            reason: "entry.height was not an unsigned integer".to_owned(),
        })
        .and_then(|raw| {
            u32::try_from(raw).map_err(|err| TestError::RpcShape {
                method: "getaddressutxos",
                reason: format!("entry.height did not fit u32: {err}"),
            })
        })?;
    Ok(AddressUtxo {
        txid,
        output_index,
        satoshis,
        height,
    })
}

fn display_txid_to_wire_bytes(txid: &str) -> Result<[u8; 32], TestError> {
    let raw = txid.as_bytes();
    if raw.len() != 64 {
        return Err(TestError::InvalidTxId {
            reason: format!("expected 64 hex chars, got {}", raw.len()),
        });
    }
    let mut decoded = [0_u8; 32];
    for (index, chunk) in raw.chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
    }
    decoded.reverse();
    Ok(decoded)
}

fn hex_nibble(byte: u8) -> Result<u8, TestError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(TestError::InvalidTxId {
            reason: "txid contained a non-hex character".to_owned(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("live gate error: {0}")]
    Live(#[from] LiveTestError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {0}")]
    Chain(#[from] zally_chain::ChainSourceError),
    #[error("submitter error: {0}")]
    Submitter(#[from] zally_chain::SubmitterError),
    #[error("transparent funding transaction could not be built: {reason}")]
    TransparentFunding { reason: String },
    #[error("idempotency key error: {0}")]
    IdempotencyKey(#[from] zally_core::IdempotencyKeyError),
    #[error("funded live test requires ZALLY_NETWORK=regtest")]
    RegtestRequired,
    #[error("Zally transparent receiver was missing from the funding address")]
    TransparentReceiverMissing,
    #[error("Sapling proving parameters are not installed")]
    SaplingParametersMissing,
    #[error("payment disclosure verification failed: {0}")]
    PaymentDisclosureVerification(
        #[from] zcash_payment_disclosure::PaymentDisclosureVerificationError,
    ),
    #[error("no spendable regtest coinbase was found for transparent test address {address}")]
    RegtestCoinbaseUnavailable { address: String },
    #[error("invalid {var}: {reason}")]
    InvalidEnv { var: &'static str, reason: String },
    #[error("{context} transaction was rejected: {reason}")]
    SubmitRejected {
        context: &'static str,
        reason: String,
    },
    #[error("invalid displayed transaction id: {reason}")]
    InvalidTxId { reason: String },
    #[error("json-rpc command {method} failed with status {status_code:?}: {stderr}")]
    RpcCommandFailed {
        method: &'static str,
        status_code: Option<i32>,
        stderr: String,
    },
    #[error("json-rpc {method} returned unexpected shape: {reason}")]
    RpcShape {
        method: &'static str,
        reason: String,
    },
    #[error("sync snapshot stream closed")]
    SyncStreamClosed,
    #[error("timed out waiting for sync to reach tip")]
    SyncTimeout,
    #[error("unexpected test outcome: {reason}")]
    Unexpected { reason: String },
}
