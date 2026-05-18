//! Regression: `Wallet::derive_next_address_with_transparent` returns a Unified Address
//! whose transparent receiver is populated.

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn derive_address_populates_transparent() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;

    // The upstream BIP-44 transparent gap limit defaults to 10 and pre-generates the gap
    // ahead of the reserved address, so a single
    // `derive_next_address_with_transparent` on a fresh wallet exhausts the
    // safe-reservation budget. Further calls require an intervening on-chain output to a
    // reserved address. The regression we care about is whether the first call returns a
    // UA whose transparent receiver is populated.
    let ua = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;
    assert!(
        ua.transparent().is_some(),
        "derive_next_address_with_transparent returned a UA without a transparent receiver"
    );
    Ok(())
}
