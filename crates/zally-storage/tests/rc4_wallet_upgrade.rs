//! A wallet database written by an earlier Zally release opens on the current stack with its
//! identity, exposed addresses, balance, and commitment trees intact, and keeps working.
//!
//! Fixture (`tests/fixtures/wallet_rc4_testnet.db`): a Testnet wallet written by Zally
//! `4eaa0bb` on `zcash_client_sqlite 0.22.0-rc.4`. Its account comes from the seed
//! `[0x5a; 32]`, anchored at height 4009899. It scanned the checked-in compact blocks
//! 4009900-4009949, derived two shielded-only and one transparent-bearing Unified Address at
//! chain tip 4009999, recorded one 250000-zatoshi transparent UTXO mined at 4009940, and
//! recorded chain tips 4009999 (visible) and 4009949 (settled). Opening it applies every
//! wallet migration added since rc.4. Tests open a copy so the checked-in file never changes.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zally_core::{AccountId, BlockHeight, CompactBlockArtifact, Network, TxId, Zatoshis};
use zally_keys::{SeedMaterial, SeedMaterialError};
use zally_storage::{ScanRequest, Sqlite, SqliteOptions, StorageError, WalletStorage};
use zcash_protocol::ShieldedPool;

#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

use scan_artifact::{FixtureError, testnet_chain_state_at, testnet_compact_blocks};

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

const FIXTURE_SCANNED_HEIGHT: u32 = 4_009_949;
const FIXTURE_RANGE_END: u32 = 4_009_999;

/// Wallet migrations the rc.4 fixture already applied, and the count after upgrading.
const RC4_MIGRATION_COUNT: i64 = 57;
const CURRENT_MIGRATION_COUNT: i64 = 75;

/// `zcash_client_sqlite`'s `fix_bad_ironwood_change_flagging` migration id.
const FIX_BAD_IRONWOOD_CHANGE_FLAGGING: uuid::Uuid =
    uuid::uuid!("cc104d0d-54d6-4e07-9404-202676561d94");

/// `zcash_client_sqlite` encodings the change-flag scenario writes directly.
const KEY_SCOPE_EXTERNAL: i64 = 0;
const KEY_SCOPE_INTERNAL: i64 = 1;
const IRONWOOD_POOL_CODE: i64 = 4;

fn fixture_seed() -> Result<SeedMaterial, TestError> {
    Ok(SeedMaterial::from_raw_bytes(vec![0x5a; 32])?)
}

/// Copies the fixture into a fresh directory without opening it.
fn copy_fixture() -> Result<(PathBuf, TempDir), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wallet_rc4_testnet.db"),
        &db_path,
    )?;
    Ok((db_path, temp))
}

async fn open_copied_fixture(db_path: PathBuf) -> Result<(Sqlite, AccountId), TestError> {
    let storage = Sqlite::new(SqliteOptions::for_network(Network::Testnet, db_path));
    storage.open_or_create().await?;
    let account_id = storage
        .find_account_for_seed(&fixture_seed()?)
        .await?
        .ok_or(TestError::AccountMissing)?;
    Ok((storage, account_id))
}

async fn open_fixture_copy() -> Result<(Sqlite, AccountId, TempDir), TestError> {
    let (db_path, temp) = copy_fixture()?;
    let (storage, account_id) = open_copied_fixture(db_path).await?;
    Ok((storage, account_id, temp))
}

fn applied_migration_ids(db_path: &Path) -> Result<Vec<uuid::Uuid>, TestError> {
    let conn = rusqlite::Connection::open(db_path)?;
    let mut stmt = conn.prepare("SELECT id FROM schemer_migrations")?;
    let ids = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .map(|id| {
            let id = id?;
            uuid::Uuid::from_slice(&id).map_err(|_| TestError::Fixture)
        })
        .collect::<Result<Vec<_>, TestError>>()?;
    Ok(ids)
}

async fn scan(
    storage: &Sqlite,
    blocks: Vec<CompactBlockArtifact>,
    anchor_height: u32,
) -> Result<(), TestError> {
    storage
        .scan_blocks(ScanRequest::new(
            blocks,
            BlockHeight::from(anchor_height + 1),
            testnet_chain_state_at(anchor_height)?,
        ))
        .await?;
    Ok(())
}

async fn assert_roots_match_chain(storage: &Sqlite, height: u32) -> Result<(), TestError> {
    let wallet = storage.commitment_tree_roots().await?;
    let chain = storage
        .tree_state_roots(testnet_chain_state_at(height)?)
        .await?;
    assert_eq!(wallet.sapling, chain.sapling, "Sapling root at {height}");
    assert_eq!(wallet.orchard, chain.orchard, "Orchard root at {height}");
    Ok(())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_applies_every_new_migration() -> Result<(), TestError> {
    let (db_path, _temp) = copy_fixture()?;
    let before = applied_migration_ids(&db_path)?;
    assert_eq!(i64::try_from(before.len()), Ok(RC4_MIGRATION_COUNT));
    assert!(!before.contains(&FIX_BAD_IRONWOOD_CHANGE_FLAGGING));

    let _opened = open_copied_fixture(db_path.clone()).await?;

    let after = applied_migration_ids(&db_path)?;
    assert_eq!(i64::try_from(after.len()), Ok(CURRENT_MIGRATION_COUNT));
    assert!(before.iter().all(|id| after.contains(id)));
    assert!(after.contains(&FIX_BAD_IRONWOOD_CHANGE_FLAGGING));
    Ok(())
}

/// An rc.4 Ironwood change note flagged `is_change = 0` reports as change after the upgrade.
///
/// `fix_bad_ironwood_change_flagging` flags an internally-scoped Ironwood note as change when
/// its account funded the transaction. An externally-scoped note in the same transaction
/// stays a receive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_ironwood_change_is_reported_as_change_after_upgrade() -> Result<(), TestError> {
    let (db_path, _temp) = copy_fixture()?;
    let tx_id = TxId::from_bytes([0x1c; 32]);
    {
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute(
            "INSERT INTO transactions (id_tx, txid, block, mined_height, tx_index, \
                 min_observed_height) \
             VALUES (2, ?1, 4009945, 4009945, 1, 4009945)",
            [tx_id.as_bytes().to_vec()],
        )?;
        for (action_index, key_scope) in [(0_i64, KEY_SCOPE_INTERNAL), (1, KEY_SCOPE_EXTERNAL)] {
            conn.execute(
                "INSERT INTO ironwood_received_notes (transaction_id, action_index, account_id, \
                     diversifier, value, rho, rseed, is_change, recipient_key_scope, \
                     note_version) \
                 VALUES (2, ?1, 1, zeroblob(11), 100000, zeroblob(32), zeroblob(32), 0, ?2, 2)",
                rusqlite::params![action_index, key_scope],
            )?;
        }
        conn.execute(
            "INSERT INTO sent_notes (transaction_id, output_pool, output_index, \
                 from_account_id, to_address, value) \
             VALUES (2, ?1, 2, 1, 'utest-placeholder', 50000)",
            [IRONWOOD_POOL_CODE],
        )?;
    }

    let (storage, account_id) = open_copied_fixture(db_path).await?;

    let mut receives: Vec<(u32, bool)> = storage
        .list_shielded_receives_for_account(account_id)
        .await?
        .into_iter()
        .filter(|row| row.tx_id == tx_id)
        .map(|row| {
            assert_eq!(row.protocol, ShieldedPool::Ironwood);
            (row.output_index, row.is_change)
        })
        .collect();
    receives.sort_unstable();
    assert_eq!(receives, vec![(0, true), (1, false)]);
    Ok(())
}

/// Listing exposed addresses skips standalone transparent rows instead of failing.
///
/// `zcash_client_sqlite` stores imported standalone transparent addresses without a
/// diversifier index and marks them exposed once they receive funds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exposed_addresses_skip_standalone_transparent_rows() -> Result<(), TestError> {
    let (db_path, _temp) = copy_fixture()?;
    let (storage, account_id) = open_copied_fixture(db_path.clone()).await?;
    let standalone = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([0x11; 20]),
    )
    .encode(&Network::Testnet.to_parameters());
    {
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute(
            "INSERT INTO addresses (account_id, key_scope, diversifier_index_be, address, \
                 cached_transparent_receiver_address, exposed_at_height, receiver_flags) \
             VALUES (1, -1, NULL, ?1, ?1, 4009945, 1)",
            [&standalone],
        )?;
    }

    let listed = storage.list_exposed_addresses(account_id).await?;
    assert_eq!(listed.len(), FIXTURE_EXPOSED_ADDRESSES.len());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc4_wallet_keeps_scanning_after_upgrade() -> Result<(), TestError> {
    let (storage, _account_id, _temp) = open_fixture_copy().await?;
    scan(
        &storage,
        testnet_compact_blocks(
            u64::from(FIXTURE_SCANNED_HEIGHT) + 1,
            u64::from(FIXTURE_RANGE_END),
        )?,
        FIXTURE_SCANNED_HEIGHT,
    )
    .await?;
    assert_eq!(
        storage.fully_scanned_height().await?,
        Some(BlockHeight::from(FIXTURE_RANGE_END))
    );
    assert_roots_match_chain(&storage, FIXTURE_RANGE_END).await
}

/// Reorders the last two Orchard actions of the last block that has a transaction with two,
/// so the block's final note commitment, which the tree retains at its checkpoint, changes.
fn reorder_last_orchard_actions(
    mut blocks: Vec<CompactBlockArtifact>,
) -> Result<Vec<CompactBlockArtifact>, TestError> {
    let actions = blocks
        .iter_mut()
        .rev()
        .find_map(|block| {
            block
                .transactions
                .iter_mut()
                .rev()
                .find(|transaction| transaction.orchard_actions.len() >= 2)
                .map(|transaction| &mut transaction.orchard_actions)
        })
        .ok_or(TestError::NoReorderableBlock)?;
    let last = actions.len() - 1;
    actions.swap(last - 1, last);
    Ok(blocks)
}

/// Scans a divergent copy of 4009950-4009999 over the upgraded wallet, so the canonical
/// blocks then hit a shardtree `Insert(Conflict)` at the retained checkpoint leaf.
async fn wallet_with_divergent_tree() -> Result<(Sqlite, TempDir), TestError> {
    let (storage, _account_id, temp) = open_fixture_copy().await?;
    let canonical = testnet_compact_blocks(
        u64::from(FIXTURE_SCANNED_HEIGHT) + 1,
        u64::from(FIXTURE_RANGE_END),
    )?;
    scan(
        &storage,
        reorder_last_orchard_actions(canonical.clone())?,
        FIXTURE_SCANNED_HEIGHT,
    )
    .await?;

    let conflict = scan(&storage, canonical, FIXTURE_SCANNED_HEIGHT).await;
    assert!(
        matches!(
            conflict,
            Err(TestError::Storage(
                StorageError::CommitmentTreeConflict { .. }
            ))
        ),
        "a canonical rescan over a divergent tree must surface a commitment-tree conflict: \
         {conflict:?}"
    );
    Ok((storage, temp))
}

/// The sync repair ladder's first cure for a commitment-tree conflict is a rewind below the
/// divergence, then a rescan from the chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commitment_tree_conflict_heals_by_rewind_and_rescan() -> Result<(), TestError> {
    let (storage, _temp) = wallet_with_divergent_tree().await?;

    storage
        .truncate_to_chain_state(testnet_chain_state_at(FIXTURE_SCANNED_HEIGHT)?)
        .await?;
    scan(
        &storage,
        testnet_compact_blocks(
            u64::from(FIXTURE_SCANNED_HEIGHT) + 1,
            u64::from(FIXTURE_RANGE_END),
        )?,
        FIXTURE_SCANNED_HEIGHT,
    )
    .await?;
    assert_roots_match_chain(&storage, FIXTURE_RANGE_END).await
}

/// The ladder's last cure is a rebuild from the account birthday, which keeps the account's
/// identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commitment_tree_conflict_heals_by_rebuild_from_birthday() -> Result<(), TestError> {
    let (storage, _temp) = wallet_with_divergent_tree().await?;
    let birthday_anchor = 4_009_899;

    let account_id = storage
        .recreate_with_account(&fixture_seed()?, testnet_chain_state_at(birthday_anchor)?)
        .await?;
    assert_eq!(account_id.as_uuid().to_string(), FIXTURE_ACCOUNT_ID);
    storage
        .update_chain_tip(BlockHeight::from(FIXTURE_RANGE_END))
        .await?;
    scan(
        &storage,
        testnet_compact_blocks(u64::from(birthday_anchor) + 1, u64::from(FIXTURE_RANGE_END))?,
        birthday_anchor,
    )
    .await?;
    assert_roots_match_chain(&storage, FIXTURE_RANGE_END).await
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedMaterialError),
    #[error("fixture error: {0}")]
    FixtureFile(#[from] FixtureError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the fixture seed has no account in the upgraded wallet")]
    AccountMissing,
    #[error("no fixture block has two Orchard actions to reorder")]
    NoReorderableBlock,
    #[error("fixture constant is out of range")]
    Fixture,
}
