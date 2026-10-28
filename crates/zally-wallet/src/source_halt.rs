//! Detection of a chain source that stopped following the chain.
//!
//! A source that halts keeps answering with the last chain it saw, so a wallet scanning it
//! looks healthy while it serves a frozen branch. The sync driver watches the source's
//! visible tip: once the tip has not moved for [`crate::SyncDriverOptions::visible_tip_stall_limit_ms`]
//! and a fresh epoch read still shows it, the driver halts with
//! [`SourceHalt::VisibleTipStalled`], refuses new spends, and resumes when the tip moves.
//!
//! A source that reports a typed halt status maps it onto a further [`SourceHalt`] variant and
//! enters it through [`SourceTipWatch::halt`], so the driver's halt and resume path stays the
//! same.

use zally_chain::BlockId;
use zally_core::BlockHeight;

/// Why the chain source stopped following the chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SourceHalt {
    /// The source kept answering, but its visible tip stayed at `visible_tip_height` since
    /// `unchanged_since_ms` for longer than the configured stall limit.
    VisibleTipStalled {
        /// Visible tip the source keeps reporting.
        visible_tip_height: BlockHeight,
        /// Unix milliseconds when the driver first saw that tip.
        unchanged_since_ms: u64,
    },
}

impl std::fmt::Display for SourceHalt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VisibleTipStalled {
                visible_tip_height,
                unchanged_since_ms,
            } => write!(
                formatter,
                "chain source visible tip stalled at height {} since unix ms {unchanged_since_ms}",
                visible_tip_height.as_u32()
            ),
        }
    }
}

/// The driver's record of the source's visible tip and whether it has halted.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SourceTipWatch {
    visible_tip: Option<BlockId>,
    unchanged_since_ms: u64,
    halted: Option<HaltedSource>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HaltedSource {
    pub(crate) cause: SourceHalt,
    pub(crate) since_ms: u64,
}

/// What a tip observation changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceTipChange {
    /// The source still reports the tip the watch last recorded.
    Unchanged,
    /// The source reports a different tip; a halted watch resumed.
    Moved,
}

impl SourceTipWatch {
    /// A watch whose stall clock starts at `now_ms`.
    pub(crate) const fn started_at(now_ms: u64) -> Self {
        Self {
            visible_tip: None,
            unchanged_since_ms: now_ms,
            halted: None,
        }
    }

    pub(crate) const fn halted(&self) -> Option<HaltedSource> {
        self.halted
    }

    /// Records the source's visible tip. A tip other than the recorded one restarts the stall
    /// clock and clears a halt.
    pub(crate) fn observe(&mut self, visible_tip: BlockId, now_ms: u64) -> SourceTipChange {
        if self.visible_tip == Some(visible_tip) {
            return SourceTipChange::Unchanged;
        }
        self.visible_tip = Some(visible_tip);
        self.unchanged_since_ms = now_ms;
        self.halted = None;
        SourceTipChange::Moved
    }

    /// Whether the stall clock has run past `stall_limit_ms`, so the driver should read a
    /// fresh epoch before deciding.
    pub(crate) const fn is_due_for_probe(&self, now_ms: u64, stall_limit_ms: u64) -> bool {
        now_ms.saturating_sub(self.unchanged_since_ms) >= stall_limit_ms
    }

    /// Halts the watch when its recorded tip has stayed unchanged past `stall_limit_ms`.
    ///
    /// Callers first [`Self::observe`] the tip a fresh epoch read returned, so a stall is
    /// only declared against a source that is still answering. Returns the halt the watch
    /// entered, or `None` when it was already halted, has recorded no tip, or the stall
    /// limit has not passed.
    pub(crate) fn halt_if_stalled(
        &mut self,
        now_ms: u64,
        stall_limit_ms: u64,
    ) -> Option<HaltedSource> {
        let visible_tip = self.visible_tip?;
        if self.halted.is_some() || !self.is_due_for_probe(now_ms, stall_limit_ms) {
            return None;
        }
        Some(self.halt(
            SourceHalt::VisibleTipStalled {
                visible_tip_height: visible_tip.height,
                unchanged_since_ms: self.unchanged_since_ms,
            },
            now_ms,
        ))
    }

    /// Halts the watch with `cause` until the source's visible tip moves.
    pub(crate) const fn halt(&mut self, cause: SourceHalt, now_ms: u64) -> HaltedSource {
        let halted = HaltedSource {
            cause,
            since_ms: now_ms,
        };
        self.halted = Some(halted);
        halted
    }
}

#[cfg(test)]
mod tests {
    use zally_core::BlockHash;

    use super::*;

    fn tip(height: u32) -> BlockId {
        BlockId {
            height: BlockHeight::from(height),
            hash: BlockHash::from_bytes([u8::try_from(height % 251).unwrap_or(0); 32]),
        }
    }

    #[test]
    fn a_watch_without_a_recorded_tip_never_halts() {
        let mut watch = SourceTipWatch::started_at(0);
        assert!(watch.halt_if_stalled(10_000, 1_000).is_none());
        assert!(watch.halted().is_none());
    }

    #[test]
    fn a_tip_unchanged_past_the_stall_limit_halts_until_it_moves() {
        let mut watch = SourceTipWatch::started_at(0);
        assert_eq!(watch.observe(tip(10), 0), SourceTipChange::Moved);
        assert_eq!(watch.observe(tip(10), 999), SourceTipChange::Unchanged);
        assert!(watch.halt_if_stalled(999, 1_000).is_none());
        let halted = watch.halt_if_stalled(1_000, 1_000);
        assert_eq!(
            halted.map(|halted| halted.cause),
            Some(SourceHalt::VisibleTipStalled {
                visible_tip_height: BlockHeight::from(10),
                unchanged_since_ms: 0,
            })
        );
        assert!(
            watch.halt_if_stalled(5_000, 1_000).is_none(),
            "an already-halted watch reports no new halt"
        );
        assert!(watch.halted().is_some());

        assert_eq!(watch.observe(tip(11), 6_000), SourceTipChange::Moved);
        assert!(watch.halted().is_none(), "a moved tip resumes the source");
        assert!(!watch.is_due_for_probe(6_500, 1_000));
    }

    #[test]
    fn a_replaced_tip_at_the_same_height_counts_as_movement() {
        let mut watch = SourceTipWatch::started_at(0);
        watch.observe(tip(10), 0);
        let replaced = BlockId {
            height: BlockHeight::from(10),
            hash: BlockHash::from_bytes([0xee; 32]),
        };
        assert_eq!(watch.observe(replaced, 2_000), SourceTipChange::Moved);
    }
}
