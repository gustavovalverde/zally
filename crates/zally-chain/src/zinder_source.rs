//! Live [`ChainSource`] implementation backed by a [`zinder_client::EndpointBackedIndex`].
//!
//! `ZinderChainSource` wraps a [`zinder_client::RemoteChainIndex`] (gRPC) and exposes the
//! Zally-vocabulary [`ChainSource`] surface that [`zally_wallet::Wallet`] consumes. The
//! source streams chain events, so it needs an endpoint-backed handle; a canonical-only
//! [`zinder_client::LocalChainIndex`] cannot back it.
//!
//! The wrapper is intentionally thin: every method translates Zally domain types into
//! zinder-core/zinder-client domain types, calls the underlying handle, and translates the
//! result back. Remote construction records the expected network; each response-bearing
//! call validates the authenticated chain epoch against it before exposing facts to Zally.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactSaplingOutput,
    CompactSaplingSpend, CompactShieldedAction, CompactTransaction, CompactTransparentInput,
    CompactTransparentOutput, Network, TreeStateArtifact, TxId, Zatoshis,
};
use zcash_protocol::consensus::{NetworkUpgrade, Parameters as _};
use zinder_client::{
    ChainEvent as ZinderChainEvent, ChainEventCursor as ZinderChainEventCursor,
    ChainEventCursorRecovery as ZinderChainEventCursorRecovery,
    ChainEventEnvelope as ZinderChainEventEnvelope, EndpointBackedIndex,
    EventStreamStart as ZinderEventStreamStart, RemoteChainIndex, RemoteOpenOptions,
    TransparentAddressUnspentOutputsQuery, WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1,
    WALLET_EVENTS_CHAIN_V1, WALLET_READ_COMPACT_BLOCK_IRONWOOD_V2,
    WALLET_READ_COMPACT_BLOCK_RANGE_V2, WALLET_READ_SERVER_INFO_V2,
    WALLET_READ_SETTLED_TIP_BLOCK_V1, WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
    WALLET_READ_SUBTREE_ROOTS_IRONWOOD_V1, WALLET_READ_TRANSACTION_BY_ID_V2,
    WALLET_READ_TREE_STATE_AT_HEIGHT_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1,
};
use zinder_core::{
    BlockHeight as ZinderBlockHeight, BlockHeightRange as ZinderBlockHeightRange,
    ChainEpochId as ZinderChainEpochId, Network as ZinderNetwork,
    ShieldedProtocol as ZinderShieldedProtocol, SubtreeRootIndex as ZinderSubtreeRootIndex,
    SubtreeRootRange as ZinderSubtreeRootRange, TransactionId as ZinderTransactionId,
    TransparentAddressScriptHash, TreeStateArtifact as ZinderTreeStateArtifact,
    TxStatus as ZinderTxStatus,
};

use crate::error::ChainSourceError;
use crate::source::{
    BlockHeightRange, BlockId, ChainEpoch, ChainEpochCommitted, ChainEpochId, ChainEvent,
    ChainEventCursor, ChainEventCursorRecovery, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainRangeReverted, ChainSource, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};

const DEFAULT_SUBTREE_PAGE_SIZE: u32 = 256;
const REQUIRED_SYNC_CAPABILITIES: [&str; 10] = [
    WALLET_READ_SERVER_INFO_V2,
    WALLET_READ_VISIBLE_TIP_BLOCK_V1,
    WALLET_READ_SETTLED_TIP_BLOCK_V1,
    WALLET_READ_COMPACT_BLOCK_RANGE_V2,
    WALLET_READ_COMPACT_BLOCK_IRONWOOD_V2,
    WALLET_READ_TREE_STATE_AT_HEIGHT_V2,
    WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
    WALLET_READ_SUBTREE_ROOTS_IRONWOOD_V1,
    WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1,
    WALLET_EVENTS_CHAIN_V1,
];
const MINIMUM_CONTRACT_REVISION: u32 = 2;

pub(crate) type ZinderCapabilitySet = BTreeSet<String>;

/// Options for connecting [`ZinderChainSource`] to a remote `zinder-query` endpoint.
#[derive(Clone, Debug)]
pub struct ZinderRemoteOptions {
    /// Native `WalletQuery` gRPC endpoint URI (e.g. `http://127.0.0.1:9102`).
    pub endpoint: String,
    /// Zally network this endpoint serves. Validated at connect time.
    pub network: Network,
}

/// Live `ChainSource` backed by a [`zinder_client::EndpointBackedIndex`].
///
/// `ZinderChainSource` is `Clone` via `Arc`; cloning is cheap and shares the underlying
/// gRPC channel.
#[derive(Clone)]
pub struct ZinderChainSource {
    inner: Arc<dyn EndpointBackedIndex>,
    network: Network,
    capabilities: Arc<tokio::sync::OnceCell<ZinderCapabilitySet>>,
}

impl std::fmt::Debug for ZinderChainSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZinderChainSource")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl ZinderChainSource {
    /// Builds a chain-source handle pointed at a `zinder-query` gRPC endpoint.
    ///
    /// The gRPC channel is lazy: only URI parsing can fail here; the
    /// connection is established on the first chain-source call and
    /// re-established automatically after a transport failure.
    pub fn connect_remote(options: ZinderRemoteOptions) -> Result<Self, ChainSourceError> {
        let zinder_network = zally_network_to_zinder(options.network)?;
        let remote = RemoteChainIndex::connect(RemoteOpenOptions {
            endpoint: options.endpoint,
            network: zinder_network,
        })?;

        Ok(Self {
            inner: Arc::new(remote),
            network: options.network,
            capabilities: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// Wraps an already-constructed [`EndpointBackedIndex`].
    ///
    /// Useful for tests that supply an in-memory fake. The source streams chain
    /// events, so it needs an endpoint-backed handle; a canonical-only
    /// `LocalChainIndex` cannot back it.
    #[must_use]
    pub fn from_chain_index(inner: Arc<dyn EndpointBackedIndex>, network: Network) -> Self {
        Self {
            inner,
            network,
            capabilities: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Returns a [`crate::ZinderSubmitter`] backed by the same gRPC channel as this chain
    /// source. Use this when the same in-process consumer needs both the read plane
    /// (`ChainSource`) and the broadcast plane (`Submitter`) against the same Zinder
    /// endpoint; sharing the channel avoids opening a second TCP connection.
    #[must_use]
    pub fn submitter(&self) -> crate::ZinderSubmitter {
        crate::ZinderSubmitter::from_chain_index_with_capabilities(
            Arc::clone(&self.inner),
            self.network,
            Arc::clone(&self.capabilities),
        )
    }

    async fn ensure_capabilities(&self, required: &[&str]) -> Result<(), ChainSourceError> {
        let capabilities = self
            .capabilities
            .get_or_try_init(|| async {
                let descriptor = self
                    .inner
                    .server_info()
                    .await
                    .map_err(Self::map_indexer_error)?;
                let common = descriptor
                    .common
                    .ok_or(ChainSourceError::UnsupportedResponse {
                        response: "WalletServerInfo.common",
                    })?;
                if common.contract_revision < MINIMUM_CONTRACT_REVISION {
                    return Err(ChainSourceError::ContractRevisionUnsupported {
                        minimum_revision: MINIMUM_CONTRACT_REVISION,
                        actual_revision: common.contract_revision,
                    });
                }
                Ok::<_, ChainSourceError>(common.capabilities.into_iter().collect())
            })
            .await?;
        let missing_capabilities = required
            .iter()
            .filter(|capability| !capabilities.contains(**capability))
            .map(|capability| (*capability).to_owned())
            .collect::<Vec<_>>();
        if missing_capabilities.is_empty() {
            Ok(())
        } else {
            Err(ChainSourceError::CapabilitiesUnavailable {
                capabilities: missing_capabilities,
            })
        }
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "IndexerError is non-exhaustive; unknown typed errors must retain their source and retry policy"
    )]
    fn map_indexer_error(error: zinder_client::IndexerError) -> ChainSourceError {
        match error {
            zinder_client::IndexerError::ChainEpochPinUnavailable => {
                ChainSourceError::ChainEpochPinUnavailable
            }
            zinder_client::IndexerError::ChainEventCursorExpired {
                recovery: ZinderChainEventCursorRecovery::EarliestRetained,
            } => ChainSourceError::ChainEventCursorExpired {
                recovery: ChainEventCursorRecovery::EarliestRetained,
            },
            other => ChainSourceError::Indexer(other),
        }
    }

    fn pinned_result<T>(
        operation_result: Result<T, zinder_client::IndexerError>,
    ) -> Result<T, ChainSourceError> {
        match operation_result {
            Ok(output) => Ok(output),
            Err(error) => Err(Self::map_indexer_error(error)),
        }
    }

    fn pinned_epoch(
        &self,
        chain_epoch: ChainEpoch,
    ) -> Result<ZinderChainEpochId, ChainSourceError> {
        if chain_epoch.network() != self.network {
            return Err(ChainSourceError::NetworkMismatch {
                chain_source_network: self.network,
                requested_network: chain_epoch.network(),
            });
        }
        Ok(ZinderChainEpochId::new(chain_epoch.id().value()))
    }
}

#[async_trait]
impl ChainSource for ZinderChainSource {
    fn network(&self) -> Network {
        self.network
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
        self.ensure_capabilities(&REQUIRED_SYNC_CAPABILITIES)
            .await?;
        translate_chain_epoch(self.inner.current_epoch().await?, self.network)
    }

    async fn compact_blocks(
        &self,
        chain_epoch: ChainEpoch,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        self.ensure_capabilities(&REQUIRED_SYNC_CAPABILITIES)
            .await?;
        let epoch = self.pinned_epoch(chain_epoch)?;
        let zinder_range = ZinderBlockHeightRange::inclusive(
            ZinderBlockHeight::new(block_range.start_height().as_u32()),
            ZinderBlockHeight::new(block_range.end_height().as_u32()),
        );
        let stream = Self::pinned_result(
            self.inner
                .compact_blocks_in_range(zinder_range, Some(epoch))
                .await,
        )?;

        let mapped = stream.map(move |stream_item| match stream_item {
            Ok(artifact) => decode_compact_block(&artifact),
            Err(error) => Err(Self::map_indexer_error(error)),
        });
        Ok(Box::pin(mapped) as CompactBlockStream)
    }

    async fn tree_state_at(
        &self,
        chain_epoch: ChainEpoch,
        block_height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError> {
        self.ensure_capabilities(&REQUIRED_SYNC_CAPABILITIES)
            .await?;
        let epoch = self.pinned_epoch(chain_epoch)?;
        let artifact = Self::pinned_result(
            self.inner
                .tree_state_at(ZinderBlockHeight::new(block_height.as_u32()), Some(epoch))
                .await,
        )?;
        decode_tree_state(&artifact, block_height, self.network)
    }

    async fn subtree_roots(
        &self,
        chain_epoch: ChainEpoch,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        self.ensure_capabilities(&REQUIRED_SYNC_CAPABILITIES)
            .await?;
        let epoch = self.pinned_epoch(chain_epoch)?;
        let bounded = max_count.clamp(1, DEFAULT_SUBTREE_PAGE_SIZE);
        let max_entries = NonZeroU32::new(bounded).unwrap_or(NonZeroU32::MIN);
        let range = ZinderSubtreeRootRange::new(
            zally_pool_to_zinder(pool),
            ZinderSubtreeRootIndex::new(start_index.0),
            max_entries,
        );
        let artifacts =
            Self::pinned_result(self.inner.subtree_roots_in_range(range, Some(epoch)).await)?;
        Ok(artifacts
            .into_iter()
            .map(|artifact| SubtreeRoot {
                index: SubtreeIndex(artifact.subtree_index.value()),
                root_bytes: artifact.root_hash.as_bytes(),
                completing_block_height: BlockHeight::from(
                    artifact.completing_block_height.value(),
                ),
            })
            .collect())
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        self.ensure_capabilities(&[WALLET_READ_TRANSACTION_BY_ID_V2])
            .await?;
        let zinder_id = ZinderTransactionId::from_bytes(*tx_id.as_bytes());
        let status = self.inner.transaction_by_id(zinder_id, None).await?;
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "non_exhaustive zinder tx statuses fail closed on unknown variants"
        )]
        let translated = match status {
            ZinderTxStatus::Mined(mined) => TransactionStatus::Confirmed {
                tx_id,
                confirmed_at_height: BlockHeight::from(mined.location.block_height.value()),
            },
            ZinderTxStatus::InMempool(_) => TransactionStatus::InMempool { tx_id },
            ZinderTxStatus::NotFound => TransactionStatus::NotFound,
            _ => {
                return Err(ChainSourceError::UnsupportedResponse {
                    response: "TransactionStatus",
                });
            }
        };
        Ok(translated)
    }

    async fn transparent_utxos(
        &self,
        chain_epoch: ChainEpoch,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.ensure_capabilities(&[WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1])
            .await?;
        let epoch = self.pinned_epoch(chain_epoch)?;
        let address_script_hash =
            TransparentAddressScriptHash::of_script_pub_key(script_pub_key_bytes);
        let query = TransparentAddressUnspentOutputsQuery {
            address_script_hash,
            start_height: ZinderBlockHeight::new(0),
            at_epoch_id: Some(epoch),
        };
        let mut stream =
            Self::pinned_result(self.inner.transparent_address_unspent_outputs(query).await)?;
        let mut utxos = Vec::new();
        while let Some(stream_item) = stream.next().await {
            let output = match stream_item {
                Ok(chunk) => {
                    if chunk.chain_epoch.id != epoch {
                        return Err(ChainSourceError::ChainEpochPinUnavailable);
                    }
                    chunk.output
                }
                Err(error) => {
                    return Err(Self::map_indexer_error(error));
                }
            };
            let tx_id = TxId::from_bytes(output.outpoint.transaction_id.as_bytes());
            let value_zat = Zatoshis::try_from(output.value_zat).map_err(|error| {
                ChainSourceError::MalformedTransparentUtxoSet {
                    reason: format!(
                        "outpoint {tx_id}:{} has invalid value_zat {}: {error}",
                        output.outpoint.output_index, output.value_zat
                    ),
                }
            })?;
            utxos.push(TransparentUtxo {
                tx_id,
                output_index: output.outpoint.output_index,
                value_zat,
                confirmed_at_height: BlockHeight::from(output.block_height.value()),
                script_pub_key_bytes: output.script_pub_key,
            });
        }
        Ok(utxos)
    }

    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        self.ensure_capabilities(&[WALLET_EVENTS_CHAIN_V1]).await?;
        let inner = self.inner.clone();
        let zinder_start = match start {
            ChainEventStreamStart::AfterCursor(cursor) => ZinderEventStreamStart::AfterCursor(
                ZinderChainEventCursor::from_bytes(cursor.into_bytes()),
            ),
            ChainEventStreamStart::EarliestRetained => ZinderEventStreamStart::EarliestRetained,
            ChainEventStreamStart::LiveTail => ZinderEventStreamStart::LiveTail,
        };
        let stream = inner
            .chain_events(zinder_start)
            .await
            .map_err(Self::map_indexer_error)?;
        let network = self.network;
        let mapped = stream.map(move |envelope_result| match envelope_result {
            Ok(envelope) => translate_chain_event_envelope(&envelope, network),
            Err(err) => Err(Self::map_indexer_error(err)),
        });
        Ok(Box::pin(mapped) as ChainEventEnvelopeStream)
    }
}

fn translate_chain_event_envelope(
    envelope: &ZinderChainEventEnvelope,
    expected_network: Network,
) -> Result<ChainEventEnvelope, ChainSourceError> {
    let chain_epoch = translate_chain_epoch(envelope.chain_epoch, expected_network)?;
    if BlockHeight::from(envelope.settled_tip_height.value()) != chain_epoch.settled_tip().height {
        return Err(ChainSourceError::UnsupportedResponse {
            response: "chain-event envelope settled tip differs from its epoch",
        });
    }
    let event = translate_chain_event(envelope, expected_network)?;
    let committed_epoch = match &event {
        ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. } => {
            committed.chain_epoch
        }
    };
    if committed_epoch != chain_epoch {
        return Err(ChainSourceError::UnsupportedResponse {
            response: "chain-event committed epoch differs from its envelope epoch",
        });
    }
    Ok(ChainEventEnvelope::new(
        ChainEventCursor::from_bytes(envelope.cursor.as_bytes().to_vec()),
        envelope.event_sequence,
        chain_epoch,
        event,
    ))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive zinder chain events fail closed on unknown variants"
)]
fn translate_chain_event(
    envelope: &ZinderChainEventEnvelope,
    expected_network: Network,
) -> Result<ChainEvent, ChainSourceError> {
    match &envelope.event {
        ZinderChainEvent::ChainCommitted { committed } => Ok(ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch: translate_chain_epoch(committed.chain_epoch, expected_network)?,
                block_range: zinder_range_to_zally(committed.block_range)?,
            },
        }),
        ZinderChainEvent::ChainReorged {
            reverted,
            committed,
        } => Ok(ChainEvent::ChainReorged {
            reverted: ChainRangeReverted {
                chain_epoch: translate_chain_epoch(reverted.chain_epoch, expected_network)?,
                block_range: zinder_range_to_zally(reverted.block_range)?,
            },
            committed: ChainEpochCommitted {
                chain_epoch: translate_chain_epoch(committed.chain_epoch, expected_network)?,
                block_range: zinder_range_to_zally(committed.block_range)?,
            },
        }),
        _ => Err(ChainSourceError::UnsupportedResponse {
            response: "ChainEvent",
        }),
    }
}

fn translate_chain_epoch(
    epoch: zinder_core::ChainEpoch,
    expected_network: Network,
) -> Result<ChainEpoch, ChainSourceError> {
    let actual_network = zinder_network_to_zally(epoch.network, expected_network)?;
    if actual_network != expected_network {
        return Err(ChainSourceError::NetworkMismatch {
            chain_source_network: actual_network,
            requested_network: expected_network,
        });
    }
    ChainEpoch::new(
        ChainEpochId::new(epoch.id.value()),
        actual_network,
        BlockId {
            height: BlockHeight::from(epoch.visible_tip_height.value()),
            hash: BlockHash::from_bytes(epoch.visible_tip_hash.as_bytes()),
        },
        BlockId {
            height: BlockHeight::from(epoch.settled_tip_height.value()),
            hash: BlockHash::from_bytes(epoch.settled_tip_hash.as_bytes()),
        },
    )
    .ok_or(ChainSourceError::UnsupportedResponse {
        response: "ChainEpoch(settled_tip_above_visible_tip)",
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "unknown future Zinder networks fail closed instead of inheriting configured parameters"
)]
fn zinder_network_to_zally(
    network: ZinderNetwork,
    expected_network: Network,
) -> Result<Network, ChainSourceError> {
    let actual_network = match network {
        ZinderNetwork::ZcashMainnet => Network::Mainnet,
        ZinderNetwork::ZcashTestnet => Network::Testnet,
        ZinderNetwork::ZcashRegtest => match expected_network {
            Network::Regtest(_) => expected_network,
            _ => Network::regtest(),
        },
        _ => {
            return Err(ChainSourceError::UnsupportedResponse {
                response: "ChainEpoch.network",
            });
        }
    };
    if actual_network != expected_network {
        return Err(ChainSourceError::NetworkMismatch {
            chain_source_network: actual_network,
            requested_network: expected_network,
        });
    }
    Ok(actual_network)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive Zally networks map unknown variants to NetworkMismatch"
)]
fn zally_network_to_zinder(network: Network) -> Result<ZinderNetwork, ChainSourceError> {
    match network {
        Network::Mainnet => Ok(ZinderNetwork::ZcashMainnet),
        Network::Testnet => Ok(ZinderNetwork::ZcashTestnet),
        Network::Regtest(_) => Ok(ZinderNetwork::ZcashRegtest),
        _ => Err(ChainSourceError::NetworkMismatch {
            chain_source_network: network,
            requested_network: network,
        }),
    }
}

const fn zally_pool_to_zinder(pool: ShieldedPool) -> ZinderShieldedProtocol {
    match pool {
        ShieldedPool::Sapling => ZinderShieldedProtocol::Sapling,
        ShieldedPool::Orchard => ZinderShieldedProtocol::Orchard,
        ShieldedPool::Ironwood => ZinderShieldedProtocol::Ironwood,
    }
}

fn zinder_range_to_zally(
    range: ZinderBlockHeightRange,
) -> Result<BlockHeightRange, ChainSourceError> {
    BlockHeightRange::new(
        BlockHeight::from(range.start.value()),
        BlockHeight::from(range.end.value()),
    )
    .ok_or(ChainSourceError::UnsupportedResponse {
        response: "reversed Zinder block-height range",
    })
}

fn decode_compact_block(
    artifact: &zinder_core::CompactBlockArtifact,
) -> Result<CompactBlockArtifact, ChainSourceError> {
    let block_height = BlockHeight::from(artifact.height().value());
    let transactions = artifact
        .transactions()
        .iter()
        .map(|transaction| compact_transaction(transaction, block_height))
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = artifact.chain_metadata();
    Ok(CompactBlockArtifact {
        height: BlockHeight::from(artifact.height().value()),
        block_hash: BlockHash::from_bytes(artifact.block_hash().as_bytes()),
        previous_block_hash: BlockHash::from_bytes(artifact.previous_block_hash().as_bytes()),
        block_time_seconds: artifact.time(),
        transactions,
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size: metadata.sapling_commitment_tree_size,
            orchard_commitment_tree_size: metadata.orchard_commitment_tree_size,
            ironwood_commitment_tree_size: metadata.ironwood_commitment_tree_size,
        },
    })
}

fn compact_transaction(
    transaction: &zinder_core::CompactTransaction,
    block_height: BlockHeight,
) -> Result<CompactTransaction, ChainSourceError> {
    let fee_zat = transaction
        .data
        .fee_zat
        .map(Zatoshis::try_from)
        .transpose()
        .map_err(|error| ChainSourceError::MalformedCompactBlock {
            block_height,
            reason: error.to_string(),
        })?;
    let transparent_outputs = transaction
        .data
        .transparent_outputs
        .iter()
        .map(|output| {
            Ok(CompactTransparentOutput {
                value_zat: Zatoshis::try_from(output.value_zat).map_err(|error| {
                    ChainSourceError::MalformedCompactBlock {
                        block_height,
                        reason: error.to_string(),
                    }
                })?,
                script_pub_key_bytes: output.script_pub_key.clone(),
            })
        })
        .collect::<Result<Vec<_>, ChainSourceError>>()?;
    Ok(CompactTransaction {
        index: transaction.index,
        tx_id: TxId::from_bytes(transaction.transaction_id.as_bytes()),
        fee_zat,
        sapling_spends: transaction
            .data
            .sapling_spends
            .iter()
            .map(|spend| CompactSaplingSpend {
                nullifier_bytes: spend.nullifier,
            })
            .collect(),
        sapling_outputs: transaction
            .data
            .sapling_outputs
            .iter()
            .map(|output| CompactSaplingOutput {
                commitment_bytes: output.commitment,
                ephemeral_key_bytes: output.ephemeral_key,
                ciphertext_bytes: output.ciphertext,
            })
            .collect(),
        orchard_actions: transaction
            .data
            .orchard_actions
            .iter()
            .map(compact_shielded_action)
            .collect(),
        ironwood_actions: transaction
            .data
            .ironwood_actions
            .iter()
            .map(compact_shielded_action)
            .collect(),
        transparent_inputs: transaction
            .data
            .transparent_inputs
            .iter()
            .map(|input| CompactTransparentInput {
                previous_tx_id: TxId::from_bytes(input.previous_transaction_id.as_bytes()),
                previous_output_index: input.previous_output_index,
            })
            .collect(),
        transparent_outputs,
    })
}

fn compact_shielded_action(action: &zinder_core::CompactShieldedAction) -> CompactShieldedAction {
    CompactShieldedAction {
        nullifier_bytes: action.nullifier,
        commitment_bytes: action.commitment,
        ephemeral_key_bytes: action.ephemeral_key,
        ciphertext_bytes: action.ciphertext,
    }
}

/// Translates Zinder's stored `z_gettreestate` JSON payload into Zally's source-neutral
/// [`TreeStateArtifact`].
///
/// Zinder stores Zebra's `z_gettreestate` JSON response verbatim. The fields map directly:
/// `height`, `hash`, and `time` are top-level. Each pool's
/// `commitments.finalState` hex becomes its serialized frontier bytes; the storage boundary
/// later performs the librustzcash conversion.
fn decode_tree_state(
    artifact: &ZinderTreeStateArtifact,
    requested_height: BlockHeight,
    network: Network,
) -> Result<TreeStateArtifact, ChainSourceError> {
    let artifact_height = BlockHeight::from(artifact.height.value());
    if artifact_height != requested_height {
        return Err(ChainSourceError::TreeStateAnchorHeightMismatch {
            requested_height,
            returned_height: artifact_height,
        });
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&artifact.payload_bytes).map_err(|err| {
            ChainSourceError::MalformedCompactBlock {
                block_height: requested_height,
                reason: format!("zinder tree-state payload is not JSON: {err}"),
            }
        })?;

    let sapling_final_state_bytes = pool_final_state(
        &parsed,
        "sapling",
        NetworkUpgrade::Sapling,
        requested_height,
        network,
    )?;
    let orchard_final_state_bytes = pool_final_state(
        &parsed,
        "orchard",
        NetworkUpgrade::Nu5,
        requested_height,
        network,
    )?;
    let ironwood_final_state_bytes = pool_final_state(
        &parsed,
        "ironwood",
        NetworkUpgrade::Nu6_3,
        requested_height,
        network,
    )?;

    Ok(TreeStateArtifact {
        network,
        height: artifact_height,
        block_hash: BlockHash::from_bytes(artifact.block_hash.as_bytes()),
        block_time_seconds: artifact.block_time_seconds,
        sapling_final_state_bytes,
        orchard_final_state_bytes,
        ironwood_final_state_bytes,
    })
}

/// Reads one pool's `finalState` frontier from the tree-state JSON.
///
/// A missing or empty frontier is only valid below the pool's activation height, where no
/// commitment tree exists yet. At or above activation the frontier seeds the scan's starting
/// position, so defaulting it silently would build a tree whose root can never reconverge
/// with the chain; the mismatch would later surface as a root divergence blamed on the scan.
fn pool_final_state(
    parsed: &serde_json::Value,
    pool: &str,
    upgrade: NetworkUpgrade,
    height: BlockHeight,
    network: Network,
) -> Result<Vec<u8>, ChainSourceError> {
    let final_state = parsed
        .pointer(&format!("/{pool}/commitments/finalState"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if final_state.is_empty() {
        let is_pool_active = network
            .to_parameters()
            .activation_height(upgrade)
            .map(BlockHeight::from)
            .is_some_and(|activation| height >= activation);
        if is_pool_active {
            return Err(ChainSourceError::MalformedCompactBlock {
                block_height: height,
                reason: format!(
                    "tree-state JSON missing `{pool}` finalState with the pool active at this height"
                ),
            });
        }
    }
    hex::decode(final_state).map_err(|error| ChainSourceError::MalformedCompactBlock {
        block_height: height,
        reason: format!("tree-state JSON `{pool}` finalState is not hexadecimal: {error}"),
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests panic on fixture decode outcomes that contradict the case under test"
)]
mod tests {
    use super::*;
    use zinder_core::{
        BlockHash as ZinderBlockHash, BlockId as ZinderBlockId,
        CompactBlockArtifact as ZinderCompactBlockArtifact,
        CompactChainMetadata as ZinderCompactChainMetadata,
        CompactSaplingOutput as ZinderCompactSaplingOutput,
        CompactSaplingSpend as ZinderCompactSaplingSpend,
        CompactShieldedAction as ZinderCompactShieldedAction,
        CompactTransaction as ZinderCompactTransaction,
        CompactTransactionData as ZinderCompactTransactionData,
        CompactTransparentInput as ZinderCompactTransparentInput,
        CompactTransparentOutput as ZinderCompactTransparentOutput,
        TransactionId as ZinderTransactionId,
    };

    const TESTNET_NU5_HEIGHT: u32 = 1_842_420;

    fn compact_artifact(data: ZinderCompactTransactionData) -> ZinderCompactBlockArtifact {
        ZinderCompactBlockArtifact::new(
            ZinderBlockId::new(
                ZinderBlockHeight::new(42),
                ZinderBlockHash::from_bytes([7; 32]),
            ),
            ZinderBlockHash::from_bytes([6; 32]),
            1_700_000_000,
            vec![ZinderCompactTransaction {
                index: 9,
                transaction_id: ZinderTransactionId::from_bytes([8; 32]),
                data,
            }],
            ZinderCompactChainMetadata {
                sapling_commitment_tree_size: 10,
                orchard_commitment_tree_size: 11,
                ironwood_commitment_tree_size: 12,
            },
        )
        .expect("ordered compact artifact")
    }

    #[test]
    fn compact_block_maps_every_wallet_field_exactly() {
        let action = ZinderCompactShieldedAction {
            nullifier: [4; 32],
            commitment: [5; 32],
            ephemeral_key: [6; 32],
            ciphertext: [7; 52],
        };
        let artifact = compact_artifact(ZinderCompactTransactionData {
            fee_zat: Some(123),
            sapling_spends: vec![ZinderCompactSaplingSpend { nullifier: [1; 32] }],
            sapling_outputs: vec![ZinderCompactSaplingOutput {
                commitment: [2; 32],
                ephemeral_key: [3; 32],
                ciphertext: [4; 52],
            }],
            orchard_actions: vec![action.clone()],
            ironwood_actions: vec![action],
            transparent_inputs: vec![ZinderCompactTransparentInput {
                previous_transaction_id: ZinderTransactionId::from_bytes([9; 32]),
                previous_output_index: 2,
            }],
            transparent_outputs: vec![ZinderCompactTransparentOutput {
                value_zat: 456,
                script_pub_key: vec![0x51],
            }],
        });
        let mapped = decode_compact_block(&artifact).expect("valid artifact maps");
        assert_eq!(mapped.height, BlockHeight::from(42));
        assert_eq!(mapped.block_hash, BlockHash::from_bytes([7; 32]));
        assert_eq!(mapped.previous_block_hash, BlockHash::from_bytes([6; 32]));
        assert_eq!(mapped.block_time_seconds, 1_700_000_000);
        assert_eq!(mapped.chain_metadata.sapling_commitment_tree_size, 10);
        assert_eq!(mapped.chain_metadata.orchard_commitment_tree_size, 11);
        assert_eq!(mapped.chain_metadata.ironwood_commitment_tree_size, 12);
        let transaction = &mapped.transactions[0];
        assert_eq!(transaction.index, 9);
        assert_eq!(transaction.tx_id, TxId::from_bytes([8; 32]));
        assert_eq!(transaction.fee_zat, Zatoshis::try_from(123).ok());
        assert_eq!(transaction.sapling_spends[0].nullifier_bytes, [1; 32]);
        assert_eq!(transaction.sapling_outputs[0].ciphertext_bytes, [4; 52]);
        assert_eq!(transaction.orchard_actions[0].commitment_bytes, [5; 32]);
        assert_eq!(transaction.ironwood_actions[0].ephemeral_key_bytes, [6; 32]);
        assert_eq!(transaction.transparent_inputs[0].previous_output_index, 2);
        assert_eq!(transaction.transparent_outputs[0].value_zat.as_u64(), 456);
        assert_eq!(
            transaction.transparent_outputs[0].script_pub_key_bytes,
            vec![0x51]
        );
    }

    #[test]
    fn compact_block_rejects_money_above_maximum() {
        let excessive = zcash_protocol::value::MAX_MONEY + 1;
        let fee = compact_artifact(ZinderCompactTransactionData {
            fee_zat: Some(excessive),
            ..ZinderCompactTransactionData::default()
        });
        assert!(matches!(
            decode_compact_block(&fee),
            Err(ChainSourceError::MalformedCompactBlock { .. })
        ));
        let output = compact_artifact(ZinderCompactTransactionData {
            transparent_outputs: vec![ZinderCompactTransparentOutput {
                value_zat: excessive,
                script_pub_key: Vec::new(),
            }],
            ..ZinderCompactTransactionData::default()
        });
        assert!(matches!(
            decode_compact_block(&output),
            Err(ChainSourceError::MalformedCompactBlock { .. })
        ));
    }

    fn tree_state_artifact(height: u32, pools: &[(&str, &str)]) -> ZinderTreeStateArtifact {
        let mut root = serde_json::json!({
            "height": height,
            "hash": "00".repeat(32),
            "time": 1_700_000_000,
        });
        for (pool, final_state) in pools {
            root[pool] = serde_json::json!({ "commitments": { "finalState": final_state } });
        }
        ZinderTreeStateArtifact::new(
            ZinderBlockHeight::new(height),
            ZinderBlockHash::from_bytes([0; 32]),
            1_700_000_000,
            serde_json::to_vec(&root).expect("fixture serializes"),
        )
    }

    #[test]
    fn missing_pool_frontier_defaults_below_activation() {
        let artifact = tree_state_artifact(TESTNET_NU5_HEIGHT - 1, &[("sapling", "abcd")]);
        let tree_state = decode_tree_state(
            &artifact,
            BlockHeight::from(TESTNET_NU5_HEIGHT - 1),
            Network::Testnet,
        )
        .expect("pre-activation tree state decodes");
        assert_eq!(tree_state.sapling_final_state_bytes, vec![0xab, 0xcd]);
        assert!(tree_state.orchard_final_state_bytes.is_empty());
        assert!(tree_state.ironwood_final_state_bytes.is_empty());
    }

    #[test]
    fn missing_pool_frontier_faults_at_activation() {
        let artifact = tree_state_artifact(TESTNET_NU5_HEIGHT, &[("sapling", "abcd")]);
        let err = decode_tree_state(
            &artifact,
            BlockHeight::from(TESTNET_NU5_HEIGHT),
            Network::Testnet,
        )
        .expect_err("post-activation tree state without an orchard frontier faults");
        assert!(matches!(
            err,
            ChainSourceError::MalformedCompactBlock { .. }
        ));
        assert!(err.to_string().contains("orchard"));
    }

    #[test]
    fn empty_pool_frontier_faults_the_same_as_missing() {
        let artifact =
            tree_state_artifact(TESTNET_NU5_HEIGHT, &[("sapling", "abcd"), ("orchard", "")]);
        let err = decode_tree_state(
            &artifact,
            BlockHeight::from(TESTNET_NU5_HEIGHT),
            Network::Testnet,
        )
        .expect_err("an empty frontier string carries no more evidence than a missing one");
        assert!(err.to_string().contains("orchard"));
    }

    #[test]
    fn present_pool_frontiers_decode_at_any_height() {
        let artifact = tree_state_artifact(
            4_200_000,
            &[("sapling", "aa"), ("orchard", "bb"), ("ironwood", "cc")],
        );
        let tree_state =
            decode_tree_state(&artifact, BlockHeight::from(4_200_000u32), Network::Testnet)
                .expect("fully populated tree state decodes");
        assert_eq!(tree_state.sapling_final_state_bytes, vec![0xaa]);
        assert_eq!(tree_state.orchard_final_state_bytes, vec![0xbb]);
        assert_eq!(tree_state.ironwood_final_state_bytes, vec![0xcc]);
        assert_eq!(tree_state.block_time_seconds, 1_700_000_000);
    }

    #[test]
    fn tree_state_rejects_artifact_height_different_from_request() {
        let artifact = tree_state_artifact(41, &[]);
        let error = decode_tree_state(&artifact, BlockHeight::from(42), Network::Testnet)
            .expect_err("artifact height mismatch must fail closed");
        assert!(matches!(
            error,
            ChainSourceError::TreeStateAnchorHeightMismatch { .. }
        ));
    }

    #[test]
    fn zinder_network_mapping_rejects_every_mismatch() {
        assert!(zinder_network_to_zally(ZinderNetwork::ZcashMainnet, Network::Mainnet).is_ok());
        assert!(zinder_network_to_zally(ZinderNetwork::ZcashTestnet, Network::Testnet).is_ok());
        assert!(zinder_network_to_zally(ZinderNetwork::ZcashRegtest, Network::regtest()).is_ok());
        for (actual, expected) in [
            (ZinderNetwork::ZcashMainnet, Network::Testnet),
            (ZinderNetwork::ZcashMainnet, Network::regtest()),
            (ZinderNetwork::ZcashTestnet, Network::Mainnet),
            (ZinderNetwork::ZcashTestnet, Network::regtest()),
            (ZinderNetwork::ZcashRegtest, Network::Mainnet),
            (ZinderNetwork::ZcashRegtest, Network::Testnet),
        ] {
            assert!(matches!(
                zinder_network_to_zally(actual, expected),
                Err(ChainSourceError::NetworkMismatch { .. })
            ));
        }
    }
}
