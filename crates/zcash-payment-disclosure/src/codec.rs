use std::fmt;

use blake2b_simd::Params;
use zcash_protocol::{
    TxId,
    consensus::{NetworkConstants, NetworkType},
};

const DRAFT1_PROFILE_BYTE: u8 = 0x01;
const ZALLY_IRONWOOD_PROFILE_BYTE: u8 = 0x02;
const MAX_MESSAGE_BYTES: usize = 65_535;
const MAX_DISCLOSURE_ENTRIES: u64 = 4_096;
const SIGNED_PERSONALIZATION_PREFIX: &[u8; 12] = b"ZIP311Signed";

/// An immutable interpretation of the ZIP-311 draft and its wire encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PaymentDisclosureProfile {
    /// The first incubating profile, supporting Sapling spends and outputs.
    Zip311Draft1,
    /// Zally's incubating Ironwood extension, pending upstream specification.
    ZallyIronwood,
}

impl PaymentDisclosureProfile {
    const fn profile_byte(self) -> u8 {
        match self {
            Self::Zip311Draft1 => DRAFT1_PROFILE_BYTE,
            Self::ZallyIronwood => ZALLY_IRONWOOD_PROFILE_BYTE,
        }
    }
}

/// A Sapling spend-authority proof included in a payment disclosure.
#[derive(Clone, Eq, PartialEq)]
pub struct SaplingSpendDisclosure {
    index: u32,
    cv: [u8; 32],
    rk: [u8; 32],
    zkproof: [u8; 192],
    spend_auth_sig: [u8; 64],
}

impl fmt::Debug for SaplingSpendDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaplingSpendDisclosure")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl SaplingSpendDisclosure {
    /// Constructs a Sapling spend disclosure from its canonical fields.
    #[must_use]
    pub const fn new(
        index: u32,
        cv: [u8; 32],
        rk: [u8; 32],
        zkproof: [u8; 192],
        spend_auth_sig: [u8; 64],
    ) -> Self {
        Self {
            index,
            cv,
            rk,
            zkproof,
            spend_auth_sig,
        }
    }

    /// Returns the Sapling spend index in the disclosed transaction.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn cv_bytes(&self) -> &[u8; 32] {
        &self.cv
    }

    pub(crate) const fn rk_bytes(&self) -> [u8; 32] {
        self.rk
    }

    pub(crate) const fn zkproof_bytes(&self) -> &[u8; 192] {
        &self.zkproof
    }

    pub(crate) const fn spend_auth_sig_bytes(&self) -> [u8; 64] {
        self.spend_auth_sig
    }
}

/// A Sapling output disclosed using its outgoing cipher key.
#[derive(Clone, Eq, PartialEq)]
pub struct SaplingOutputDisclosure {
    index: u32,
    ock: [u8; 32],
}

impl fmt::Debug for SaplingOutputDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaplingOutputDisclosure")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl SaplingOutputDisclosure {
    /// Constructs a Sapling output disclosure.
    #[must_use]
    pub const fn new(index: u32, ock: [u8; 32]) -> Self {
        Self { index, ock }
    }

    /// Returns the Sapling output index in the disclosed transaction.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn ock_bytes(&self) -> [u8; 32] {
        self.ock
    }
}

/// A message-bound authorization signature for one mined Ironwood spend action.
#[derive(Clone, Eq, PartialEq)]
pub struct IronwoodSpendDisclosure {
    index: u32,
    spend_auth_sig: [u8; 64],
}

impl fmt::Debug for IronwoodSpendDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IronwoodSpendDisclosure")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl IronwoodSpendDisclosure {
    /// Constructs an Ironwood spend disclosure.
    #[must_use]
    pub const fn new(index: u32, spend_auth_sig: [u8; 64]) -> Self {
        Self {
            index,
            spend_auth_sig,
        }
    }

    /// Returns the Ironwood action index in the disclosed transaction.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn spend_auth_sig_bytes(&self) -> [u8; 64] {
        self.spend_auth_sig
    }
}

/// An Ironwood output disclosed using its outgoing cipher key.
#[derive(Clone, Eq, PartialEq)]
pub struct IronwoodOutputDisclosure {
    index: u32,
    ock: [u8; 32],
}

impl fmt::Debug for IronwoodOutputDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IronwoodOutputDisclosure")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl IronwoodOutputDisclosure {
    /// Constructs an Ironwood output disclosure.
    #[must_use]
    pub const fn new(index: u32, ock: [u8; 32]) -> Self {
        Self { index, ock }
    }

    /// Returns the Ironwood action index containing the selected output.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn ock_bytes(&self) -> [u8; 32] {
        self.ock
    }
}

/// A parsed, canonical payment disclosure.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentDisclosure {
    profile: PaymentDisclosureProfile,
    transaction_id: TxId,
    message: Vec<u8>,
    sapling_spends: Vec<SaplingSpendDisclosure>,
    sapling_outputs: Vec<SaplingOutputDisclosure>,
    ironwood_spends: Vec<IronwoodSpendDisclosure>,
    ironwood_outputs: Vec<IronwoodOutputDisclosure>,
}

impl fmt::Debug for PaymentDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentDisclosure")
            .field("profile", &self.profile)
            .field("transaction_id", &self.transaction_id)
            .field("message_bytes", &self.message.len())
            .field("sapling_spends", &self.sapling_spends.len())
            .field("sapling_outputs", &self.sapling_outputs.len())
            .field("ironwood_spends", &self.ironwood_spends.len())
            .field("ironwood_outputs", &self.ironwood_outputs.len())
            .finish()
    }
}

impl PaymentDisclosure {
    /// Constructs a canonical payment disclosure.
    ///
    /// # Errors
    ///
    /// Returns an error when the message is too large, the disclosure proves no
    /// spend authority, or an index sequence is not strictly increasing.
    pub fn new(
        profile: PaymentDisclosureProfile,
        transaction_id: TxId,
        message: Vec<u8>,
        sapling_spends: Vec<SaplingSpendDisclosure>,
        sapling_outputs: Vec<SaplingOutputDisclosure>,
    ) -> Result<Self, PaymentDisclosureCodecError> {
        if profile != PaymentDisclosureProfile::Zip311Draft1 {
            return Err(PaymentDisclosureCodecError::ProfileShapeMismatch);
        }
        validate_disclosure_shape(
            message.len(),
            sapling_spends.len(),
            sapling_spends.iter().map(SaplingSpendDisclosure::index),
            sapling_outputs.len(),
            sapling_outputs.iter().map(SaplingOutputDisclosure::index),
        )?;

        Ok(Self {
            profile,
            transaction_id,
            message,
            sapling_spends,
            sapling_outputs,
            ironwood_spends: Vec::new(),
            ironwood_outputs: Vec::new(),
        })
    }

    /// Constructs a canonical Zally Ironwood extension disclosure.
    ///
    /// # Errors
    ///
    /// Returns an error when the message is too large, no spend authority is disclosed, or
    /// an index sequence is not strictly increasing.
    pub fn ironwood_extension(
        transaction_id: TxId,
        message: Vec<u8>,
        ironwood_spends: Vec<IronwoodSpendDisclosure>,
        ironwood_outputs: Vec<IronwoodOutputDisclosure>,
    ) -> Result<Self, PaymentDisclosureCodecError> {
        validate_ironwood_disclosure_shape(
            message.len(),
            ironwood_spends.len(),
            ironwood_spends.iter().map(IronwoodSpendDisclosure::index),
            ironwood_outputs.len(),
            ironwood_outputs.iter().map(IronwoodOutputDisclosure::index),
        )?;
        Ok(Self {
            profile: PaymentDisclosureProfile::ZallyIronwood,
            transaction_id,
            message,
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
            ironwood_spends,
            ironwood_outputs,
        })
    }

    /// Parses canonical signed disclosure bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, unsupported, or non-canonical bytes.
    pub fn from_bytes(disclosure_bytes: &[u8]) -> Result<Self, PaymentDisclosureCodecError> {
        let mut cursor = DisclosureCursor::new(disclosure_bytes);
        let profile_byte = cursor.read_u8()?;
        let profile = match profile_byte {
            DRAFT1_PROFILE_BYTE => PaymentDisclosureProfile::Zip311Draft1,
            ZALLY_IRONWOOD_PROFILE_BYTE => PaymentDisclosureProfile::ZallyIronwood,
            _ => {
                return Err(PaymentDisclosureCodecError::ProfileUnsupported { profile_byte });
            }
        };

        let mut transaction_id_bytes = cursor.read_array::<32>()?;
        transaction_id_bytes.reverse();
        let transaction_id = TxId::from_bytes(transaction_id_bytes);
        let message_bytes = cursor.read_compact_size(MAX_MESSAGE_BYTES as u64)?;
        let message = cursor.read_vec(usize_from_u64(message_bytes))?;

        let disclosure = match profile {
            PaymentDisclosureProfile::Zip311Draft1 => {
                parse_draft1_body(&mut cursor, transaction_id, message)?
            }
            PaymentDisclosureProfile::ZallyIronwood => {
                parse_ironwood_body(&mut cursor, transaction_id, message)?
            }
        };
        if cursor.remaining() != 0 {
            return Err(PaymentDisclosureCodecError::TrailingBytes {
                trailing_bytes: cursor.remaining(),
            });
        }
        Ok(disclosure)
    }

    /// Encodes this disclosure in canonical signed form.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut disclosure_bytes = self.header_bytes();
        match self.profile {
            PaymentDisclosureProfile::Zip311Draft1 => {
                self.write_draft1_body(&mut disclosure_bytes, true);
            }
            PaymentDisclosureProfile::ZallyIronwood => {
                self.write_ironwood_body(&mut disclosure_bytes, true);
            }
        }
        disclosure_bytes
    }

    /// Computes the ZIP-311 signed-disclosure digest for a network.
    #[must_use]
    pub fn compute_digest(&self, network: NetworkType) -> [u8; 32] {
        let mut personalization = [0; 16];
        personalization[..SIGNED_PERSONALIZATION_PREFIX.len()]
            .copy_from_slice(SIGNED_PERSONALIZATION_PREFIX);
        personalization[SIGNED_PERSONALIZATION_PREFIX.len()..]
            .copy_from_slice(&network.coin_type().to_le_bytes());
        let digest = Params::new()
            .hash_length(32)
            .personal(&personalization)
            .hash(&self.unsigned_bytes());
        let mut digest_bytes = [0; 32];
        digest_bytes.copy_from_slice(digest.as_bytes());
        digest_bytes
    }

    /// Returns the immutable disclosure profile.
    #[must_use]
    pub const fn profile(&self) -> PaymentDisclosureProfile {
        self.profile
    }

    /// Returns the transaction identifier committed to by this disclosure.
    #[must_use]
    pub const fn transaction_id(&self) -> TxId {
        self.transaction_id
    }

    /// Returns the verifier-supplied message bound by the spend authorization signatures.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    pub(crate) fn sapling_spends(&self) -> &[SaplingSpendDisclosure] {
        &self.sapling_spends
    }

    pub(crate) fn sapling_outputs(&self) -> &[SaplingOutputDisclosure] {
        &self.sapling_outputs
    }

    pub(crate) fn ironwood_spends(&self) -> &[IronwoodSpendDisclosure] {
        &self.ironwood_spends
    }

    pub(crate) fn ironwood_outputs(&self) -> &[IronwoodOutputDisclosure] {
        &self.ironwood_outputs
    }

    fn unsigned_bytes(&self) -> Vec<u8> {
        let mut disclosure_bytes = self.header_bytes();
        match self.profile {
            PaymentDisclosureProfile::Zip311Draft1 => {
                self.write_draft1_body(&mut disclosure_bytes, false);
            }
            PaymentDisclosureProfile::ZallyIronwood => {
                self.write_ironwood_body(&mut disclosure_bytes, false);
            }
        }
        disclosure_bytes
    }

    fn header_bytes(&self) -> Vec<u8> {
        let mut disclosure_bytes = Vec::with_capacity(40 + self.message.len());
        disclosure_bytes.push(self.profile.profile_byte());
        let mut transaction_id_bytes = *self.transaction_id.as_ref();
        transaction_id_bytes.reverse();
        disclosure_bytes.extend_from_slice(&transaction_id_bytes);
        write_compact_size(&mut disclosure_bytes, self.message.len() as u64);
        disclosure_bytes.extend_from_slice(&self.message);
        disclosure_bytes
    }

    fn write_draft1_body(&self, disclosure_bytes: &mut Vec<u8>, has_signatures: bool) {
        write_compact_size(disclosure_bytes, 0);
        write_compact_size(disclosure_bytes, self.sapling_spends.len() as u64);
        for spend in &self.sapling_spends {
            write_compact_size(disclosure_bytes, u64::from(spend.index));
            disclosure_bytes.extend_from_slice(&spend.cv);
            disclosure_bytes.extend_from_slice(&spend.rk);
            disclosure_bytes.extend_from_slice(&spend.zkproof);
            disclosure_bytes.push(0);
            if has_signatures {
                disclosure_bytes.extend_from_slice(&spend.spend_auth_sig);
            }
        }

        write_compact_size(disclosure_bytes, self.sapling_outputs.len() as u64);
        for output in &self.sapling_outputs {
            write_compact_size(disclosure_bytes, u64::from(output.index));
            disclosure_bytes.extend_from_slice(&output.ock);
        }
    }

    fn write_ironwood_body(&self, disclosure_bytes: &mut Vec<u8>, has_signatures: bool) {
        write_compact_size(disclosure_bytes, self.ironwood_spends.len() as u64);
        for spend in &self.ironwood_spends {
            write_compact_size(disclosure_bytes, u64::from(spend.index));
            if has_signatures {
                disclosure_bytes.extend_from_slice(&spend.spend_auth_sig);
            }
        }
        write_compact_size(disclosure_bytes, self.ironwood_outputs.len() as u64);
        for output in &self.ironwood_outputs {
            write_compact_size(disclosure_bytes, u64::from(output.index));
            disclosure_bytes.extend_from_slice(&output.ock);
        }
    }
}

fn parse_draft1_body(
    cursor: &mut DisclosureCursor<'_>,
    transaction_id: TxId,
    message: Vec<u8>,
) -> Result<PaymentDisclosure, PaymentDisclosureCodecError> {
    let transparent_input_count = cursor.read_compact_size(MAX_DISCLOSURE_ENTRIES)?;
    if transparent_input_count != 0 {
        return Err(PaymentDisclosureCodecError::TransparentInputsUnsupported);
    }
    let spend_count = cursor.read_compact_size(MAX_DISCLOSURE_ENTRIES)?;
    let mut sapling_spends = Vec::with_capacity(usize_from_u64(spend_count));
    for _ in 0..spend_count {
        let index = cursor.read_index()?;
        let cv = cursor.read_array::<32>()?;
        let rk = cursor.read_array::<32>()?;
        let zkproof = cursor.read_array::<192>()?;
        if cursor.read_u8()? != 0 {
            return Err(PaymentDisclosureCodecError::AddressProofUnsupported);
        }
        sapling_spends.push(SaplingSpendDisclosure::new(
            index,
            cv,
            rk,
            zkproof,
            cursor.read_array::<64>()?,
        ));
    }
    let output_count = cursor.read_compact_size(MAX_DISCLOSURE_ENTRIES)?;
    let mut sapling_outputs = Vec::with_capacity(usize_from_u64(output_count));
    for _ in 0..output_count {
        sapling_outputs.push(SaplingOutputDisclosure::new(
            cursor.read_index()?,
            cursor.read_array::<32>()?,
        ));
    }
    PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        transaction_id,
        message,
        sapling_spends,
        sapling_outputs,
    )
}

fn parse_ironwood_body(
    cursor: &mut DisclosureCursor<'_>,
    transaction_id: TxId,
    message: Vec<u8>,
) -> Result<PaymentDisclosure, PaymentDisclosureCodecError> {
    let spend_count = cursor.read_compact_size(MAX_DISCLOSURE_ENTRIES)?;
    let mut ironwood_spends = Vec::with_capacity(usize_from_u64(spend_count));
    for _ in 0..spend_count {
        ironwood_spends.push(IronwoodSpendDisclosure::new(
            cursor.read_index()?,
            cursor.read_array::<64>()?,
        ));
    }
    let output_count = cursor.read_compact_size(MAX_DISCLOSURE_ENTRIES)?;
    let mut ironwood_outputs = Vec::with_capacity(usize_from_u64(output_count));
    for _ in 0..output_count {
        ironwood_outputs.push(IronwoodOutputDisclosure::new(
            cursor.read_index()?,
            cursor.read_array::<32>()?,
        ));
    }
    PaymentDisclosure::ironwood_extension(
        transaction_id,
        message,
        ironwood_spends,
        ironwood_outputs,
    )
}

pub(crate) fn validate_disclosure_shape(
    message_bytes: usize,
    spend_count: usize,
    spend_indices: impl IntoIterator<Item = u32>,
    output_count: usize,
    output_indices: impl IntoIterator<Item = u32>,
) -> Result<(), PaymentDisclosureCodecError> {
    validate_indexed_disclosure_shape(
        message_bytes,
        spend_count,
        spend_indices,
        output_count,
        output_indices,
        "sapling_spend",
        "sapling_output",
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the two indexed disclosure sequences each require a count, indices, and field name"
)]
fn validate_indexed_disclosure_shape(
    message_bytes: usize,
    spend_count: usize,
    spend_indices: impl IntoIterator<Item = u32>,
    output_count: usize,
    output_indices: impl IntoIterator<Item = u32>,
    spend_field: &'static str,
    output_field: &'static str,
) -> Result<(), PaymentDisclosureCodecError> {
    if message_bytes > MAX_MESSAGE_BYTES {
        return Err(PaymentDisclosureCodecError::MessageTooLong {
            message_bytes,
            max_message_bytes: MAX_MESSAGE_BYTES,
        });
    }
    for entry_count in [spend_count, output_count] {
        if entry_count > usize_from_u64(MAX_DISCLOSURE_ENTRIES) {
            return Err(PaymentDisclosureCodecError::SizeOutOfRange {
                size: u64::try_from(entry_count).unwrap_or(u64::MAX),
                bound: MAX_DISCLOSURE_ENTRIES,
            });
        }
    }
    let mut spend_indices = spend_indices.into_iter().peekable();
    if spend_indices.peek().is_none() {
        return Err(PaymentDisclosureCodecError::NoProvenInput);
    }
    require_strictly_increasing(spend_indices, spend_field)?;
    require_strictly_increasing(output_indices, output_field)
}

pub(crate) fn validate_ironwood_disclosure_shape(
    message_bytes: usize,
    spend_count: usize,
    spend_indices: impl IntoIterator<Item = u32>,
    output_count: usize,
    output_indices: impl IntoIterator<Item = u32>,
) -> Result<(), PaymentDisclosureCodecError> {
    validate_indexed_disclosure_shape(
        message_bytes,
        spend_count,
        spend_indices,
        output_count,
        output_indices,
        "ironwood_spend",
        "ironwood_output",
    )
}

/// Failure to construct or decode a payment disclosure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentDisclosureCodecError {
    /// The profile byte is not supported by this build. Retry posture: `not_retryable`.
    #[error("payment disclosure profile byte {profile_byte:#04x} is unsupported")]
    ProfileUnsupported {
        /// Unrecognized profile byte.
        profile_byte: u8,
    },
    /// A profile was paired with fields from another profile. Retry posture: `not_retryable`.
    #[error("payment disclosure profile does not match its field shape")]
    ProfileShapeMismatch,
    /// The message exceeds the profile limit. Retry posture: `not_retryable`.
    #[error("payment disclosure message is {message_bytes} bytes; maximum is {max_message_bytes}")]
    MessageTooLong {
        /// Actual message length.
        message_bytes: usize,
        /// Maximum accepted message length.
        max_message_bytes: usize,
    },
    /// The disclosure proves no input authority. Retry posture: `not_retryable`.
    #[error("payment disclosure must prove at least one input")]
    NoProvenInput,
    /// Draft1 production does not support transparent inputs. Retry posture: `not_retryable`.
    #[error("transparent input disclosures are unsupported by this profile implementation")]
    TransparentInputsUnsupported,
    /// Draft1 production does not yet support ZIP-304 address proofs. Retry posture: `not_retryable`.
    #[error("ZIP-304 address proofs are unsupported by this profile implementation")]
    AddressProofUnsupported,
    /// A disclosure index sequence is not canonical. Retry posture: `not_retryable`.
    #[error("{field} index {index} is not strictly greater than its predecessor")]
    IndexNotIncreasing {
        /// Field containing the invalid sequence.
        field: &'static str,
        /// First invalid index.
        index: u32,
    },
    /// The byte stream ended before a field was complete. Retry posture: `not_retryable`.
    #[error("payment disclosure is truncated at byte {offset}")]
    Truncated {
        /// Offset of the incomplete field.
        offset: usize,
    },
    /// A `CompactSize` integer is non-minimal. Retry posture: `not_retryable`.
    #[error("payment disclosure contains non-minimal CompactSize at byte {offset}")]
    CompactSizeNonMinimal {
        /// Offset of the `CompactSize` prefix.
        offset: usize,
    },
    /// A decoded size exceeds its profile bound. Retry posture: `not_retryable`.
    #[error("payment disclosure size {size} exceeds bound {bound}")]
    SizeOutOfRange {
        /// Decoded size.
        size: u64,
        /// Maximum accepted size.
        bound: u64,
    },
    /// Extra bytes follow the canonical disclosure. Retry posture: `not_retryable`.
    #[error("payment disclosure has {trailing_bytes} trailing bytes")]
    TrailingBytes {
        /// Count of unconsumed bytes.
        trailing_bytes: usize,
    },
}

fn require_strictly_increasing(
    indices: impl IntoIterator<Item = u32>,
    field: &'static str,
) -> Result<(), PaymentDisclosureCodecError> {
    let mut previous = None;
    for index in indices {
        if previous.is_some_and(|prior| index <= prior) {
            return Err(PaymentDisclosureCodecError::IndexNotIncreasing { field, index });
        }
        previous = Some(index);
    }
    Ok(())
}

fn write_compact_size(disclosure_bytes: &mut Vec<u8>, size: u64) {
    if size < 0xfd {
        disclosure_bytes.push(u8::try_from(size).unwrap_or(0));
    } else if let Ok(short_size) = u16::try_from(size) {
        disclosure_bytes.push(0xfd);
        disclosure_bytes.extend_from_slice(&short_size.to_le_bytes());
    } else if let Ok(word_size) = u32::try_from(size) {
        disclosure_bytes.push(0xfe);
        disclosure_bytes.extend_from_slice(&word_size.to_le_bytes());
    } else {
        disclosure_bytes.push(0xff);
        disclosure_bytes.extend_from_slice(&size.to_le_bytes());
    }
}

fn usize_from_u64(size: u64) -> usize {
    usize::try_from(size).unwrap_or(0)
}

struct DisclosureCursor<'a> {
    disclosure_bytes: &'a [u8],
    offset: usize,
}

impl<'a> DisclosureCursor<'a> {
    const fn new(disclosure_bytes: &'a [u8]) -> Self {
        Self {
            disclosure_bytes,
            offset: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.disclosure_bytes.len().saturating_sub(self.offset)
    }

    fn read_u8(&mut self) -> Result<u8, PaymentDisclosureCodecError> {
        let byte = self.disclosure_bytes.get(self.offset).copied().ok_or(
            PaymentDisclosureCodecError::Truncated {
                offset: self.offset,
            },
        )?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_array<const LENGTH: usize>(
        &mut self,
    ) -> Result<[u8; LENGTH], PaymentDisclosureCodecError> {
        let end = self.offset.saturating_add(LENGTH);
        let source = self.disclosure_bytes.get(self.offset..end).ok_or(
            PaymentDisclosureCodecError::Truncated {
                offset: self.offset,
            },
        )?;
        let mut bytes = [0; LENGTH];
        bytes.copy_from_slice(source);
        self.offset = end;
        Ok(bytes)
    }

    fn read_vec(&mut self, length: usize) -> Result<Vec<u8>, PaymentDisclosureCodecError> {
        let end = self.offset.saturating_add(length);
        let source = self.disclosure_bytes.get(self.offset..end).ok_or(
            PaymentDisclosureCodecError::Truncated {
                offset: self.offset,
            },
        )?;
        self.offset = end;
        Ok(source.to_vec())
    }

    fn read_index(&mut self) -> Result<u32, PaymentDisclosureCodecError> {
        let index = self.read_compact_size(u64::from(u32::MAX))?;
        u32::try_from(index).map_err(|_| PaymentDisclosureCodecError::SizeOutOfRange {
            size: index,
            bound: u64::from(u32::MAX),
        })
    }

    fn read_compact_size(&mut self, bound: u64) -> Result<u64, PaymentDisclosureCodecError> {
        let prefix_offset = self.offset;
        let prefix = self.read_u8()?;
        let size = match prefix {
            0xfd => {
                let size = u64::from(u16::from_le_bytes(self.read_array::<2>()?));
                if size < 0xfd {
                    return Err(PaymentDisclosureCodecError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                size
            }
            0xfe => {
                let size = u64::from(u32::from_le_bytes(self.read_array::<4>()?));
                if u16::try_from(size).is_ok() {
                    return Err(PaymentDisclosureCodecError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                size
            }
            0xff => {
                let size = u64::from_le_bytes(self.read_array::<8>()?);
                if u32::try_from(size).is_ok() {
                    return Err(PaymentDisclosureCodecError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                size
            }
            byte => u64::from(byte),
        };
        if size > bound {
            return Err(PaymentDisclosureCodecError::SizeOutOfRange { size, bound });
        }
        Ok(size)
    }
}
