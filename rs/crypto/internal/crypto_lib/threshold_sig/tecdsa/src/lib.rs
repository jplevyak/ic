use ic_types::crypto::AlgorithmId;
use ic_types::{NodeIndex, NumberOfNodes, Randomness};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ThresholdEcdsaError {
    CurveMismatch,
    InconsistentCiphertext,
    InconsistentCommitments,
    InsufficientDealings,
    InterpolationError,
    InvalidArguments(String),
    InvalidFieldElement,
    InvalidOpening,
    InvalidPoint,
    InvalidProof,
    InvalidRecipients,
    InvalidScalar,
    InvalidSecretShare,
    InvalidThreshold(usize, usize),
    SerializationError(String),
}

pub type ThresholdEcdsaResult<T> = std::result::Result<T, ThresholdEcdsaError>;

mod dealings;
mod fe;
mod group;
mod hash2curve;
mod mega;
mod poly;
mod seed;
mod transcript;
mod xmd;

pub use dealings::*;
pub use fe::*;
pub use group::*;
pub use mega::*;
pub use poly::*;
pub use seed::*;
pub use transcript::*;
pub use xmd::*;

/// Create MEGa encryption keypair
pub fn gen_keypair(
    curve_type: EccCurveType,
    seed: Randomness,
) -> Result<(MEGaPublicKey, MEGaPrivateKey), ThresholdEcdsaError> {
    use rand_core::SeedableRng;

    let mut rng = rand_chacha::ChaCha20Rng::from_seed(seed.get());
    let private_key = MEGaPrivateKey::generate(curve_type, &mut rng)?;

    let public_key = private_key.public_key()?;

    Ok((public_key, private_key))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IdkgCreateDealingInternalError {
    UnsupportedAlgorithm,
    InvalidRecipients,
    // Contains the requested threshold and the number of receivers
    InvalidThreshold(usize, usize),
    InvalidSecretShare,
    InternalError(String),
}

impl From<ThresholdEcdsaError> for IdkgCreateDealingInternalError {
    fn from(e: ThresholdEcdsaError) -> Self {
        match e {
            ThresholdEcdsaError::InvalidRecipients => Self::InvalidRecipients,
            ThresholdEcdsaError::InvalidSecretShare => Self::InvalidSecretShare,
            ThresholdEcdsaError::InvalidThreshold(t, r) => Self::InvalidThreshold(t, r),
            x => Self::InternalError(format!("{:?}", x)),
        }
    }
}

/// Create a dealing for threshold ECDSA
pub fn create_dealing(
    algorithm_id: ic_types::crypto::AlgorithmId,
    associated_data: &[u8],
    dealer_index: NodeIndex,
    threshold: NumberOfNodes,
    recipients: &[MEGaPublicKey],
    shares: &SecretShares,
    randomness: Randomness,
) -> Result<IDkgDealingInternal, IdkgCreateDealingInternalError> {
    let curve = match algorithm_id {
        AlgorithmId::ThresholdEcdsaSecp256k1 => Ok(EccCurveType::K256),
        _ => Err(IdkgCreateDealingInternalError::UnsupportedAlgorithm),
    }?;

    let seed = Seed::from_randomness(&randomness);

    IDkgDealingInternal::new(
        shares,
        curve,
        seed,
        threshold.get() as usize,
        recipients,
        dealer_index as usize,
        associated_data,
    )
    .map_err(|e| e.into())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IDkgCreateTranscriptInternalError {
    UnsupportedAlgorithm,
    InconsistentCommitments,
    InsufficientDealings,
    InternalError(String),
}

impl From<ThresholdEcdsaError> for IDkgCreateTranscriptInternalError {
    fn from(e: ThresholdEcdsaError) -> Self {
        match e {
            ThresholdEcdsaError::CurveMismatch => Self::InconsistentCommitments,
            ThresholdEcdsaError::InconsistentCommitments => Self::InconsistentCommitments,
            ThresholdEcdsaError::InsufficientDealings => Self::InsufficientDealings,
            x => Self::InternalError(format!("{:?}", x)),
        }
    }
}

/// Create a new IDkg transcript
pub fn create_transcript(
    algorithm_id: AlgorithmId,
    reconstruction_threshold: NumberOfNodes,
    verified_dealings: &BTreeMap<NodeIndex, IDkgDealingInternal>,
    operation_mode: &IDkgTranscriptOperationInternal,
) -> Result<IDkgTranscriptInternal, IDkgCreateTranscriptInternalError> {
    let curve = match algorithm_id {
        AlgorithmId::ThresholdEcdsaSecp256k1 => Ok(EccCurveType::K256),
        _ => Err(IDkgCreateTranscriptInternalError::UnsupportedAlgorithm),
    }?;

    IDkgTranscriptInternal::new(
        curve,
        reconstruction_threshold.get() as usize,
        verified_dealings,
        operation_mode,
    )
    .map_err(|e| e.into())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IDkgLoadTranscriptInternalError {
    UnsupportedAlgorithm,
    InconsistentCommitments,
    InsufficientDealings,
    InternalError(String),
}

impl From<ThresholdEcdsaError> for IDkgLoadTranscriptInternalError {
    fn from(e: ThresholdEcdsaError) -> Self {
        match e {
            ThresholdEcdsaError::CurveMismatch => Self::InconsistentCommitments,
            ThresholdEcdsaError::InconsistentCommitments => Self::InconsistentCommitments,
            ThresholdEcdsaError::InsufficientDealings => Self::InsufficientDealings,
            x => Self::InternalError(format!("{:?}", x)),
        }
    }
}

pub fn compute_secret_shares(
    verified_dealings: &BTreeMap<NodeIndex, IDkgDealingInternal>,
    transcript: &IDkgTranscriptInternal,
    context_data: &[u8],
    receiver_index: NodeIndex,
    secret_key: &MEGaPrivateKey,
    public_key: &MEGaPublicKey,
) -> Result<CommitmentOpening, IDkgLoadTranscriptInternalError> {
    CommitmentOpening::from_dealings(
        verified_dealings,
        transcript,
        context_data,
        receiver_index,
        secret_key,
        public_key,
    )
    .map_err(|e| e.into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IDkgVerifyDealingInternalError {
    UnsupportedAlgorithm,
    InvalidCommitment,
    InvalidProof,
    InvalidRecipients,
    InternalError(String),
}

impl From<ThresholdEcdsaError> for IDkgVerifyDealingInternalError {
    fn from(e: ThresholdEcdsaError) -> Self {
        match e {
            ThresholdEcdsaError::InvalidProof => Self::InvalidProof,
            ThresholdEcdsaError::InconsistentCommitments => Self::InvalidCommitment,
            ThresholdEcdsaError::InvalidRecipients => Self::InvalidRecipients,
            x => Self::InternalError(format!("{:?}", x)),
        }
    }
}

/// Verify a dealing using public information
///
/// Verify that the dealing has the expected type of ciphertext
/// and commitment (depending on the type of dealing)
///
/// When CRP-1158 is completed this will also verify the zero
/// knowledge proofs
pub fn publicly_verify_dealing(
    algorithm_id: AlgorithmId,
    dealing: &IDkgDealingInternal,
    transcript_type: &IDkgTranscriptOperationInternal,
    reconstruction_threshold: NumberOfNodes,
    dealer_index: NodeIndex,
    number_of_receivers: NumberOfNodes,
) -> Result<(), IDkgVerifyDealingInternalError> {
    let curve = match algorithm_id {
        AlgorithmId::ThresholdEcdsaSecp256k1 => Ok(EccCurveType::K256),
        _ => Err(IDkgVerifyDealingInternalError::UnsupportedAlgorithm),
    }?;

    dealing
        .publicly_verify(
            curve,
            transcript_type,
            reconstruction_threshold,
            dealer_index,
            number_of_receivers,
        )
        .map_err(|e| e.into())
}

/// Verify a dealing using private information
///
/// This private verification must be done after the dealing has been publically
/// verified. This operation decrypts the dealing and verifies that the
/// decrypted value is consistent with the commitment in the dealing.
#[allow(clippy::too_many_arguments)]
pub fn privately_verify_dealing(
    algorithm_id: AlgorithmId,
    dealing: &IDkgDealingInternal,
    private_key: &MEGaPrivateKey,
    public_key: &MEGaPublicKey,
    associated_data: &[u8],
    dealer_index: NodeIndex,
    recipient_index: NodeIndex,
) -> Result<(), IDkgVerifyDealingInternalError> {
    let curve = match algorithm_id {
        AlgorithmId::ThresholdEcdsaSecp256k1 => Ok(EccCurveType::K256),
        _ => Err(IDkgVerifyDealingInternalError::UnsupportedAlgorithm),
    }?;

    dealing
        .privately_verify(
            curve,
            private_key,
            public_key,
            associated_data,
            dealer_index,
            recipient_index,
        )
        .map_err(|e| e.into())
}
