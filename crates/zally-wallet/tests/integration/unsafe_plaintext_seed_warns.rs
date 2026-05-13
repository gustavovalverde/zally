//! REQ-KEYS-2 — opening a wallet with plaintext sealing emits a WARN-level event.
//!
//! Gated behind `unsafe_plaintext_seed`.

use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use zally_core::{BlockHeight, Network};
use zally_keys::PlaintextSealing;
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn unsafe_plaintext_seed_warns() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        events: Arc::clone(&events),
    });
    let _default_guard = tracing::subscriber::set_default(subscriber);

    let sealing = PlaintextSealing::new(temp.seed_path());
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let _ = Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    let sealing = PlaintextSealing::new(temp.seed_path());
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let _ = Wallet::open(network, sealing, storage).await?;

    let plaintext_count = {
        let captured = events.lock().map_err(|_| TestError::Mutex)?;
        captured
            .iter()
            .filter(|e| e.contains("plaintext_seed_in_use"))
            .count()
    };
    assert!(
        plaintext_count >= 2,
        "expected at least 2 plaintext_seed_in_use events (create + open), got {plaintext_count}"
    );
    Ok(())
}

struct CaptureLayer {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        if event.metadata().target() != "zally::wallet" {
            return;
        }
        let mut buf = String::new();
        let mut visitor = StringVisitor { buf: &mut buf };
        event.record(&mut visitor);
        if let Ok(mut guard) = self.events.lock() {
            guard.push(buf);
        }
    }
}

struct StringVisitor<'a> {
    buf: &'a mut String,
}

impl tracing::field::Visit for StringVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, field_value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.buf, "{}={:?} ", field.name(), field_value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, field_value: &str) {
        use std::fmt::Write;
        let _ = write!(self.buf, "{}={} ", field.name(), field_value);
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("events mutex poisoned")]
    Mutex,
}
