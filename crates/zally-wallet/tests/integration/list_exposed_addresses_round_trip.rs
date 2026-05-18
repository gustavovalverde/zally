//! Regression: `Wallet::list_exposed_addresses` returns previously-derived Unified
//! Addresses in derivation order, without advancing any diversifier index or burning a
//! BIP-44 transparent gap-limit slot.

use uuid::Uuid;
use zally_core::AccountId;
use zally_wallet::WalletError;

use super::fixtures::{TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn list_exposed_addresses_returns_baseline_after_create() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let exposed = wallet.list_exposed_addresses(account_id).await?;
    for entry in &exposed {
        assert_eq!(entry.network, network);
    }
    Ok(())
}

#[tokio::test]
async fn list_exposed_addresses_extends_when_address_derived() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

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
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;

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
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;

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
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;

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

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("unexpected test outcome: {reason}")]
    Unexpected { reason: String },
}
