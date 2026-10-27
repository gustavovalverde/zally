//! A wallet database written by an earlier Zally release opens on the current stack with its
//! identity, exposed addresses, balance, and commitment trees intact.
//!
//! Fixture (`tests/fixtures/wallet_rc4_testnet.db`): a Testnet wallet written by Zally
//! `4eaa0bb` on `zcash_client_sqlite 0.22.0-rc.4`. Its account comes from the seed
//! `[0x5a; 32]`, anchored at height 4009899. It scanned the checked-in compact blocks
//! 4009900-4009949, derived two shielded-only and one transparent-bearing Unified Address at
//! chain tip 4009999, recorded one 250000-zatoshi transparent UTXO mined at 4009940, and
//! recorded chain tips 4009999 (visible) and 4009949 (settled). Opening it applies every
//! wallet migration added since rc.4. Tests open a copy so the checked-in file never changes.

use std::path::Path;

use tempfile::TempDir;
use zally_core::{AccountId, BlockHeight, Network, Zatoshis};
use zally_keys::{SeedMaterial, SeedMaterialError};
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};

const FIXTURE_ACCOUNT_ID: &str = "96d98677-f009-584d-84d9-8d91e219314b";

/// Exposed addresses as the rc.4 wallet listed them: encoding, diversifier index, whether a
/// transparent receiver is present, and exposure height.
const FIXTURE_EXPOSED_ADDRESSES: [(&str, u128, bool, u32); 4] = [
    (
        "utest1avqmg5ewarmdlqsvzd46pl7hpedacz7q9f75aatgthvfe34ljs08tx4xzn6evk5j0mwp4cyy6a7ced79erfv9w6stkyx6zzhzhae6tz8ar29ej8r8980cca5zkvkhx7dshsd3zfdmxsgqa9j0wze0exf99er4uzdqax32xp69t02fedd80u53rucyk87vd8ujdpnfxl6r8hk7vxs5vp",
        0,
        true,
        4_009_900,
    ),
    (
        "utest13tne7pvnemwyfq0frfy8m3xd4cgyfanseejz25huvqnxta7y5qgxr63kx8kdc8ucj5ftgzsk7j0gghhy22d9kdysna9ukz8wkr2lcgztscpzkj2n2sjqdsjdzv4ll4s6t4gapr2elwn3p6sut6kyzvyr3gjn0pz0j9g79qnumjrchq9nlejlrx4sjd76nreght3sgakgrda2xcudjlh",
        2,
        true,
        4_009_940,
    ),
    (
        "utest1hd6awpg7dyekyck9dkrv3pu99g4czr2lr44kfgshadkll4kxkkq2y9rhkghq9qldvtj7n9ykyrs6xlgrp757jq7uwpvvx5yddhzwjwcsr4ksd3x7pcf3kpu0er2vh3gd8axlk4d4vjkysljkjyk7rwt227r3rl8c34sggjpuavrwu654",
        4_610_435_759,
        false,
        4_009_999,
    ),
    (
        "utest13h7vcnn8d78702r25w54tm8qrslwmmj7fpt7pnrjazj0nqrgpsrglgfqw67t58qemc0pahn9244fxmlr8ugs3axxkqvyvkhfts4x05dayqe4tex907e9d03ly04355s2ehfv0vsemx9ll0f7qa3z36eh0juv7cx0z25ac62zdu5hk4ep",
        4_610_435_760,
        false,
        4_009_999,
    ),
];

/// Commitment tree roots at the fixture's latest checkpoint (4009949), little-endian hex.
const FIXTURE_SAPLING_ROOT_HEX: &str =
    "1cb5150e0187e8b5dd7db31c18ac28a358337dda20fbd6eeb8e72227a7742303";
const FIXTURE_ORCHARD_ROOT_HEX: &str =
    "bbe8caf92bd21fac598ec76edddff59244ecd6a292a9ce7c2a51004364902101";

const FIXTURE_TRANSPARENT_UTXO_ZAT: u64 = 250_000;

async fn open_fixture_copy() -> Result<(Sqlite, AccountId, TempDir), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wallet_rc4_testnet.db"),
        &db_path,
    )?;
    let storage = Sqlite::new(SqliteOptions::for_network(Network::Testnet, db_path));
    storage.open_or_create().await?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])?;
    let account_id = storage
        .find_account_for_seed(&seed)
        .await?
        .ok_or(TestError::AccountMissing)?;
    Ok((storage, account_id, temp))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_keeps_its_account_identity() -> Result<(), TestError> {
    let (_storage, account_id, _temp) = open_fixture_copy().await?;
    assert_eq!(account_id.as_uuid().to_string(), FIXTURE_ACCOUNT_ID);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_keeps_its_exposed_addresses() -> Result<(), TestError> {
    let (storage, account_id, _temp) = open_fixture_copy().await?;
    let params = Network::Testnet.to_parameters();
    let listed: Vec<(String, u128, bool, Option<BlockHeight>)> = storage
        .list_exposed_addresses(account_id)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.unified_address.encode(&params),
                u128::from(row.diversifier_index),
                row.has_transparent_receiver,
                row.exposed_at_height,
            )
        })
        .collect();
    let expected: Vec<(String, u128, bool, Option<BlockHeight>)> = FIXTURE_EXPOSED_ADDRESSES
        .iter()
        .map(|(encoded, index, has_transparent, height)| {
            (
                (*encoded).to_owned(),
                *index,
                *has_transparent,
                Some(BlockHeight::from(*height)),
            )
        })
        .collect();
    assert_eq!(listed, expected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_keeps_its_balance_and_scan_progress() -> Result<(), TestError> {
    let (storage, account_id, _temp) = open_fixture_copy().await?;
    let balance = storage.get_account_balance(account_id).await?;
    assert_eq!(balance.sapling_zat, Zatoshis::zero());
    assert_eq!(balance.orchard_zat, Zatoshis::zero());
    assert_eq!(balance.ironwood_zat, Zatoshis::zero());
    assert_eq!(
        balance.transparent_mature_zat,
        Zatoshis::try_from(FIXTURE_TRANSPARENT_UTXO_ZAT).map_err(|_| TestError::Fixture)?
    );
    assert_eq!(balance.transparent_immature_zat, Zatoshis::zero());
    assert_eq!(balance.as_of_height, Some(BlockHeight::from(4_009_999)));
    assert_eq!(
        storage.fully_scanned_height().await?,
        Some(BlockHeight::from(4_009_949))
    );
    assert_eq!(
        storage.account_birthday().await?,
        BlockHeight::from(4_009_900)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_keeps_its_commitment_tree_roots() -> Result<(), TestError> {
    let (storage, _account_id, _temp) = open_fixture_copy().await?;
    let roots = storage.commitment_tree_roots().await?;
    assert_eq!(
        roots.sapling.map(hex::encode).as_deref(),
        Some(FIXTURE_SAPLING_ROOT_HEX)
    );
    assert_eq!(
        roots.orchard.map(hex::encode).as_deref(),
        Some(FIXTURE_ORCHARD_ROOT_HEX)
    );
    assert_eq!(roots.ironwood, None);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedMaterialError),
    #[error("the fixture seed has no account in the upgraded wallet")]
    AccountMissing,
    #[error("fixture constant is out of range")]
    Fixture,
}
