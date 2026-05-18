//! Regression: `Wallet::list_exposed_addresses` returns previously-derived Unified
//! Addresses in derivation order, without advancing any diversifier index or burning a
//! BIP-44 transparent gap-limit slot.

use uuid::Uuid;
use zally_core::{AccountId, BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError, WalletOptions};

#[tokio::test]
async fn list_exposed_addresses_returns_baseline_after_create() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let (wallet, account_id) = create_wallet(&temp, network).await?;

    let exposed = wallet.list_exposed_addresses(account_id).await?;
    for entry in &exposed {
        assert_eq!(entry.network, network);
    }
    Ok(())
}

#[tokio::test]
async fn list_exposed_addresses_extends_when_address_derived() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let (wallet, account_id) = create_wallet(&temp, network).await?;

    let baseline = wallet.list_exposed_addresses(account_id).await?.len();

    let first = wallet.derive_next_address(account_id).await?;
    let second = wallet.derive_next_address(account_id).await?;

    let exposed = wallet.list_exposed_addresses(account_id).await?;
    assert_eq!(
        exposed.len(),
        baseline + 2,
        "two derivations must add two entries to the list"
    );
    let addresses: Vec<_> = exposed.iter().map(|entry| &entry.unified_address).collect();
    assert!(
        addresses.contains(&&first),
        "first derived UA missing from exposed list"
    );
    assert!(
        addresses.contains(&&second),
        "second derived UA missing from exposed list"
    );
    for entry in &exposed {
        assert_eq!(entry.network, network);
    }
    Ok(())
}

#[tokio::test]
async fn list_exposed_addresses_flags_transparent_receiver() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let (wallet, account_id) = create_wallet(&temp, network).await?;

    let with_transparent = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;

    let exposed = wallet.list_exposed_addresses(account_id).await?;
    let entry = exposed
        .iter()
        .find(|entry| entry.unified_address == with_transparent)
        .ok_or_else(|| TestError::Unexpected {
            reason: "transparent UA missing from list".to_owned(),
        })?;
    assert!(
        entry.has_transparent_receiver,
        "transparent UA must report has_transparent_receiver = true"
    );
    Ok(())
}

#[tokio::test]
async fn list_exposed_addresses_is_idempotent_and_gap_limit_invariant() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let (wallet, account_id) = create_wallet(&temp, network).await?;

    let before = wallet.derive_next_address(account_id).await?;
    let count_after_first_derive = wallet.list_exposed_addresses(account_id).await?.len();

    let first_list = wallet.list_exposed_addresses(account_id).await?;
    let second_list = wallet.list_exposed_addresses(account_id).await?;
    assert_eq!(
        first_list, second_list,
        "list_exposed_addresses must be a pure snapshot"
    );

    let after = wallet.derive_next_address(account_id).await?;
    assert_ne!(
        before, after,
        "derive_next_address after list must advance by exactly one diversifier slot"
    );

    let count_after_second_derive = wallet.list_exposed_addresses(account_id).await?.len();
    assert_eq!(
        count_after_second_derive,
        count_after_first_derive + 1,
        "the in-between list calls must not have added entries"
    );
    Ok(())
}

#[tokio::test]
async fn list_exposed_addresses_unknown_account_returns_account_not_found() -> Result<(), TestError>
{
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let (wallet, _account_id) = create_wallet(&temp, network).await?;

    let unknown = AccountId::from_uuid(Uuid::nil());
    match wallet.list_exposed_addresses(unknown).await {
        Err(WalletError::AccountNotFound) => Ok(()),
        Err(other) => Err(TestError::Unexpected {
            reason: format!("expected AccountNotFound, got {other:?}"),
        }),
        Ok(addresses) => Err(TestError::Unexpected {
            reason: format!("expected error, got {} entries", addresses.len()),
        }),
    }
}

async fn create_wallet(
    temp: &TempWalletPath,
    network: Network,
) -> Result<(Wallet, AccountId), TestError> {
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) = Wallet::create(
        &chain,
        network,
        sealing,
        storage,
        BlockHeight::from(1),
        WalletOptions::default(),
    )
    .await?;
    Ok((wallet, account_id))
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("unexpected test outcome: {reason}")]
    Unexpected { reason: String },
}
