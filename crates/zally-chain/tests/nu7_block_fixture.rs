//! The chain-plane transaction parsers accept a transaction mined under NU7.
//!
//! Fixture (`tests/fixtures/nu7_coinbase_block.hex`): block 3 of a Zakura 1.6.0 Regtest chain
//! configured with NU7 active from height 3. It holds a single version 6 coinbase transaction
//! committing to the NU7 consensus branch id `0x77190AD9`.

use zally_chain::{parse_transaction_expiry_height, parse_transaction_id};
use zally_core::BlockHeight;
use zcash_primitives::block::BlockHeader;
use zcash_primitives::transaction::{Transaction, TxVersion};
use zcash_protocol::consensus::BranchId;

const NU7_BLOCK_HEX: &str = include_str!("fixtures/nu7_coinbase_block.hex");
const NU7_BRANCH_ID: u32 = 0x7719_0AD9;

/// Splits the fixture block into its header and the bytes of its only transaction.
fn header_and_coinbase() -> Result<(BlockHeader, Vec<u8>), TestError> {
    let block_bytes = hex::decode(NU7_BLOCK_HEX.trim())?;
    let mut reader = block_bytes.as_slice();
    let header = BlockHeader::read(&mut reader)?;
    let (transaction_count, coinbase) = reader.split_first().ok_or(TestError::Truncated)?;
    assert_eq!(
        *transaction_count, 1,
        "the fixture block holds one transaction"
    );
    Ok((header, coinbase.to_vec()))
}

#[test]
fn nu7_coinbase_parses_as_a_version_6_nu7_transaction() -> Result<(), TestError> {
    let (_header, coinbase) = header_and_coinbase()?;
    let transaction = Transaction::read(coinbase.as_slice(), BranchId::Nu7)?;

    assert_eq!(transaction.version(), TxVersion::V6);
    assert_eq!(transaction.consensus_branch_id(), BranchId::Nu7);
    assert_eq!(u32::from(transaction.consensus_branch_id()), NU7_BRANCH_ID);
    Ok(())
}

#[test]
fn chain_plane_parsers_accept_the_nu7_coinbase() -> Result<(), TestError> {
    let (header, coinbase) = header_and_coinbase()?;
    let transaction = Transaction::read(coinbase.as_slice(), BranchId::Nu7)?;

    let tx_id = parse_transaction_id(&coinbase)?;
    assert_eq!(tx_id.as_bytes(), transaction.txid().as_ref());
    assert_eq!(
        tx_id.as_bytes(),
        &header.merkle_root,
        "a single-transaction block's merkle root is its coinbase txid"
    );
    assert_eq!(
        parse_transaction_expiry_height(&coinbase)?,
        BlockHeight::from(u32::from(transaction.expiry_height()))
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("fixture hex error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("fixture io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transaction parse error: {0}")]
    Parse(#[from] zally_chain::TransactionParseError),
    #[error("fixture block ends after its header")]
    Truncated,
}
