//! Live [`ChainSource`] implementation backed by `zinder_client::ChainIndex`.
//!
//! `ZinderChainSource` wraps either a [`zinder_client::RemoteChainIndex`] (gRPC) or
//! [`zinder_client::LocalChainIndex`] (colocated RocksDB-secondary) and exposes the
//! Zally-vocabulary [`ChainSource`] surface that [`zally_wallet::Wallet`] consumes.
//!
//! The wrapper is intentionally thin: every method translates Zally domain types into
//! zinder-core/zinder-client domain types, calls the underlying [`ChainIndex`], and
//! translates the result back. Network alignment is checked at construction; per-call
//! re-validation is unnecessary because the underlying client pins the network at connect
//! time.

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use prost::Message;
use zally_core::{BlockHeight, Network, TxId};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;
use zinder_client::{
    ChainEvent as ZinderChainEvent, ChainEventCursor as ZinderChainEventCursor,
    ChainEventEnvelope as ZinderChainEventEnvelope, ChainIndex, RemoteChainIndex,
    RemoteOpenOptions, TransparentAddressUtxosQuery,
};
use zinder_core::{
    BlockHeight as ZinderBlockHeight, BlockHeightRange as ZinderBlockHeightRange,
    Network as ZinderNetwork, ShieldedProtocol as ZinderShieldedProtocol,
    SubtreeRootIndex as ZinderSubtreeRootIndex, SubtreeRootRange as ZinderSubtreeRootRange,
    TransactionId as ZinderTransactionId, TransparentAddressScriptHash, TxStatus as ZinderTxStatus,
};

use crate::chain_error::ChainSourceError;
use crate::chain_source::{
    BlockHeightRange, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainSource, CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot, TransactionStatus,
    TransparentUtxo,
};

const DEFAULT_SUBTREE_PAGE_SIZE: u32 = 256;

/// Options for connecting [`ZinderChainSource`] to a remote `zinder-query` endpoint.
#[derive(Clone, Debug)]
pub struct ZinderRemoteOptions {
    /// Native `WalletQuery` gRPC endpoint URI (e.g. `http://127.0.0.1:9101`).
    pub endpoint: String,
    /// Zally network this endpoint serves. Validated at connect time.
    pub network: Network,
}

/// Live `ChainSource` backed by a [`zinder_client::ChainIndex`].
///
/// `ZinderChainSource` is `Clone` via `Arc`; cloning is cheap and shares the underlying
/// gRPC channel.
#[derive(Clone)]
pub struct ZinderChainSource {
    inner: Arc<dyn ChainIndex>,
    network: Network,
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
        })
    }

    /// Wraps an already-constructed [`ChainIndex`] (any implementation).
    ///
    /// Useful for tests that supply an in-memory fake, and for advanced operators that
    /// open a `LocalChainIndex` against a colocated zinder-ingest `RocksDB` secondary.
    #[must_use]
    pub fn from_chain_index(inner: Arc<dyn ChainIndex>, network: Network) -> Self {
        Self { inner, network }
    }

    /// Returns a [`crate::ZinderSubmitter`] backed by the same gRPC channel as this chain
    /// source. Use this when the same in-process consumer needs both the read plane
    /// (`ChainSource`) and the broadcast plane (`Submitter`) against the same Zinder
    /// endpoint; sharing the channel avoids opening a second TCP connection.
    #[must_use]
    pub fn submitter(&self) -> crate::ZinderSubmitter {
        crate::ZinderSubmitter::from_chain_index(Arc::clone(&self.inner), self.network)
    }
}

#[async_trait]
impl ChainSource for ZinderChainSource {
    fn network(&self) -> Network {
        self.network
    }

    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        let block_id = self.inner.latest_block(None).await?;
        Ok(BlockHeight::from(block_id.height.value()))
    }

    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let zinder_range = ZinderBlockHeightRange::inclusive(
            ZinderBlockHeight::new(block_range.start_height.as_u32()),
            ZinderBlockHeight::new(block_range.end_height.as_u32()),
        );
        let stream = self
            .inner
            .compact_blocks_in_range(zinder_range, None)
            .await?;

        let mapped = stream.map(|stream_item| match stream_item {
            Ok(artifact) => decode_compact_block(&artifact.payload_bytes, artifact.height),
            Err(err) => Err(ChainSourceError::from(err)),
        });
        Ok(Box::pin(mapped) as CompactBlockStream)
    }

    async fn tree_state_at(
        &self,
        block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        let artifact = self
            .inner
            .tree_state_at(ZinderBlockHeight::new(block_height.as_u32()), None)
            .await?;
        decode_tree_state(&artifact.payload_bytes, block_height, self.network)
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        let bounded = max_count.clamp(1, DEFAULT_SUBTREE_PAGE_SIZE);
        let max_entries = NonZeroU32::new(bounded).unwrap_or(NonZeroU32::MIN);
        let range = ZinderSubtreeRootRange::new(
            zally_pool_to_zinder(pool),
            ZinderSubtreeRootIndex::new(start_index.0),
            max_entries,
        );
        let artifacts = self.inner.subtree_roots_in_range(range, None).await?;
        Ok(artifacts
            .into_iter()
            .map(|artifact| SubtreeRoot {
                index: SubtreeIndex(artifact.subtree_index.value()),
                root_bytes: artifact.root_hash.as_bytes(),
            })
            .collect())
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        let zinder_id = ZinderTransactionId::from_bytes(*tx_id.as_bytes());
        let status = self.inner.transaction_by_id(zinder_id, None).await?;
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "non_exhaustive zinder tx statuses map unknown variants to NotFound"
        )]
        let translated = match status {
            ZinderTxStatus::Mined(mined) => TransactionStatus::Confirmed {
                tx_id,
                confirmed_at_height: BlockHeight::from(mined.artifact.block_height.value()),
            },
            ZinderTxStatus::InMempool(_) => TransactionStatus::InMempool { tx_id },
            ZinderTxStatus::NotFound | ZinderTxStatus::ConflictingChain => {
                TransactionStatus::NotFound
            }
            _ => TransactionStatus::NotFound,
        };
        Ok(translated)
    }

    async fn transparent_utxos(
        &self,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        let address_script_hash =
            TransparentAddressScriptHash::of_script_pub_key(script_pub_key_bytes);
        let query = TransparentAddressUtxosQuery {
            address_script_hash,
            start_height: ZinderBlockHeight::new(0),
            max_entries: None,
            from_cursor: None,
        };
        let view = self.inner.transparent_address_utxos(query, None).await?;
        Ok(view
            .utxos
            .into_iter()
            .map(|artifact| TransparentUtxo {
                tx_id: TxId::from_bytes(artifact.outpoint.transaction_id.as_bytes()),
                output_index: artifact.outpoint.output_index,
                value_zat: artifact.value_zat,
                confirmed_at_height: BlockHeight::from(artifact.block_height.value()),
                script_pub_key_bytes: artifact.script_pub_key,
            })
            .collect())
    }

    async fn chain_event_envelopes(
        &self,
        from_cursor: Option<ChainEventCursor>,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let inner = self.inner.clone();
        let zinder_cursor =
            from_cursor.map(|cursor| ZinderChainEventCursor::from_bytes(cursor.into_bytes()));
        let stream = inner.chain_events(zinder_cursor).await?;
        let mapped = stream.map(|envelope_result| match envelope_result {
            Ok(envelope) => Ok(translate_chain_event_envelope(&envelope)),
            Err(err) => Err(ChainSourceError::from(err)),
        });
        Ok(Box::pin(mapped) as ChainEventEnvelopeStream)
    }
}

fn translate_chain_event_envelope(envelope: &ZinderChainEventEnvelope) -> ChainEventEnvelope {
    ChainEventEnvelope::new(
        ChainEventCursor::from_bytes(envelope.cursor.as_bytes().to_vec()),
        envelope.event_sequence,
        BlockHeight::from(envelope.finalized_height.value()),
        translate_chain_event(envelope),
    )
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive zinder chain events map unknown variants to ChainTipAdvanced"
)]
fn translate_chain_event(envelope: &ZinderChainEventEnvelope) -> ChainEvent {
    match &envelope.event {
        ZinderChainEvent::ChainCommitted { committed } => ChainEvent::ChainTipAdvanced {
            committed_range: zinder_range_to_zally(committed.block_range),
            new_tip_height: BlockHeight::from(committed.block_range.end.value()),
        },
        ZinderChainEvent::ChainReorged {
            reverted,
            committed,
        } => ChainEvent::ChainReorged {
            reverted_range: zinder_range_to_zally(reverted.block_range),
            committed_range: zinder_range_to_zally(committed.block_range),
            new_tip_height: BlockHeight::from(committed.block_range.end.value()),
        },
        _ => {
            let finalized = BlockHeight::from(envelope.finalized_height.value());
            ChainEvent::ChainTipAdvanced {
                committed_range: BlockHeightRange {
                    start_height: finalized,
                    end_height: finalized,
                },
                new_tip_height: finalized,
            }
        }
    }
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
    }
}

fn zinder_range_to_zally(range: ZinderBlockHeightRange) -> BlockHeightRange {
    BlockHeightRange {
        start_height: BlockHeight::from(range.start.value()),
        end_height: BlockHeight::from(range.end.value()),
    }
}

fn decode_compact_block(
    payload_bytes: &[u8],
    height: ZinderBlockHeight,
) -> Result<CompactBlock, ChainSourceError> {
    <CompactBlock as Message>::decode(payload_bytes).map_err(|err| {
        ChainSourceError::MalformedCompactBlock {
            block_height: BlockHeight::from(height.value()),
            reason: err.to_string(),
        }
    })
}

/// Translates zinder's stored `z_gettreestate` JSON payload into the lightwalletd
/// `TreeState` protobuf shape that `zcash_client_backend` consumes.
///
/// Zinder stores Zebra's `z_gettreestate` JSON response verbatim. The fields map directly:
/// `height`, `hash`, `time` are top-level; `sapling.commitments.finalState` and
/// `orchard.commitments.finalState` become the hex-encoded `sapling_tree` and
/// `orchard_tree` fields on the protobuf.
fn decode_tree_state(
    payload_bytes: &[u8],
    height: BlockHeight,
    network: Network,
) -> Result<TreeState, ChainSourceError> {
    let parsed: serde_json::Value = serde_json::from_slice(payload_bytes).map_err(|err| {
        ChainSourceError::MalformedCompactBlock {
            block_height: height,
            reason: format!("zinder tree-state payload is not JSON: {err}"),
        }
    })?;

    let height_value = parsed
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ChainSourceError::MalformedCompactBlock {
            block_height: height,
            reason: "tree-state JSON missing `height`".into(),
        })?;
    let hash = parsed
        .get("hash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ChainSourceError::MalformedCompactBlock {
            block_height: height,
            reason: "tree-state JSON missing `hash`".into(),
        })?
        .to_owned();
    let time = parsed
        .get("time")
        .and_then(serde_json::Value::as_u64)
        .and_then(|t| u32::try_from(t).ok())
        .unwrap_or(0);
    let sapling_tree = parsed
        .pointer("/sapling/commitments/finalState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let orchard_tree = parsed
        .pointer("/orchard/commitments/finalState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    Ok(TreeState {
        network: lightwalletd_network_label(network).to_owned(),
        height: height_value,
        hash,
        time,
        sapling_tree,
        orchard_tree,
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "lightwalletd TreeState distinguishes only main and test network labels"
)]
const fn lightwalletd_network_label(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "main",
        _ => "test",
    }
}
