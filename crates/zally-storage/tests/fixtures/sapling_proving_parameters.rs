use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct SaplingProvingParameterPaths {
    pub(crate) spend: PathBuf,
    pub(crate) output: PathBuf,
}

pub(crate) fn create_sapling_proving_parameters(
    directory: &Path,
) -> Result<SaplingProvingParameterPaths, std::io::Error> {
    let (spend_bytes, output_bytes) = wagyu_zcash_parameters::load_sapling_parameters();
    let spend = directory.join(zcash_proofs::SAPLING_SPEND_NAME);
    let output = directory.join(zcash_proofs::SAPLING_OUTPUT_NAME);
    fs::write(&spend, spend_bytes)?;
    fs::write(&output, output_bytes)?;
    Ok(SaplingProvingParameterPaths { spend, output })
}
