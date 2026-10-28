//! Retirement of pending broadcasts once the chain settles their fate.
//!
//! A pending broadcast stays recorded until it is mined at or below the judged height, or
//! until no block above that height can include it. Wall-clock age never retires one: a
//! settled tip can trail the visible tip by the chain source's whole reorg window.

use std::collections::HashSet;
use std::time::Duration;

use zally_chain::{
    ChainEpoch, ChainSource, TransactionStatus, UnmineableReason, parse_transaction_header,
};
use zally_core::{BlockHeight, TxId};
use zally_storage::UnmineableHold;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::wallet::Wallet;

/// Whether the chain source can back unmineable verdicts in this pass, and on which epoch.
enum VerdictGate {
    /// The source's reorg window meets the minimum and `ChainEpoch` covers the judged height.
    Open(ChainEpoch),
    /// Verdicts wait, for the reason given.
    Closed(String),
}

/// Whether a transaction the wallet judged unmineable is in the chain.
enum ChainPresence {
    /// Absent from the canonical chain at the pinned epoch.
    Absent,
    /// Mined; the wallet recorded it.
    Mined,
    /// Not established, for the reason given.
    Unknown(String),
    /// Not established because the source did not answer in time; the rest of the pass
    /// skips the source rather than wait on it once per broadcast.
    Unresponsive(String),
}

/// A chain source pinned to the epoch one verdict pass is judged against.
#[derive(Clone, Copy)]
struct PinnedChain<'a> {
    chain: &'a dyn ChainSource,
    epoch: ChainEpoch,
}

/// State one call of [`Wallet::retire_pending_broadcasts`] shares across broadcasts.
struct VerdictPass<'a> {
    chain: &'a dyn ChainSource,
    judged_height: BlockHeight,
    /// `None` holds every verdict; see `WalletInner::scan_blind_sends`.
    scan_blind_sends: Option<HashSet<TxId>>,
    /// Opened on the first broadcast that needs a verdict, then reused.
    gate: Option<VerdictGate>,
    withheld: HashSet<TxId>,
}

impl Wallet {
    /// Retires every pending broadcast whose fate the chain settled.
    ///
    /// Broadcasts are judged at the lower of the fully scanned height and the settled tip. The
    /// wallet has seen every block up to that height, and a reorg within the chain source's
    /// bound cannot replace them. A broadcast mined at or below it drops its pending inputs. A
    /// broadcast not mined there that no later block accepts is recorded as unmineable once,
    /// which drops its pending inputs, frees its idempotency key for a replacement, and
    /// reopens the dispense holds finalized with it; [`WalletEvent::PendingBroadcastUnmineable`]
    /// reports it and [`WalletEvent::HoldDispenseUnmineable`] reports each hold. A
    /// transaction a rewind removed from the chain is judged the same way. Judging at the
    /// visible tip instead could report a broadcast that an unscanned or reorged block still
    /// mines, and its replacement would pay twice.
    ///
    /// The wallet's own scan is the primary evidence; the chain source is a second guard. Before
    /// the first verdict in a call, the wallet reads the source's reorg window and pins an epoch
    /// whose visible tip covers the judged height. It records a broadcast unmineable only when
    /// the window meets [`crate::WalletOptions::minimum_source_reorg_window_blocks`], the source
    /// reports the transaction not found at that epoch, and the epoch is still current. One the
    /// source reports mined is recorded mined at that height, with the earlier steps of its
    /// send, and keeps its key. A send whose earlier steps' outputs the wallet could not record
    /// is never judged. Every other case leaves the broadcast unjudged until a later sync and
    /// publishes [`WalletEvent::PendingBroadcastVerdictWithheld`] once.
    pub(crate) async fn retire_pending_broadcasts(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<(), WalletError> {
        let storage = &self.inner.storage;
        let (Some(scanned_height), Some(settled_tip_height)) = (
            storage.fully_scanned_height().await?,
            storage.find_settled_tip().await?,
        ) else {
            return Ok(());
        };
        let judged_height = scanned_height.min(settled_tip_height);
        if self.is_source_reorg_window_too_shallow() {
            let _ = self
                .verify_source_reorg_window(chain, self.source_lookup_timeout())
                .await;
        }
        let mut pass = VerdictPass {
            chain,
            judged_height,
            scan_blind_sends: self.inner.scan_blind_sends.lock().clone(),
            gate: None,
            withheld: HashSet::new(),
        };
        for pending in storage.list_pending_broadcast_transactions().await? {
            let tx_id = pending.broadcast_tx_id;
            match pending.mined_height {
                Some(mined_height) if mined_height <= judged_height => {
                    storage
                        .clear_pending_broadcast_inputs_for_mined(&[tx_id])
                        .await?;
                    continue;
                }
                Some(_) => continue,
                None => {}
            }
            let header = match parse_transaction_header(&pending.raw_bytes) {
                Ok(header) => header,
                Err(error) => {
                    tracing::error!(
                        target: "zally::wallet",
                        event = "pending_broadcast_unreadable",
                        %tx_id,
                        %error,
                        "stored pending broadcast could not be parsed"
                    );
                    continue;
                }
            };
            let Err(reason) = header.check_mineable(self.network(), judged_height) else {
                continue;
            };
            match self
                .judge_presence(&mut pass, tx_id, &pending.raw_bytes)
                .await
            {
                ChainPresence::Absent => {
                    if let Some(holds) = storage
                        .record_unmineable_transaction(tx_id, judged_height)
                        .await?
                    {
                        self.report_unmineable(tx_id, &reason, judged_height, holds);
                    }
                }
                ChainPresence::Mined => {}
                ChainPresence::Unknown(cause) | ChainPresence::Unresponsive(cause) => {
                    self.withhold_verdict(tx_id, &cause);
                    pass.withheld.insert(tx_id);
                }
            }
        }
        let withheld = pass.withheld;
        self.inner
            .withheld_verdicts
            .lock()
            .retain(|tx_id| withheld.contains(tx_id));
        Ok(())
    }

    /// Establishes whether `tx_id`, which the wallet's scan judged unmineable, is absent from
    /// the chain, the only answer that lets the verdict record it unmineable.
    ///
    /// For a send the scan may have passed blind, absence is never established: the source's
    /// answer can still record it mined, but `NotFound` alone cannot free its key.
    async fn judge_presence(
        &self,
        pass: &mut VerdictPass<'_>,
        tx_id: TxId,
        raw_bytes: &[u8],
    ) -> ChainPresence {
        let blind_cause = match pass.scan_blind_sends {
            None => Some(
                "the wallet has not stored the carried steps of unsettled sends since it opened, \
                 so its scan may not recognize a payment mined",
            ),
            Some(ref blind) if blind.contains(&tx_id) => Some(
                "the wallet scanned without the outputs of this send's earlier steps, so its \
                 scan may have missed the payment mined",
            ),
            Some(_) => None,
        };
        let gate = match pass.gate {
            Some(ref gate) => gate,
            None => pass
                .gate
                .insert(self.open_verdict_gate(pass.chain, pass.judged_height).await),
        };
        let presence = match gate {
            VerdictGate::Open(epoch) => {
                let pinned = PinnedChain {
                    chain: pass.chain,
                    epoch: *epoch,
                };
                self.find_chain_presence(pinned, tx_id, raw_bytes).await
            }
            VerdictGate::Closed(cause) => ChainPresence::Unknown(cause.clone()),
        };
        if let ChainPresence::Unresponsive(ref cause) = presence {
            pass.gate = Some(VerdictGate::Closed(cause.clone()));
        }
        match (blind_cause, presence) {
            (Some(cause), ChainPresence::Absent) => ChainPresence::Unknown(cause.to_owned()),
            (_, presence) => presence,
        }
    }

    /// Reads the source's reorg window and pins an epoch whose visible tip covers
    /// `judged_height`.
    async fn open_verdict_gate(
        &self,
        chain: &dyn ChainSource,
        judged_height: BlockHeight,
    ) -> VerdictGate {
        if !self
            .verify_source_reorg_window(chain, self.source_lookup_timeout())
            .await
        {
            return VerdictGate::Closed(
                "the chain source's reorg window is not verified to meet the minimum".to_owned(),
            );
        }
        match tokio::time::timeout(self.source_lookup_timeout(), chain.current_epoch()).await {
            Ok(Ok(epoch))
                if epoch.network() == self.network()
                    && epoch.visible_tip().height >= judged_height =>
            {
                VerdictGate::Open(epoch)
            }
            Ok(Ok(epoch)) => VerdictGate::Closed(format!(
                "the chain source's epoch, visible to height {}, does not cover judged height {}",
                epoch.visible_tip().height.as_u32(),
                judged_height.as_u32()
            )),
            Ok(Err(error)) => {
                VerdictGate::Closed(format!("could not pin a chain source epoch: {error}"))
            }
            Err(_elapsed) => {
                VerdictGate::Closed("pinning a chain source epoch timed out".to_owned())
            }
        }
    }

    /// Asks the pinned source whether `tx_id` is in the canonical chain at its epoch.
    ///
    /// `NotFound` counts as absent only while `epoch` is still the source's current epoch, so a
    /// lookup that raced an epoch change cannot pass for absence. A transaction the source
    /// reports mined is recorded mined.
    async fn find_chain_presence(
        &self,
        pinned: PinnedChain<'_>,
        tx_id: TxId,
        raw_bytes: &[u8],
    ) -> ChainPresence {
        let PinnedChain { chain, epoch } = pinned;
        let timeout = self.source_lookup_timeout();
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "a status the wallet does not know cannot prove the transaction absent"
        )]
        match tokio::time::timeout(timeout, chain.transaction_status(epoch, tx_id)).await {
            Ok(Ok(TransactionStatus::NotFound)) => {
                match tokio::time::timeout(timeout, chain.current_epoch()).await {
                    Ok(Ok(current)) if current.id() == epoch.id() => ChainPresence::Absent,
                    Ok(Ok(_)) => ChainPresence::Unknown(
                        "the chain source's epoch changed during the lookup".to_owned(),
                    ),
                    Ok(Err(error)) => ChainPresence::Unknown(format!(
                        "could not confirm the chain source's epoch after the lookup: {error}"
                    )),
                    Err(_elapsed) => ChainPresence::Unresponsive(
                        "confirming the chain source's epoch timed out".to_owned(),
                    ),
                }
            }
            Ok(Ok(TransactionStatus::Confirmed {
                confirmed_at_height,
                ..
            })) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_mined_unrecognized",
                    %tx_id,
                    confirmed_at_height = confirmed_at_height.as_u32(),
                    "chain source reports a pending broadcast mined that the wallet's scan did \
                     not recognize; recording it mined"
                );
                self.record_send_confirmed(pinned, tx_id, raw_bytes, confirmed_at_height)
                    .await;
                ChainPresence::Mined
            }
            Ok(Ok(status)) => {
                ChainPresence::Unknown(format!("the chain source reports {status:?}"))
            }
            Ok(Err(error)) => {
                ChainPresence::Unknown(format!("the chain source lookup failed: {error}"))
            }
            Err(_elapsed) => {
                ChainPresence::Unresponsive("the chain source lookup timed out".to_owned())
            }
        }
    }

    /// Publishes [`WalletEvent::PendingBroadcastVerdictWithheld`] the first time a verdict on
    /// `tx_id` waits, and logs every time.
    fn withhold_verdict(&self, tx_id: TxId, cause: &str) {
        tracing::warn!(
            target: "zally::wallet",
            event = "pending_broadcast_verdict_withheld",
            %tx_id,
            cause,
            "pending broadcast looks unmineable but the wallet cannot confirm it yet; its key \
             stays bound"
        );
        if self.inner.withheld_verdicts.lock().insert(tx_id) {
            self.publish_event(WalletEvent::PendingBroadcastVerdictWithheld {
                tx_id,
                reason: cause.to_owned(),
            });
        }
    }

    fn source_lookup_timeout(&self) -> Duration {
        Duration::from_millis(self.inner.options.source_lookup_timeout_ms)
    }

    /// Reports a broadcast just recorded unmineable and each dispense hold it settled.
    fn report_unmineable(
        &self,
        tx_id: TxId,
        reason: &UnmineableReason,
        judged_height: BlockHeight,
        holds: Vec<UnmineableHold>,
    ) {
        tracing::warn!(
            target: "zally::wallet",
            event = "pending_broadcast_unmineable",
            %tx_id,
            %reason,
            judged_height = judged_height.as_u32(),
            "pending broadcast can never be mined; recreate it"
        );
        self.publish_event(WalletEvent::PendingBroadcastUnmineable {
            tx_id,
            reason: reason.to_string(),
        });
        for hold in holds {
            tracing::warn!(
                target: "zally::wallet",
                event = "hold_dispense_unmineable",
                hold_id = %hold.hold_id.as_uuid(),
                %tx_id,
                is_reopened = hold.is_reopened,
                "dispense hold was finalized with a transaction that can never be mined"
            );
            self.publish_event(WalletEvent::HoldDispenseUnmineable {
                hold_id: hold.hold_id,
                request_id: hold.request_id,
                tx_id,
                is_reopened: hold.is_reopened,
            });
        }
    }

    /// Records `tx_id` mined at `confirmed_at_height`, after the earlier steps of its send that
    /// the source also reports mined, so their outputs are known before the step that spends
    /// them. A failure is logged and retried by a later sync, since the broadcast stays pending.
    async fn record_send_confirmed(
        &self,
        pinned: PinnedChain<'_>,
        tx_id: TxId,
        raw_bytes: &[u8],
        confirmed_at_height: BlockHeight,
    ) {
        let storage = &self.inner.storage;
        let earlier_steps = match storage.list_send_steps(tx_id).await {
            Ok(steps) => steps,
            Err(error) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_steps_unreadable",
                    %tx_id,
                    %error,
                    "could not read the steps of a send the chain source reports mined"
                );
                Vec::new()
            }
        };
        let mut confirmed = Vec::new();
        for step in earlier_steps
            .into_iter()
            .filter(|step| step.tx_id != tx_id && step.mined_height.is_none())
        {
            let Some(step_bytes) = step.raw_bytes else {
                continue;
            };
            if let Ok(Ok(TransactionStatus::Confirmed {
                confirmed_at_height: step_height,
                ..
            })) = tokio::time::timeout(
                self.source_lookup_timeout(),
                pinned.chain.transaction_status(pinned.epoch, step.tx_id),
            )
            .await
            {
                confirmed.push((step_bytes, step_height));
            }
        }
        confirmed.push((raw_bytes.to_vec(), confirmed_at_height));
        for (step_bytes, step_height) in confirmed {
            if let Err(error) = storage
                .record_confirmed_broadcast(step_bytes, step_height)
                .await
            {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_confirmation_record_failed",
                    %tx_id,
                    %error,
                    "could not record a send the chain source reports mined; a later sync retries"
                );
                return;
            }
        }
    }
}
