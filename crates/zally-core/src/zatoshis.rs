//! Non-negative integer zatoshi amount.

/// Non-negative integer zatoshi amount.
///
/// Construction above [`Zatoshis::MAX`] is rejected. The unit suffix on field and parameter
/// names (`_zat`) is required by [Public interfaces §Required suffixes] so a bare `u64` is
/// never mistaken for a different unit.
///
/// [Public interfaces §Required suffixes]: ../../docs/architecture/public-interfaces.md
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Zatoshis(u64);

impl Zatoshis {
    /// The maximum representable amount: 21,000,000 ZEC expressed in zatoshis.
    pub const MAX: Self = Self(2_100_000_000_000_000);

    /// The zero amount.
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Returns the inner zatoshi count.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for Zatoshis {
    type Error = ZatoshisError;

    fn try_from(zat: u64) -> Result<Self, Self::Error> {
        if zat > Self::MAX.0 {
            Err(ZatoshisError::ExceedsMaxMoney { attempted_zat: zat })
        } else {
            Ok(Self(zat))
        }
    }
}

/// Error returned when [`Zatoshis`] construction fails.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ZatoshisError {
    /// Input exceeds `MAX_MONEY`.
    ///
    /// `not_retryable`: the same input will always exceed the cap; the caller must clamp.
    #[error("zatoshi amount {attempted_zat} exceeds MAX_MONEY (2100000000000000 zatoshis)")]
    ExceedsMaxMoney {
        /// The rejected input.
        attempted_zat: u64,
    },
}

impl ZatoshisError {
    /// Whether the same input may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::ExceedsMaxMoney { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zatoshis_max_accepted() -> Result<(), ZatoshisError> {
        let max = Zatoshis::try_from(Zatoshis::MAX.as_u64())?;
        assert_eq!(max, Zatoshis::MAX);
        Ok(())
    }

    #[test]
    fn zatoshis_above_max_rejected() {
        let outcome = Zatoshis::try_from(Zatoshis::MAX.as_u64() + 1);
        assert!(matches!(
            outcome,
            Err(ZatoshisError::ExceedsMaxMoney { .. })
        ));
        if let Err(err) = outcome {
            assert!(!err.is_retryable());
        }
    }

    #[test]
    fn zatoshis_zero_round_trip() -> Result<(), ZatoshisError> {
        assert_eq!(Zatoshis::zero().as_u64(), 0);
        assert_eq!(Zatoshis::try_from(0_u64)?, Zatoshis::zero());
        Ok(())
    }
}
