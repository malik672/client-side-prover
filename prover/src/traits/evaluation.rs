//! This module defines a collection of traits that define the behavior of a
//! polynomial evaluation engine A vector of size N is treated as a multilinear
//! polynomial in \log{N} variables, and a commitment provided by the commitment
//! engine is treated as a multilinear polynomial commitment
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
  errors::NovaError,
  traits::{commitment::CommitmentEngineTrait, Engine},
};

/// A trait that ties different pieces of the commitment evaluation together
pub trait EvaluationEngineTrait<E: Engine>: Clone + Send + Sync {
  /// A type that holds the prover key
  type ProverKey: Send + Sync + Serialize + for<'de> Deserialize<'de>;

  /// A type that holds the verifier key
  type VerifierKey: Send
    + Sync
    // required for easy Digest computation purposes, could be relaxed to
    // [`crate::digest::Digestible`]
    + Serialize
    + for<'de> Deserialize<'de>;

  /// A type that holds the evaluation argument
  type EvaluationArgument: Clone + Send + Sync + Serialize + for<'de> Deserialize<'de>;

  /// A method to perform any additional setup needed to produce proofs of
  /// evaluations
  ///
  /// **Note:** This method should be cheap and should not copy most of the
  /// commitment key. Look at `CommitmentEngineTrait::setup` for generating
  /// SRS data.
  fn setup(
    ck: Arc<<<E as Engine>::CE as CommitmentEngineTrait<E>>::CommitmentKey>,
  ) -> (Self::ProverKey, Self::VerifierKey);

  /// A method to prove the evaluation of a multilinear polynomial
  fn prove(
    ck: &<<E as Engine>::CE as CommitmentEngineTrait<E>>::CommitmentKey,
    pk: &Self::ProverKey,
    transcript: &mut E::TE,
    comm: &<<E as Engine>::CE as CommitmentEngineTrait<E>>::Commitment,
    poly: &[E::Scalar],
    point: &[E::Scalar],
    eval: &E::Scalar,
  ) -> Result<Self::EvaluationArgument, NovaError>;

  /// A method to verify the purported evaluation of a multilinear polynomials
  fn verify(
    vk: &Self::VerifierKey,
    transcript: &mut E::TE,
    comm: &<<E as Engine>::CE as CommitmentEngineTrait<E>>::Commitment,
    point: &[E::Scalar],
    eval: &E::Scalar,
    arg: &Self::EvaluationArgument,
  ) -> Result<(), NovaError>;
}
