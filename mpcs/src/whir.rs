mod field_wrapper;
mod spec;
mod structure;
mod utils;

use super::PolynomialCommitmentScheme;
use ff_ext::ExtensionField;
use field_wrapper::ExtensionFieldWrapper as FieldWrapper;
use serde::{Serialize, de::DeserializeOwned};
use spec::WhirSpec;
use structure::{Whir, WhirDigest, WhirInnerT, digest_to_bytes};
use utils::poly2whir;
pub use whir::ceno_binding::Error;
use whir::ceno_binding::PolynomialCommitmentScheme as WhirPCS;

impl<E: ExtensionField, Spec: WhirSpec<E>> PolynomialCommitmentScheme<E> for Whir<E, Spec>
where
    E: Serialize + DeserializeOwned,
    E::BaseField: Serialize + DeserializeOwned,
{
    type Param = <WhirInnerT<E, Spec> as WhirPCS<FieldWrapper<E>>>::Param;
    type ProverParam = <WhirInnerT<E, Spec> as WhirPCS<FieldWrapper<E>>>::Param;
    type VerifierParam = <WhirInnerT<E, Spec> as WhirPCS<FieldWrapper<E>>>::Param;
    type Commitment = WhirDigest<E, Spec>;
    type Proof = <WhirInnerT<E, Spec> as WhirPCS<FieldWrapper<E>>>::Proof;
    type CommitmentWithWitness =
        <WhirInnerT<E, Spec> as WhirPCS<FieldWrapper<E>>>::CommitmentWithWitness;
    type CommitmentChunk = WhirDigest<E, Spec>;

    fn setup(poly_size: usize) -> Result<Self::Param, crate::Error> {
        Ok(WhirInnerT::<E, Spec>::setup(poly_size))
    }

    fn trim(
        param: Self::Param,
        poly_size: usize,
    ) -> Result<(Self::ProverParam, Self::VerifierParam), crate::Error> {
        if poly_size > (1 << param.num_variables) {
            return Err(crate::Error::InvalidPcsParam(
                "Poly size is greater than param poly size".to_string(),
            ));
        }
        // TODO: Do the real trim instead of regenerating.
        let param = WhirInnerT::<E, Spec>::setup(poly_size);
        Ok((param.clone(), param.clone()))
    }

    fn commit(
        pp: &Self::ProverParam,
        poly: &multilinear_extensions::mle::DenseMultilinearExtension<E>,
    ) -> Result<Self::CommitmentWithWitness, crate::Error> {
        let witness = WhirInnerT::<E, Spec>::commit(&pp, &poly2whir(&poly))
            .map_err(crate::Error::WhirError)?;

        Ok(witness)
    }

    fn write_commitment(
        comm: &Self::Commitment,
        transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<(), crate::Error> {
        transcript.append_message(&digest_to_bytes::<Spec, E>(&comm.inner)?);
        Ok(())
    }

    fn open(
        pp: &Self::ProverParam,
        _poly: &multilinear_extensions::mle::DenseMultilinearExtension<E>,
        comm: &Self::CommitmentWithWitness,
        point: &[E],
        eval: &E,
        _transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<Self::Proof, crate::Error> {
        WhirInnerT::<E, Spec>::open(
            &pp,
            comm.clone(), // TODO: Remove clone
            point
                .iter()
                .map(|x| FieldWrapper(*x))
                .collect::<Vec<_>>()
                .as_slice(),
            &FieldWrapper(*eval),
        )
        .map_err(crate::Error::WhirError)
    }

    fn verify(
        vp: &Self::VerifierParam,
        comm: &Self::Commitment,
        point: &[E],
        eval: &E,
        proof: &Self::Proof,
        _transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<(), crate::Error> {
        WhirInnerT::<E, Spec>::verify(
            vp,
            &comm.inner,
            &point.iter().map(|x| FieldWrapper(*x)).collect::<Vec<_>>(),
            &FieldWrapper(*eval),
            proof,
        )
        .map_err(crate::Error::WhirError)
    }

    fn get_pure_commitment(comm: &Self::CommitmentWithWitness) -> Self::Commitment {
        Self::Commitment {
            inner: comm.commitment.clone(),
        }
    }

    fn batch_commit(
        pp: &Self::ProverParam,
        polys: &[multilinear_extensions::mle::DenseMultilinearExtension<E>],
    ) -> Result<Self::CommitmentWithWitness, crate::Error> {
        todo!()
    }

    fn batch_open(
        pp: &Self::ProverParam,
        polys: &[multilinear_extensions::mle::DenseMultilinearExtension<E>],
        comms: &[Self::CommitmentWithWitness],
        points: &[Vec<E>],
        evals: &[crate::Evaluation<E>],
        transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<Self::Proof, crate::Error> {
        todo!()
    }

    fn simple_batch_open(
        pp: &Self::ProverParam,
        polys: &[multilinear_extensions::virtual_poly::ArcMultilinearExtension<E>],
        comm: &Self::CommitmentWithWitness,
        point: &[E],
        evals: &[E],
        transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<Self::Proof, crate::Error> {
        todo!()
    }

    fn batch_verify(
        vp: &Self::VerifierParam,
        comms: &[Self::Commitment],
        points: &[Vec<E>],
        evals: &[crate::Evaluation<E>],
        proof: &Self::Proof,
        transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<(), crate::Error> {
        todo!()
    }

    fn simple_batch_verify(
        vp: &Self::VerifierParam,
        comm: &Self::Commitment,
        point: &[E],
        evals: &[E],
        proof: &Self::Proof,
        transcript: &mut impl transcript::Transcript<E>,
    ) -> Result<(), crate::Error> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gen_rand_poly_base, run_commit_open_verify};
    use goldilocks::GoldilocksExt2;
    use spec::WhirDefaultSpec;

    type PcsGoldilocks = Whir<GoldilocksExt2, WhirDefaultSpec>;

    #[test]
    fn whir_commit_open_verify_goldilocks() {
        // TODO: Only support committing to base field polynomial now
        for gen_rand_poly in [gen_rand_poly_base] {
            // Challenge is over extension field, poly over the base field
            run_commit_open_verify::<GoldilocksExt2, PcsGoldilocks>(gen_rand_poly, 10, 11);
            // Test trivial proof with small num vars
            run_commit_open_verify::<GoldilocksExt2, PcsGoldilocks>(gen_rand_poly, 4, 6);
        }
    }
}
