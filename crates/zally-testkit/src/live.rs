//! Live-node test gating.
//!
//! Tests that touch a real Zcash node or chain-source endpoint call [`require_live`] from their
//! `#[ignore = LIVE_TEST_IGNORE_REASON]` body. The gate consults `ZALLY_TEST_LIVE=1` so the
//! validation-gate profile (no env var set) skips them via `--no-run-ignored`, and the
//! live-test profile picks them up via `--run-ignored=all`.

use std::env;
use std::sync::Once;

use zally_core::Network;

/// Reason string for `#[ignore]` on live tests across Zally crates.
pub const LIVE_TEST_IGNORE_REASON: &str = "live test; see CLAUDE.md §Live Node Tests";

/// Environment variable that opts in to live tests.
pub const LIVE_TEST_ENV: &str = "ZALLY_TEST_LIVE";

/// Environment variable that names the Zcash network under test.
pub const NETWORK_ENV: &str = "ZALLY_NETWORK";

/// Environment variable that names the zinder query endpoint.
pub const ZINDER_ENDPOINT_ENV: &str = "ZINDER_ENDPOINT";

/// Environment variable that opts in to mainnet live tests on top of [`LIVE_TEST_ENV`].
pub const ALLOW_MAINNET_ENV: &str = "ZALLY_TEST_ALLOW_MAINNET";

/// Installs a global `tracing_subscriber` if one is not already installed.
///
/// Holds a `Drop` guard so callers can keep it alive for the test's lifetime. The guard is
/// a no-op marker; callers that want log capture extend it in place.
#[must_use = "hold the returned guard for the duration of the test"]
pub fn init() -> InitGuard {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt::try_init();
    });
    InitGuard { _private: () }
}

/// Guard returned by [`init`]. Drops to a no-op for now.
pub struct InitGuard {
    _private: (),
}

impl Drop for InitGuard {
    fn drop(&mut self) {}
}

/// Returns `Ok(())` when live tests are enabled via `ZALLY_TEST_LIVE=1`.
///
/// `not_retryable`: env-driven configuration that does not change within a process.
///
/// # Errors
///
/// Returns [`LiveTestError::NotConfigured`] when the gate is off.
pub fn require_live() -> Result<(), LiveTestError> {
    if env_flag_is_set(LIVE_TEST_ENV) {
        Ok(())
    } else {
        Err(LiveTestError::NotConfigured { var: LIVE_TEST_ENV })
    }
}

/// Returns the network under test from `ZALLY_NETWORK`, defaulting to regtest when unset.
///
/// `not_retryable`.
///
/// # Errors
///
/// Returns [`LiveTestError::UnknownNetwork`] when the value is not one of `mainnet`,
/// `testnet`, or `regtest`. Returns [`LiveTestError::MainnetGated`] for `mainnet` unless
/// `ZALLY_TEST_ALLOW_MAINNET=1` is also set.
pub fn require_network() -> Result<Network, LiveTestError> {
    match env::var(NETWORK_ENV).ok().as_deref() {
        None | Some("" | "regtest") => Ok(Network::regtest_all_at_genesis()),
        Some("testnet") => Ok(Network::Testnet),
        Some("mainnet") => {
            if env_flag_is_set(ALLOW_MAINNET_ENV) {
                Ok(Network::Mainnet)
            } else {
                Err(LiveTestError::MainnetGated {
                    var: ALLOW_MAINNET_ENV,
                })
            }
        }
        Some(other) => Err(LiveTestError::UnknownNetwork {
            var: NETWORK_ENV,
            raw: other.to_owned(),
        }),
    }
}

/// Returns the zinder query endpoint URL from `ZINDER_ENDPOINT`.
///
/// `not_retryable`.
///
/// # Errors
///
/// Returns [`LiveTestError::MissingEnv`] when the var is unset or empty.
pub fn require_zinder_endpoint() -> Result<String, LiveTestError> {
    let raw = env::var(ZINDER_ENDPOINT_ENV).map_err(|_| LiveTestError::MissingEnv {
        var: ZINDER_ENDPOINT_ENV,
    })?;
    if raw.is_empty() {
        Err(LiveTestError::MissingEnv {
            var: ZINDER_ENDPOINT_ENV,
        })
    } else {
        Ok(raw)
    }
}

fn env_flag_is_set(var: &str) -> bool {
    env::var(var)
        .ok()
        .is_some_and(|raw| matches!(raw.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

/// Error returned by the live-test gates.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LiveTestError {
    /// Live tests are disabled because the gate env var is unset or non-truthy.
    #[error("live tests are disabled; set {var}=1 to enable")]
    NotConfigured {
        /// Name of the gating environment variable.
        var: &'static str,
    },
    /// A required environment variable is missing.
    #[error("required environment variable {var} is not set")]
    MissingEnv {
        /// Name of the missing environment variable.
        var: &'static str,
    },
    /// `ZALLY_NETWORK` was set to an unrecognised value.
    #[error("unknown {var} value: {raw}")]
    UnknownNetwork {
        /// Name of the offending environment variable.
        var: &'static str,
        /// The user-supplied value.
        raw: String,
    },
    /// Mainnet was requested without the second opt-in.
    #[error("mainnet live tests require {var}=1 in addition to ZALLY_TEST_LIVE=1")]
    MainnetGated {
        /// Name of the additional opt-in environment variable.
        var: &'static str,
    },
}

impl LiveTestError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::NotConfigured { .. }
            | Self::MissingEnv { .. }
            | Self::UnknownNetwork { .. }
            | Self::MainnetGated { .. } => false,
        }
    }
}
