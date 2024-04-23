use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};

use crate::{
    poly::{dense_mlpoly::DensePolynomial, field::JoltField},
    utils::{
        errors::ProofVerifyError,
        transcript::{AppendToTranscript, ProofTranscript},
    },
};

#[derive(Clone, Debug)]
pub struct GeneratorShape {
    pub input_length: usize,
    pub batch_size: usize
}

impl GeneratorShape {
    pub fn new(input_length: usize, batch_size: usize) -> Self {
        Self {
            input_length,
            batch_size,
        }
    }
}

pub trait CommitmentScheme: Clone + Sync + Send + 'static {
    type Field: JoltField;
    type Generators: Clone + Sync + Send;
    type Commitment: Sync + Send + CanonicalSerialize + CanonicalDeserialize + AppendToTranscript;
    type Proof: Sync + Send + CanonicalSerialize + CanonicalDeserialize;
    type BatchedProof: Sync + Send + CanonicalSerialize + CanonicalDeserialize;

    fn generators(shapes: &[GeneratorShape]) -> Self::Generators;
    fn commit(poly: &DensePolynomial<Self::Field>, gens: &Self::Generators) -> Self::Commitment;
    fn batch_commit(
        evals: &[&[Self::Field]],
        gens: &Self::Generators,
        batch_size: usize,
    ) -> Vec<Self::Commitment>;
    fn commit_slice(evals: &[Self::Field], gens: &Self::Generators) -> Self::Commitment;
    fn batch_commit_polys(
        polys: &Vec<DensePolynomial<Self::Field>>,
        gens: &Self::Generators,
        batch_size: usize
    ) -> Vec<Self::Commitment>;
    fn batch_commit_polys_ref(
        polys: &Vec<&DensePolynomial<Self::Field>>,
        gens: &Self::Generators,
        batch_size: usize
    ) -> Vec<Self::Commitment>;
    fn prove(
        poly: &DensePolynomial<Self::Field>,
        opening_point: &[Self::Field], // point at which the polynomial is evaluated
        transcript: &mut ProofTranscript,
    ) -> Self::Proof;
    fn batch_prove(
        polynomials: &[&DensePolynomial<Self::Field>],
        opening_point: &[Self::Field],
        openings: &[Self::Field],
        batch_size: usize,
        transcript: &mut ProofTranscript,
    ) -> Self::BatchedProof;

    fn verify(
        proof: &Self::Proof,
        generators: &Self::Generators,
        transcript: &mut ProofTranscript,
        opening_point: &[Self::Field], // point at which the polynomial is evaluated
        opening: &Self::Field,         // evaluation \widetilde{Z}(r)
        commitment: &Self::Commitment,
    ) -> Result<(), ProofVerifyError>;

    fn batch_verify(
        batch_proof: &Self::BatchedProof,
        generators: &Self::Generators,
        opening_point: &[Self::Field],
        openings: &[Self::Field],
        commitments: &[&Self::Commitment],
        transcript: &mut ProofTranscript,
    ) -> Result<(), ProofVerifyError>;

    fn protocol_name() -> &'static [u8];
}
