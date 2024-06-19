use super::grand_product::{
    BatchedDenseGrandProductLayer, BatchedGrandProduct, BatchedGrandProductLayer,
    BatchedGrandProductProof,
};
use super::sumcheck::SumcheckInstanceProof;
use crate::field::JoltField;
use crate::poly::commitment::commitment_scheme::{BatchType, CommitmentScheme};
use crate::poly::dense_mlpoly::DensePolynomial;
use crate::poly::eq_poly::EqPolynomial;
use crate::utils::math::Math;
use crate::utils::transcript::{AppendToTranscript, ProofTranscript};
use ark_serialize::*;
use ark_std::{One, Zero};
use itertools::Itertools;
use rayon::prelude::*;
use thiserror::Error;

#[derive(CanonicalSerialize, CanonicalDeserialize)]
pub struct QuarkGrandProductProof<C: CommitmentScheme> {
    sumcheck_proof: SumcheckInstanceProof<C::Field>,
    g_commitment: Vec<C::Commitment>,
    claimed_eval_g_r: (Vec<C::Field>, C::BatchedProof),
    claimed_eval_g_r_x: (Vec<C::Field>, Vec<C::Field>, C::BatchedProof),
    helper_values: (Vec<C::Field>, Vec<C::Field>),
    num_vars: usize,
}
pub struct QuarkGrandProduct<F: JoltField> {
    polynomials: Vec<Vec<F>>,
    base_layers: Vec<BatchedDenseGrandProductLayer<F>>,
}

/// The depth in the product tree of the GKR grand product at which the hybrid scheme will switch to using quarks grand product proofs.
const QUARK_HYBRID_LAYER_DEPTH: usize = 3;

impl<F: JoltField, C: CommitmentScheme<Field = F>> BatchedGrandProduct<F, C>
    for QuarkGrandProduct<F>
{
    /// The bottom/input layer of the grand products
    type Leaves = Vec<Vec<F>>;

    /// Constructs the grand product circuit(s) from `leaves`
    fn construct(leaves: Self::Leaves) -> Self {
        // TODO - (aleph_v) Alow custom depths on construction
        let leave_depth = leaves[0].len().log_2();
        let num_layers = if leave_depth <= QUARK_HYBRID_LAYER_DEPTH {
            leave_depth - 1
        } else {
            QUARK_HYBRID_LAYER_DEPTH
        };

        // Taken 1 to 1 from the code in the BatchedDenseGrandProductLayer implementation
        let mut layers = Vec::<BatchedDenseGrandProductLayer<F>>::new();
        layers.push(BatchedDenseGrandProductLayer::<F>::new(leaves));

        for i in 0..num_layers {
            let previous_layers = &layers[i];
            let len = previous_layers.layer_len / 2;
            // TODO(moodlezoup): parallelize over chunks instead of over batch
            let new_layers = previous_layers
                .layers
                .par_iter()
                .map(|previous_layer| {
                    (0..len)
                        .map(|i| previous_layer[2 * i] * previous_layer[2 * i + 1])
                        .collect::<Vec<_>>()
                })
                .collect();
            layers.push(BatchedDenseGrandProductLayer::new(new_layers));
        }

        // If the leaf depth is too small we return no polynomials and all base layers
        if leave_depth <= num_layers {
            return Self {
                polynomials: Vec::<Vec<F>>::new(),
                base_layers: layers,
            };
        }

        // Take the top layer and then turn it into a quark poly
        // Note - We always push the base layer so the unwrap will work even with depth = 0
        let quark_polys = layers.pop().unwrap().layers;

        Self {
            polynomials: quark_polys,
            base_layers: layers,
        }
    }
    /// The number of layers in the grand product, in this case it is the log of the quark layer size plus the gkr layer depth.
    fn num_layers(&self) -> usize {
        self.polynomials[0].len().log_2()
    }
    /// The claimed outputs of the grand products.
    fn claims(&self) -> Vec<F> {
        self.polynomials
            .par_iter()
            .map(|f| f.iter().product())
            .collect()
    }
    /// Returns an iterator over the layers of this batched grand product circuit.
    /// Each layer is mutable so that its polynomials can be bound over the course
    /// of proving.
    #[allow(unreachable_code)]
    fn layers(&'_ mut self) -> impl Iterator<Item = &'_ mut dyn BatchedGrandProductLayer<F>> {
        panic!("We don't use the default prover and so we don't need the generic iterator");
        std::iter::empty()
    }

    /// Computes a batched grand product proof, layer by layer.
    #[tracing::instrument(skip_all, name = "BatchedGrandProduct::prove_grand_product")]
    fn prove_grand_product(
        &mut self,
        transcript: &mut ProofTranscript,
        setup: Option<&C::Setup>,
    ) -> (BatchedGrandProductProof<C>, Vec<F>) {
        let mut proof_layers = Vec::with_capacity(self.base_layers.len());

        // For proofs of polynomials of size less than 16 we support these with no quark proof
        let (quark_option, mut random, mut claims_to_verify) = if !self.polynomials.is_empty() {
            // When doing the quark hybrid proof, we first prove the grand product of a layer of a polynomial which is 4 layers deep in the tree
            // of a standard layered sumcheck grand product, then we use the sumcheck layers to prove via gkr layers that the random point opened
            // by the quark proof is in fact the folded result of the base layer.
            let (quark, random, claims) =
                QuarkGrandProductProof::<C>::prove(&self.polynomials, transcript, setup.unwrap());
            (Some(quark), random, claims)
        } else {
            (
                None,
                Vec::<F>::new(),
                <QuarkGrandProduct<F> as BatchedGrandProduct<F, C>>::claims(self),
            )
        };

        for layer in self.base_layers.iter_mut().rev() {
            proof_layers.push(layer.prove_layer(&mut claims_to_verify, &mut random, transcript));
        }

        (
            BatchedGrandProductProof {
                layers: proof_layers,
                quark_proof: quark_option,
            },
            random,
        )
    }

    /// Verifies the given grand product proof.
    fn verify_grand_product(
        proof: &BatchedGrandProductProof<C>,
        claims: &Vec<F>,
        transcript: &mut ProofTranscript,
        setup: Option<&C::Setup>,
    ) -> (Vec<F>, Vec<F>) {
        // Here we must also support the case where the number of layers is very small
        let (v_points, rand) = match proof.quark_proof.as_ref() {
            Some(quark) => {
                // In this case we verify the quark which fixes the first log(n)-4 vars in the random eval point.
                let v_len = quark.num_vars;
                // Todo (aleph_v) - bubble up errors
                quark
                    .verify(claims, transcript, v_len, setup.unwrap())
                    .unwrap()
            }
            None => {
                // Otherwise we must check the actual claims and the preset random will be empty.
                (claims.clone(), Vec::<F>::new())
            }
        };

        let (sumcheck_claims, sumcheck_r) = <Self as BatchedGrandProduct<F, C>>::verify_layers(
            &proof.layers,
            &v_points,
            transcript,
            rand,
        );

        (sumcheck_claims, sumcheck_r)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum QuarkError {
    /// returned if the sumcheck fails
    #[error("InvalidSumcheck")]
    InvalidQuarkSumcheck,
    /// Returned if a quark opening proof fails
    #[error("IvalidOpeningProof")]
    InvalidOpeningProof,
    /// Returned if eq(tau, r)*(f(1, r) - f(r, 0)*f(r,1)) does not match the result from sumcheck
    #[error("IvalidOpeningProof")]
    InvalidBinding,
}

impl<C: CommitmentScheme> QuarkGrandProductProof<C> {
    /// Computes a grand product proof using the Section 5 technique from Quarks Paper
    /// First - Extends the evals of v to create an f poly, then commits to it and evals
    /// Then - Constructs a g poly and preforms sumcheck proof that sum == 0
    /// Finally - computes opening proofs for a random sampled during sumcheck proof and returns
    /// Returns a random point and evaluation to be verified by the caller (which our hybrid prover does with GKR)
    fn prove(
        leaves: &[Vec<C::Field>],
        transcript: &mut ProofTranscript,
        setup: &C::Setup,
    ) -> (Self, Vec<C::Field>, Vec<C::Field>) {
        let v_length = leaves[0].len();
        let v_variables = v_length.log_2();

        let mut g_polys = Vec::<DensePolynomial<C::Field>>::new();
        let mut v_polys = Vec::<DensePolynomial<C::Field>>::new();
        let mut sumcheck_polys = Vec::<DensePolynomial<C::Field>>::new();
        let mut products = Vec::<C::Field>::new();

        for v in leaves.iter() {
            let v_polynomial = DensePolynomial::<C::Field>::new(v.to_vec());
            v_polys.push(v_polynomial.clone());
            let (f_1_r, f_r_0, f_r_1, p) = v_into_f::<C>(&v_polynomial);
            g_polys.push(f_1_r.clone());
            sumcheck_polys.push(f_1_r);
            sumcheck_polys.push(f_r_0);
            sumcheck_polys.push(f_r_1);
            products.push(p);
        }

        // We bind to these polynomials
        transcript.append_scalars(b"grand product claim", &products);
        let g_commitment = C::batch_commit_polys(&g_polys, setup, BatchType::Big);
        for g in g_commitment.iter() {
            g.append_to_transcript(b"f commitment", transcript);
        }

        // Now we do the sumcheck using the prove arbitrary
        // First instantiate our polynomials
        let tau: Vec<C::Field> = transcript.challenge_vector(b"element for eval poly", v_variables);
        let evals: DensePolynomial<<C as CommitmentScheme>::Field> =
            DensePolynomial::new(EqPolynomial::evals(&tau));
        //We add evals as the last second to last polynomial in the sumcheck
        sumcheck_polys.push(evals);

        // Next we calculate the polynomial equal to 1 at all points but 1,1,1...,0
        let challenge_sum = vec![C::Field::one(); v_variables];
        let eq_sum: DensePolynomial<<C as CommitmentScheme>::Field> =
            DensePolynomial::new(EqPolynomial::evals(&challenge_sum));
        //We add evals as the last to last polynomial in the sumcheck
        sumcheck_polys.push(eq_sum);

        // Sample a constant to do a random linear combination to combine the sumchecks
        let r_combination: Vec<C::Field> =
            transcript.challenge_vector(b"for the random linear comb", g_polys.len());

        // We define a closure using vals[i] = f(1, x), vals[i+1] = f(x, 0), vals[i+2] = f(x, 1)
        let output_check_fn = |vals: &[C::Field]| -> C::Field {
            let eval = vals[vals.len() - 2];
            let eq_sum = vals[vals.len() - 1];
            let mut sum = C::Field::zero();

            for i in 0..(vals.len() / 3) {
                sum += r_combination[i]
                    * (eval * (vals[i * 3] - vals[3 * i + 1] * vals[3 * i + 2])
                        + eq_sum * vals[i * 3 + 1]);
            }
            sum
        };

        // The sumcheck should have the claims times the random coefficents as the sum as all terms are zero except
        // 1,1,..,0 which is r*f(1,1,..0)
        let rlc_claims = products
            .iter()
            .zip(r_combination.iter())
            .map(|(x, r)| *x * r)
            .sum();

        // Now run the sumcheck in arbitrary mode
        // Note - We use the final randomness from binding all variables (x) as the source random for the openings so the verifier can
        //        check that the base layer is the same as is committed too.
        let (sumcheck_proof, x, _) = SumcheckInstanceProof::<C::Field>::prove_arbitrary::<_>(
            &rlc_claims,
            v_variables,
            &mut sumcheck_polys,
            output_check_fn,
            3,
            transcript,
        );

        let borrowed: Vec<&DensePolynomial<C::Field>> = g_polys.iter().collect();
        let chis_r = EqPolynomial::evals(&x);
        let openings_r: Vec<C::Field> = g_polys
            .iter()
            .map(|g| g.evaluate_at_chi_low_optimized(&chis_r))
            .collect();
        // For the version of quarks which only commits to g(1, x) we first do a direct batch proof on x
        let proof = C::batch_prove(
            setup,
            &borrowed,
            &x,
            &openings_r,
            BatchType::Big,
            transcript,
        );
        let claimed_eval_g_r = (openings_r, proof);
        // We are using f(a, x) = a*g(x) + (1-a)*h(x) where f is the polynomial with the cached internal products
        // Let r = (r_1, r')
        // f(r, 0) = r_1 * g(r', 0) + (1-r_1)*h(r', 0)
        // f(r, 1) = r_1 * g(r', 1) + (1-r_1)*h(r', 1)
        // Therefore we do a line reduced opening on g(r', 0) and g(r', 1)e();
        let mut r_prime = vec![C::Field::zero(); x.len() - 1];
        r_prime.clone_from_slice(&x[1..x.len()]);
        let claimed_eval_g_r_x = open_and_prove::<C>(&r_prime, true, &g_polys, setup, transcript);
        // next we need to make a claim about h(r', 0) and h(r', 1) so we use our line reduction to make one claim
        let ((r_t, h_r_t), helper_values) = line_reduce::<C>(&r_prime, true, &v_polys, transcript);

        let num_vars = v_variables;

        (
            Self {
                sumcheck_proof,
                g_commitment,
                claimed_eval_g_r,
                claimed_eval_g_r_x,
                helper_values,
                num_vars,
            },
            r_t,
            h_r_t,
        )
    }

    /// Verifies the given grand product proof.
    #[allow(clippy::type_complexity)]
    fn verify(
        &self,
        claims: &[C::Field],
        transcript: &mut ProofTranscript,
        n_rounds: usize,
        setup: &C::Setup,
    ) -> Result<(Vec<C::Field>, Vec<C::Field>), QuarkError> {
        // First we append the claimed values for the commitment and the product
        transcript.append_scalars(b"grand product claim", claims);
        for g in self.g_commitment.iter() {
            g.append_to_transcript(b"f commitment", transcript);
        }

        //Next sample the tau and construct the evals poly
        let tau: Vec<C::Field> = transcript.challenge_vector(b"element for eval poly", n_rounds);
        let r_combination: Vec<C::Field> =
            transcript.challenge_vector(b"for the random linear comb", self.g_commitment.len());

        // Our sumcheck is expected to equal the RLC of the claims
        let claim_rlc: C::Field = claims
            .iter()
            .zip(r_combination.iter())
            .map(|(x, r)| *x * r)
            .sum();

        // To complete the sumcheck proof we have to validate that our polynomial openings match and are right.
        let (expected, r) = self
            .sumcheck_proof
            .verify(claim_rlc, n_rounds, 3, transcript)
            .map_err(|_| QuarkError::InvalidQuarkSumcheck)?;

        // Again the batch verify expects we have a slice of borrows but we have a slice of Commitments
        let borrowed_g: Vec<&C::Commitment> = self.g_commitment.iter().collect();

        // Get the r_1 and r_prime values
        let r_1 = r[0].clone();
        let mut r_prime = vec![C::Field::zero(); r.len() - 1];
        r_prime.clone_from_slice(&r[1..r.len()]);
        // Firstly we verify that the openings of g(r) are correct
        C::batch_verify(
            &self.claimed_eval_g_r.1,
            setup,
            &r,
            &self.claimed_eval_g_r.0,
            &borrowed_g,
            transcript,
        )
        .map_err(|_| QuarkError::InvalidOpeningProof)?;
        // Next do the line reduction verification of g(r', 0) and g(r', 1)
        line_reduce_opening_verify::<C>(
            &self.claimed_eval_g_r_x,
            &r_prime,
            true,
            &borrowed_g,
            transcript,
            setup,
        )?;
        // Load the h(r,t) values using a line reduction without opening because the opening is done in calling function
        let (r_t, h_r_t) = line_reduce_verify(&self.helper_values, &r_prime, true, transcript);

        // We enforce that f opened at (1,1,...,1, 0) is in fact the product
        let challenge_sum = vec![C::Field::one(); n_rounds];

        // Use the log(n) form to calculate eq(tau, r)
        let eq_eval: C::Field = r
            .iter()
            .zip_eq(tau.iter())
            .map(|(&r_gp, &r_sc)| r_gp * r_sc + (C::Field::one() - r_gp) * (C::Field::one() - r_sc))
            .product();

        // Use the log(n) form to calculate eq(1...1, r)
        let eq_1_eval: C::Field = r
            .iter()
            .zip_eq(challenge_sum.iter())
            .map(|(&r_gp, &r_sc)| r_gp * r_sc + (C::Field::one() - r_gp) * (C::Field::one() - r_sc))
            .product();

        // We calculate f(1, r) = g(r), f(r, 0) = r_1 * g(r', 0) + (1-r_1)*h(r', 0), and  f(r, 1) = r_1 * g(r', 1) + (1-r_1)*h(r', 1)
        let one_r = &self.claimed_eval_g_r.0;
        let r_0: Vec<C::Field> = self
            .claimed_eval_g_r_x
            .0
            .iter()
            .zip(self.helper_values.0.iter())
            .map(|(r, h)| *h + r_1 * (*r - *h))
            .collect();
        let r_1: Vec<C::Field> = self
            .claimed_eval_g_r_x
            .1
            .iter()
            .zip(self.helper_values.1.iter())
            .map(|(r, h)| *h + r_1 * (*r - *h))
            .collect();

        // Finally we check that in fact the polynomial bound by the sumcheck is equal to eq(tau, r)*(f(1, r) - f(r, 0)*f(r,1)) + eq((1,1,.0),r)*f(r,0)
        let mut result_from_openings = C::Field::zero();
        for i in 0..r_0.len() {
            result_from_openings +=
                r_combination[i] * (eq_eval * (one_r[i] - r_0[i] * r_1[i]) + eq_1_eval * r_0[i]);
        }

        if result_from_openings != expected {
            return Err(QuarkError::InvalidBinding);
        }

        Ok((h_r_t, r_t))
    }
}

// Computes slices of f for the sumcheck
#[allow(clippy::type_complexity)]
fn v_into_f<C: CommitmentScheme>(
    v: &DensePolynomial<C::Field>,
) -> (
    DensePolynomial<C::Field>,
    DensePolynomial<C::Field>,
    DensePolynomial<C::Field>,
    C::Field,
) {
    let v_length = v.len();
    let mut f_evals = vec![C::Field::zero(); 2 * v_length];
    let (evals, _) = v.split_evals(v.len());
    f_evals[..v_length].clone_from_slice(evals);

    for i in v_length..2 * v_length {
        let i_shift_mod = (i << 1) % (2 * v_length);
        // The transform follows the logic of the paper and to accumulate
        // the partial sums into the correct indices.
        f_evals[i] = f_evals[i_shift_mod] * f_evals[i_shift_mod + 1]
    }

    // We pull out the co-efficient which instantiate the lower d polys for the sumcheck
    let f_1_x = f_evals[v_length..].to_vec();

    let mut f_x_0 = Vec::new();
    let mut f_x_1 = Vec::new();
    for (i, x) in f_evals.iter().enumerate() {
        if i % 2 == 0 {
            f_x_0.push(*x);
        } else {
            f_x_1.push(*x);
        }
    }

    let f_r_0 = DensePolynomial::new(f_x_0);
    let f_r_1 = DensePolynomial::new(f_x_1);
    let f_1_r = DensePolynomial::new(f_1_x);

    // f(1, ..., 1, 0) = P
    let product = f_evals[2 * v_length - 2];

    (f_1_r, f_r_0, f_r_1, product)
}

// Open a set of polynomials at a point and return the openings and proof
// Note - This uses a special case of the line reduction protocol for the case where we are opening
//        a random which is either 0 or 1 in a position (either the first or last position).
//        In this case the interpolated lined function is constant in all other points except the last one
//        the by picking 0 and 1 as the points we interpolate at we can treat the evals of f(0r) and f(1r)
//        (or vice versa) as implicitly defining the line t*f(0r) + (t-1)f(1r) and so the evals data alone
//        is sufficient to calculate the claimed line, then we sample a random value r_star and do an opening proof
//        on (r_star - 1) * f(0r) + r_star * f(1r) in the commitment to f.
fn open_and_prove<C: CommitmentScheme>(
    r: &[C::Field],
    is_before: bool,
    f_polys: &[DensePolynomial<C::Field>],
    setup: &C::Setup,
    transcript: &mut ProofTranscript,
) -> (Vec<C::Field>, Vec<C::Field>, C::BatchedProof) {
    // Do the line reduction protocol
    let ((r_star, openings_star), (openings_0, openings_1)) =
        line_reduce::<C>(r, is_before, f_polys, transcript);
    // Batch proof requires  &[&]
    let borrowed: Vec<&DensePolynomial<C::Field>> = f_polys.iter().collect();
    let proof = C::batch_prove(
        setup,
        &borrowed,
        &r_star,
        &openings_star,
        BatchType::Big,
        transcript,
    );

    (openings_0, openings_1, proof)
}

fn line_reduce<C: CommitmentScheme>(
    r: &[C::Field],
    is_before: bool,
    f_polys: &[DensePolynomial<C::Field>],
    transcript: &mut ProofTranscript,
) -> (
    (Vec<C::Field>, Vec<C::Field>),
    (Vec<C::Field>, Vec<C::Field>),
) {
    let mut r_0 = if is_before {
        r.to_vec()
    } else {
        vec![C::Field::zero()]
    };
    let mut r_1 = if is_before {
        r.to_vec()
    } else {
        vec![C::Field::one()]
    };

    if is_before {
        r_0.push(C::Field::zero());
        r_1.push(C::Field::one());
    } else {
        r_0.append(&mut r.to_vec());
        r_1.append(&mut r.to_vec());
    }

    let chis_1 = EqPolynomial::evals(&r_0);
    let openings_0: Vec<C::Field> = f_polys
        .iter()
        .map(|f| f.evaluate_at_chi_low_optimized(&chis_1))
        .collect();
    let chis_2 = EqPolynomial::evals(&r_1);
    let openings_1: Vec<C::Field> = f_polys
        .iter()
        .map(|f| f.evaluate_at_chi_low_optimized(&chis_2))
        .collect();

    // We add these to the transcript then sample an r which depends on them all
    transcript.append_scalars(b"claims for line reduction 0", &openings_0);
    transcript.append_scalars(b"claims for line reduction 1", &openings_1);
    let rand: C::Field = transcript.challenge_scalar(b"loading r_star");

    // Now calculate l(rand) = r.rand if is before or rand.r if not is before
    let mut r_star = if is_before { r.to_vec() } else { vec![rand] };
    if is_before {
        r_star.push(rand);
    } else {
        r_star.append(&mut r.to_vec());
    }

    // Now calculate the evals of f at r_star
    let chis_3 = EqPolynomial::evals(&r_star);
    let openings_star: Vec<C::Field> = f_polys
        .iter()
        .map(|f| f.evaluate_at_chi_low_optimized(&chis_3))
        .collect();

    // For debug purposes we will check that (rand - 1) * f(0r) + rand * f(1r) = openings_star
    for (star, (e_0, e_1)) in openings_star
        .iter()
        .zip(openings_0.iter().zip(openings_1.iter()))
    {
        assert_eq!(*e_0 + rand * (*e_1 - *e_0), *star);
    }

    ((r_star, openings_star), (openings_0, openings_1))
}

/// Does the counterpart of the open_and_prove by computing an r_star vector point and then validating this opening
fn line_reduce_opening_verify<C: CommitmentScheme>(
    data: &(Vec<C::Field>, Vec<C::Field>, C::BatchedProof),
    r: &[C::Field],
    is_before: bool,
    commitments: &[&C::Commitment],
    transcript: &mut ProofTranscript,
    setup: &C::Setup,
) -> Result<(), QuarkError> {
    // First compute the line reduction and points
    let (r_star, claimed) =
        &line_reduce_verify(&(data.0.clone(), data.1.clone()), r, is_before, transcript);

    // Finally check the opening at r_star
    let res = C::batch_verify(&data.2, setup, &r_star, &claimed, commitments, transcript);
    match res {
        Ok(_) => Ok(()),
        Err(_) => Err(QuarkError::InvalidOpeningProof),
    }
}

fn line_reduce_verify<F: JoltField>(
    data: &(Vec<F>, Vec<F>),
    r: &[F],
    is_before: bool,
    transcript: &mut ProofTranscript,
) -> (Vec<F>, Vec<F>) {
    // To get our random we first append the openings data
    transcript.append_scalars(b"claims for line reduction 0", &data.0);
    transcript.append_scalars(b"claims for line reduction 1", &data.1);
    let rand: F = transcript.challenge_scalar(b"loading r_star");

    // Compute l(rand) = (r, rand) or (rand,r)
    let mut r_star = if is_before { r.to_vec() } else { vec![rand] };
    if is_before {
        r_star.push(rand);
    } else {
        r_star.append(&mut r.to_vec());
    }

    // Compute our claimed openings
    let claimed: Vec<F> = data
        .0
        .iter()
        .zip(data.1.iter())
        .map(|(e0, e1)| *e0 + rand * (*e1 - *e0))
        .collect();
    (r_star, claimed)
}

#[cfg(test)]
mod quark_grand_product_tests {
    use super::*;
    use crate::poly::commitment::zeromorph::*;
    use ark_bn254::{Bn254, Fr};
    use rand_core::SeedableRng;

    #[test]
    fn quark_e2e() {
        const LAYER_SIZE: usize = 1 << 8;

        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(9 as u64);

        let leaves_1: Vec<Fr> = std::iter::repeat_with(|| Fr::random(&mut rng))
            .take(LAYER_SIZE)
            .collect();
        let leaves_2: Vec<Fr> = std::iter::repeat_with(|| Fr::random(&mut rng))
            .take(LAYER_SIZE)
            .collect();
        let known_products = vec![leaves_1.iter().product(), leaves_2.iter().product()];
        let v = vec![leaves_1, leaves_2];
        let mut transcript: ProofTranscript = ProofTranscript::new(b"test_transcript");

        let srs = ZeromorphSRS::<Bn254>::setup(&mut rng, 1 << 9);
        let setup = srs.trim(1 << 9);

        let (proof, _, _) =
            QuarkGrandProductProof::<Zeromorph<Bn254>>::prove(&v, &mut transcript, &setup);

        // Note resetting the transcript is important
        transcript = ProofTranscript::new(b"test_transcript");
        let result = proof.verify(&known_products, &mut transcript, 8, &setup);

        assert!(result.is_ok(), "Proof did not verify");
    }

    #[test]
    fn quark_hybrid_e2e() {
        const LAYER_SIZE: usize = 1 << 8;

        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(9 as u64);

        let leaves_1: Vec<Fr> = std::iter::repeat_with(|| Fr::random(&mut rng))
            .take(LAYER_SIZE)
            .collect();
        let leaves_2: Vec<Fr> = std::iter::repeat_with(|| Fr::random(&mut rng))
            .take(LAYER_SIZE)
            .collect();
        let known_products: Vec<Fr> = vec![leaves_1.iter().product(), leaves_2.iter().product()];

        let v = vec![leaves_1, leaves_2];
        let mut transcript: ProofTranscript = ProofTranscript::new(b"test_transcript");

        let srs = ZeromorphSRS::<Bn254>::setup(&mut rng, 1 << 9);
        let setup = srs.trim(1 << 9);

        let mut hybrid_grand_product =
            <QuarkGrandProduct<Fr> as BatchedGrandProduct<Fr, Zeromorph<Bn254>>>::construct(v);
        let proof: BatchedGrandProductProof<Zeromorph<Bn254>> = hybrid_grand_product
            .prove_grand_product(&mut transcript, Some(&setup))
            .0;

        // Note resetting the transcript is important
        transcript = ProofTranscript::new(b"test_transcript");
        let _ = QuarkGrandProduct::verify_grand_product(
            &proof,
            &known_products,
            &mut transcript,
            Some(&setup),
        );
    }
}
