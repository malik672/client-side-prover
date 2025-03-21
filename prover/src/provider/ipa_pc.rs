//! This module implements `EvaluationEngine` using an IPA-based polynomial
//! commitment scheme
use core::iter;
use std::{marker::PhantomData, sync::Arc};

use ff::Field;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
  digest::SimpleDigestible,
  errors::{NovaError, PCSError},
  provider::{pedersen::CommitmentKeyExtTrait, traits::DlogGroup, util::field::batch_invert},
  spartan::polys::eq::EqPolynomial,
  traits::{
    commitment::{CommitmentEngineTrait, CommitmentTrait},
    evaluation::EvaluationEngineTrait,
    Engine, TranscriptEngineTrait, TranscriptReprTrait,
  },
  zip_with, Commitment, CommitmentKey, CompressedCommitment, CE,
};

/// Provides an implementation of the prover key
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct ProverKey<E: Engine> {
  pub ck_s: CommitmentKey<E>,
}

/// Provides an implementation of the verifier key
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct VerifierKey<E: Engine> {
  pub ck_v: Arc<CommitmentKey<E>>,
  pub ck_s: CommitmentKey<E>,
}

impl<E: Engine> SimpleDigestible for VerifierKey<E> {}

/// Provides an implementation of a polynomial evaluation engine using IPA
#[derive(Clone, Debug)]
pub struct EvaluationEngine<E> {
  _p: PhantomData<E>,
}

impl<E> EvaluationEngineTrait<E> for EvaluationEngine<E>
where
  E: Engine,
  E::GE: DlogGroup,
  CommitmentKey<E>: CommitmentKeyExtTrait<E>,
{
  type EvaluationArgument = InnerProductArgument<E>;
  type ProverKey = ProverKey<E>;
  type VerifierKey = VerifierKey<E>;

  fn setup(
    ck: Arc<<<E as Engine>::CE as CommitmentEngineTrait<E>>::CommitmentKey>,
  ) -> (Self::ProverKey, Self::VerifierKey) {
    let ck_c = E::CE::setup(b"ipa", 1);

    let pk = ProverKey { ck_s: ck_c.clone() };
    let vk = VerifierKey { ck_v: ck.clone(), ck_s: ck_c };

    (pk, vk)
  }

  fn prove(
    ck: &CommitmentKey<E>,
    pk: &Self::ProverKey,
    transcript: &mut E::TE,
    comm: &Commitment<E>,
    poly: &[E::Scalar],
    point: &[E::Scalar],
    eval: &E::Scalar,
  ) -> Result<Self::EvaluationArgument, NovaError> {
    let u = InnerProductInstance::new(comm, &EqPolynomial::evals_from_points(point), eval);
    let w = InnerProductWitness::new(poly);

    InnerProductArgument::prove(ck.clone(), pk.ck_s.clone(), &u, &w, transcript)
  }

  /// A method to verify purported evaluations of a batch of polynomials
  fn verify(
    vk: &Self::VerifierKey,
    transcript: &mut E::TE,
    comm: &Commitment<E>,
    point: &[E::Scalar],
    eval: &E::Scalar,
    arg: &Self::EvaluationArgument,
  ) -> Result<(), NovaError> {
    let u = InnerProductInstance::new(comm, &EqPolynomial::evals_from_points(point), eval);

    arg.verify(&vk.ck_v, vk.ck_s.clone(), 1 << point.len(), &u, transcript)?;

    Ok(())
  }
}

fn inner_product<T: Field + Send + Sync>(a: &[T], b: &[T]) -> T {
  zip_with!(par_iter, (a, b), |x, y| *x * y).sum()
}

/// An inner product instance consists of a commitment to a vector `a` and
/// another vector `b` and the claim that c = <a, b>.
struct InnerProductInstance<E: Engine> {
  comm_a_vec: Commitment<E>,
  b_vec:      Vec<E::Scalar>,
  c:          E::Scalar,
}

impl<E> InnerProductInstance<E>
where
  E: Engine,
  E::GE: DlogGroup,
{
  fn new(comm_a_vec: &Commitment<E>, b_vec: &[E::Scalar], c: &E::Scalar) -> Self {
    Self { comm_a_vec: *comm_a_vec, b_vec: b_vec.to_vec(), c: *c }
  }
}

impl<E: Engine> TranscriptReprTrait<E::GE> for InnerProductInstance<E> {
  fn to_transcript_bytes(&self) -> Vec<u8> {
    // we do not need to include self.b_vec as in our context it is produced from
    // the transcript
    [self.comm_a_vec.to_transcript_bytes(), self.c.to_transcript_bytes()].concat()
  }
}

struct InnerProductWitness<E: Engine> {
  a_vec: Vec<E::Scalar>,
}

impl<E: Engine> InnerProductWitness<E> {
  fn new(a_vec: &[E::Scalar]) -> Self { Self { a_vec: a_vec.to_vec() } }
}

/// An inner product argument
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct InnerProductArgument<E: Engine> {
  pub(in crate::provider) L_vec: Vec<CompressedCommitment<E>>,
  pub(in crate::provider) R_vec: Vec<CompressedCommitment<E>>,
  pub(in crate::provider) a_hat: E::Scalar,
}

impl<E> InnerProductArgument<E>
where
  E: Engine,
  E::GE: DlogGroup,
  CommitmentKey<E>: CommitmentKeyExtTrait<E>,
{
  const fn protocol_name() -> &'static [u8] { b"IPA" }

  fn prove(
    ck: CommitmentKey<E>,
    mut ck_c: CommitmentKey<E>,
    U: &InnerProductInstance<E>,
    W: &InnerProductWitness<E>,
    transcript: &mut E::TE,
  ) -> Result<Self, NovaError> {
    transcript.dom_sep(Self::protocol_name());

    let (ck, _) = ck.split_at(U.b_vec.len());

    if U.b_vec.len() != W.a_vec.len() {
      return Err(NovaError::InvalidInputLength);
    }

    // absorb the instance in the transcript
    transcript.absorb(b"U", U);

    // sample a random base for committing to the inner product
    let r = transcript.squeeze(b"r")?;
    ck_c.scale(&r);

    // a closure that executes a step of the recursive inner product argument
    let prove_inner = |a_vec: &[E::Scalar],
                       b_vec: &[E::Scalar],
                       ck: CommitmentKey<E>,
                       transcript: &mut E::TE|
     -> Result<
      (
        CompressedCommitment<E>,
        CompressedCommitment<E>,
        Vec<E::Scalar>,
        Vec<E::Scalar>,
        CommitmentKey<E>,
      ),
      NovaError,
    > {
      let n = a_vec.len();
      let (ck_L, ck_R) = ck.split_at(n / 2);

      let c_L = inner_product(&a_vec[0..n / 2], &b_vec[n / 2..n]);
      let c_R = inner_product(&a_vec[n / 2..n], &b_vec[0..n / 2]);

      let L = CE::<E>::commit(
        &ck_R.combine(&ck_c),
        &a_vec[0..n / 2].iter().chain(iter::once(&c_L)).copied().collect::<Vec<E::Scalar>>(),
      )
      .compress();
      let R = CE::<E>::commit(
        &ck_L.combine(&ck_c),
        &a_vec[n / 2..n].iter().chain(iter::once(&c_R)).copied().collect::<Vec<E::Scalar>>(),
      )
      .compress();

      transcript.absorb(b"L", &L);
      transcript.absorb(b"R", &R);

      let r = transcript.squeeze(b"r")?;
      let r_inverse = r.invert().unwrap();

      // fold the left half and the right half
      let a_vec_folded =
        zip_with!((a_vec[0..n / 2].par_iter(), a_vec[n / 2..n].par_iter()), |a_L, a_R| *a_L * r
          + r_inverse * *a_R)
        .collect::<Vec<E::Scalar>>();

      let b_vec_folded =
        zip_with!((b_vec[0..n / 2].par_iter(), b_vec[n / 2..n].par_iter()), |b_L, b_R| *b_L
          * r_inverse
          + r * *b_R)
        .collect::<Vec<E::Scalar>>();

      let ck_folded = CommitmentKeyExtTrait::fold(&ck_L, &ck_R, &r_inverse, &r);

      Ok((L, R, a_vec_folded, b_vec_folded, ck_folded))
    };

    // two vectors to hold the logarithmic number of group elements
    let mut L_vec: Vec<CompressedCommitment<E>> = Vec::new();
    let mut R_vec: Vec<CompressedCommitment<E>> = Vec::new();

    // we create mutable copies of vectors and generators
    let mut a_vec = W.a_vec.to_vec();
    let mut b_vec = U.b_vec.to_vec();
    let mut ck = ck;
    for _i in 0..usize::try_from(U.b_vec.len().ilog2()).unwrap() {
      let (L, R, a_vec_folded, b_vec_folded, ck_folded) =
        prove_inner(&a_vec, &b_vec, ck, transcript)?;
      L_vec.push(L);
      R_vec.push(R);

      a_vec = a_vec_folded;
      b_vec = b_vec_folded;
      ck = ck_folded;
    }

    Ok(Self { L_vec, R_vec, a_hat: a_vec[0] })
  }

  fn verify(
    &self,
    ck: &CommitmentKey<E>,
    mut ck_c: CommitmentKey<E>,
    n: usize,
    U: &InnerProductInstance<E>,
    transcript: &mut E::TE,
  ) -> Result<(), NovaError> {
    let (ck, _) = ck.clone().split_at(U.b_vec.len());

    transcript.dom_sep(Self::protocol_name());
    if U.b_vec.len() != n
      || n != (1 << self.L_vec.len())
      || self.L_vec.len() != self.R_vec.len()
      || self.L_vec.len() >= 32
    {
      return Err(NovaError::InvalidInputLength);
    }

    // absorb the instance in the transcript
    transcript.absorb(b"U", U);

    // sample a random base for committing to the inner product
    let r = transcript.squeeze(b"r")?;
    ck_c.scale(&r);

    let P = U.comm_a_vec + CE::<E>::commit(&ck_c, &[U.c]);

    // compute a vector of public coins using self.L_vec and self.R_vec
    let r = (0..self.L_vec.len())
      .map(|i| {
        transcript.absorb(b"L", &self.L_vec[i]);
        transcript.absorb(b"R", &self.R_vec[i]);
        transcript.squeeze(b"r")
      })
      .collect::<Result<Vec<E::Scalar>, NovaError>>()?;

    // precompute scalars necessary for verification
    let r_square: Vec<E::Scalar> =
      (0..self.L_vec.len()).into_par_iter().map(|i| r[i] * r[i]).collect();
    let r_inverse = batch_invert(r.clone())?;
    let r_inverse_square: Vec<E::Scalar> =
      (0..self.L_vec.len()).into_par_iter().map(|i| r_inverse[i] * r_inverse[i]).collect();

    // compute the vector with the tensor structure
    let s = {
      let mut s = vec![E::Scalar::ZERO; n];
      s[0] = {
        let mut v = E::Scalar::ONE;
        for r_inverse_i in r_inverse {
          v *= r_inverse_i;
        }
        v
      };
      for i in 1..n {
        let pos_in_r = (31 - (i as u32).leading_zeros()) as usize;
        s[i] = s[i - (1 << pos_in_r)] * r_square[(self.L_vec.len() - 1) - pos_in_r];
      }
      s
    };

    let ck_hat = {
      let c = CE::<E>::commit(&ck, &s).compress();
      CommitmentKey::<E>::reinterpret_commitments_as_ck(&[c])?
    };

    let b_hat = inner_product(&U.b_vec, &s);

    let P_hat = {
      let ck_folded = {
        let ck_L = CommitmentKey::<E>::reinterpret_commitments_as_ck(&self.L_vec)?;
        let ck_R = CommitmentKey::<E>::reinterpret_commitments_as_ck(&self.R_vec)?;
        let ck_P = CommitmentKey::<E>::reinterpret_commitments_as_ck(&[P.compress()])?;
        ck_L.combine(&ck_R).combine(&ck_P)
      };

      CE::<E>::commit(
        &ck_folded,
        &r_square
          .iter()
          .chain(r_inverse_square.iter())
          .chain(iter::once(&E::Scalar::ONE))
          .copied()
          .collect::<Vec<E::Scalar>>(),
      )
    };

    if P_hat == CE::<E>::commit(&ck_hat.combine(&ck_c), &[self.a_hat, self.a_hat * b_hat]) {
      Ok(())
    } else {
      Err(NovaError::PCSError(PCSError::InvalidPCS))
    }
  }
}

#[cfg(test)]
mod test {
  use crate::provider::{
    ipa_pc::EvaluationEngine, util::test_utils::prove_verify_from_num_vars, GrumpkinEngine,
  };

  #[test]
  fn test_multiple_polynomial_size() {
    for num_vars in [4, 5, 6] {
      prove_verify_from_num_vars::<_, EvaluationEngine<GrumpkinEngine>>(num_vars);
    }
  }
}
