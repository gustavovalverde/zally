//! Versioned Zcash payment disclosures.
//!
//! This crate incubates the cryptographic protocol independently of Zally's
//! wallet, storage, and key-management boundaries so it can move upstream
//! without changing its public interface.

mod codec;
mod produce;
mod verify;

pub use codec::{
    IronwoodOutputDisclosure, IronwoodSpendDisclosure, PaymentDisclosure,
    PaymentDisclosureCodecError, PaymentDisclosureProfile, SaplingOutputDisclosure,
    SaplingSpendDisclosure,
};
pub use produce::{
    IronwoodDisclosurePlan, IronwoodOutputSelection, IronwoodSpendSigningInput,
    PaymentDisclosurePlan, PaymentDisclosureProductionError, SaplingOutputSelection,
    SaplingSpendProvingInput, prove_disclosure, sign_ironwood_disclosure,
};
pub use verify::{
    IronwoodOutputEvidence, IronwoodSpendEvidence, PaymentDisclosureEvidence,
    PaymentDisclosureVerificationError, SaplingOutputEvidence, SaplingSpendEvidence,
    verify_disclosure,
};
