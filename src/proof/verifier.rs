//! Groth16 proof verification
//! Since these computations are computationally very expensive, we use `elusiv_computation` macros to generate "partial"-computation-functions.
//! Calling those functions `n` times (over the span of multiple transactions) results in a finished computation.

use ark_ec::ProjectiveCurve;
use elusiv_interpreter::elusiv_computation;
use ark_bn254::{Fq, Fq2, Fq6, Fq12, Fq12Parameters, G1Affine, G2Affine, Fq6Parameters, Parameters, G1Projective};
use ark_ff::fields::models::{ QuadExtParameters, fp12_2over3over2::Fp12ParamsWrapper, fp6_3over2::Fp6ParamsWrapper};
use ark_ff::{Field, CubicExtParameters, One, Zero, biginteger::BigInteger256, field_new};
use ark_ec::models::bn::BnParameters;
use std::ops::Neg;
use super::*;
use crate::error::ElusivError::{CouldNotProcessProof, ComputationIsAlreadyFinished, PartialComputationError};
use crate::macros::guard;
use crate::types::U256;

pub fn verify_partial<VKey: VerificationKey>(
    round: usize,
    verifier_account: &mut VerificationAccount,
) -> Result<Option<bool>, ElusivError> {
    // Public input preparation
    if round < VKey::PREPARE_PUBLIC_INPUTS_ROUNDS {
        let input_index = round / 254;
        let result = prepare_public_inputs_partial::<VKey>(
            round,
            verifier_account,
            [0; 32],//verifier_account.account.get_public_input(input_index),
            input_index,
        );
        match result {
            None => guard!(round != VKey::PREPARE_PUBLIC_INPUTS_ROUNDS - 1, CouldNotProcessProof),
            Some(prepared_inputs) => {
                verifier_account.set_prepared_inputs(&G1A(prepared_inputs));

                // Add `r` for the miller loop to the `ram_fq2`
                verifier_account.ram_fq2.write(Fq2::zero(), 0);
                verifier_account.ram_fq2.write(Fq2::zero(), 1);
                verifier_account.ram_fq2.write(Fq2::zero(), 2);
            }
        }
    }

    // Combined miller loop
    else if round < VKey::COMBINED_MILLER_LOOP_ROUNDS {
        let a = G1Affine::zero();
        let b = G2Affine::zero();
        let c = G1Affine::zero();
        let prepared_inputs = G1Affine::zero();
        let mut r = G2HomProjective { x: Fq2::zero(), y: Fq2::zero(), z: Fq2::zero() };

        let result = combined_miller_loop_partial::<VKey>(
            round - VKey::PREPARE_PUBLIC_INPUTS_ROUNDS,
            verifier_account,
            &a,
            &b,
            &c,
            &prepared_inputs,
            &mut r//verifier_account.get_r(),
        )?;

        match result {
            None => {},//guard!(round != VKey::COMBINED_MILLER_LOOP_ROUNDS - 1, CouldNotProcessProof),
            Some(f) => {
                // Add `f` for the final exponentiation to the `ram_fq12`
                verifier_account.ram_fq12.write(f, 0);
            }
        }
    }
    
    // Final exponentiation
    else if round < VKey::FINAL_EXPONENTIATION_ROUNDS {
        let f = Fq12::zero();
        let result = final_exponentiation_partial(
            round - VKey::COMBINED_MILLER_LOOP_ROUNDS,
            verifier_account,
            &f
        )?;

        match result {
            None => {},//guard!(round != VKey::FINAL_EXPONENTIATION_ROUNDS - 1, CouldNotProcessProof),
            Some(v) => {
                // Final verification, we check that: https://github.com/zkcrypto/bellman/blob/9bb30a7bd261f2aa62840b80ed6750c622bebec3/src/groth16/verifier.rs#L43
                // https://github.com/arkworks-rs/groth16/blob/765817f77a6e14964c6f264d565b18676b11bd59/src/verifier.rs#L60
                return Ok(Some(VKey::alpha_g1_beta_g2() == v))
            }
        }
    }

    // Too many rounds
    else {
        return Err(ComputationIsAlreadyFinished)
    }

    Ok(None)
}

macro_rules! read_g1_projective {
    ($ram: expr, $o: literal) => { G1Projective::new($ram.read($o), $ram.read($o + 1), $ram.read($o + 2)) };
}

/// Public input preparation
/// - reference implementation: https://github.com/arkworks-rs/groth16/blob/765817f77a6e14964c6f264d565b18676b11bd59/src/verifier.rs#L22
/// - N public inputs (elements of the scalar field)
/// - the total rounds required for preparation of all inputs is 254 * N
/// - this partial computation is different from the rest, in that the caller directly passes
fn prepare_public_inputs_partial<VKey: VerificationKey>(
    round: usize,
    storage: &mut VerificationAccount,
    input: U256,
    input_index: usize,
) -> Option<G1Affine> {
    let mul_round = round % 254;

    let mut acc: G1Projective = if mul_round == 0 { G1Projective::zero() } else { read_g1_projective!(storage.ram_fq, 3) };

    if mul_round < 254 { // Standard ec scalar multiplication
        // Skip leading zeros
        if mul_round < find_first_non_zero_be(&input) { return None }

        acc.double_in_place();
        if get_bit_be(&input, mul_round) {
            acc.add_assign_mixed(&VKey::gamma_abc_g1(input_index + 1));
        }

        write_g1_projective(&mut storage.ram_fq, acc, 3);
    } else { // Adding
        let mut g_ic = if input_index == 0 { VKey::gamma_abc_g1_0() } else { read_g1_projective!(storage.ram_fq, 0) };

        g_ic += acc;

        if input_index < VKey::PUBLIC_INPUTS_COUNT {
            write_g1_projective(&mut storage.ram_fq, g_ic, 0);
        } else {
            return Some(g_ic.into())
        }
    }
    None
}

fn write_g1_projective(ram: &mut RAMFq, g1p: G1Projective, offset: usize) {
    ram.write(g1p.x, offset);
    ram.write(g1p.y, offset + 1);
    ram.write(g1p.z, offset + 2);
}

// `v`` and the bytes of v are all in LE
fn get_bit_be(v: &U256, index: usize) -> bool {
    let byte = index / 8;
    v[31 - byte] >> (7 - (index % 8)) == 1
}

// `v` is in LE
fn find_first_non_zero_be(v: &U256) -> usize {
    for byte in 0..32 {
        for bit in 0..8 {
            if v[31 - byte] >> (7 - bit) == 1 { return byte * 8 + bit }
        }
    }
    256
}

// We combine the miller loop and the coefficient generation for B
// - miller loop ref: https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L99
// - coefficient generation ref: https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L68
// - implementation:
// - the miller loop receives an iterator over 3 elements (https://github.com/arkworks-rs/groth16/blob/765817f77a6e14964c6f264d565b18676b11bd59/src/verifier.rs#L41)
// - for B we need to generate the coeffs (all other coeffs already are generated befor compilation)
// - so we have a var r = (x: rbx, y: rby, z: rbz)
elusiv_computation!(
    combined_miller_loop<VKey: VerificationKey>(
        storage: &mut VerificationAccount,
        a: &G1Affine, b: &G2Affine, c: &G1Affine, prepared_inputs: &G1Affine, r: &mut G2HomProjective,
    ) -> Fq12 {
        {
            r.x = b.x;
            r.x = b.y;
            r.x = Fq2::one();

            let f: Fq12 = Fq12::one();

            // values for B coeffs generation (https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L79)
            let alt_b: G2A = G2A(b.neg());
            let c0: Fq2 = Fq2::zero();
            let c1: Fq2 = Fq2::zero();
            let c2: Fq2 = Fq2::zero();
        }

        // Reversed ATE_LOOP_COUNT with the the last element removed (so the first in the reversed order)
        // https://github.com/arkworks-rs/curves/blob/1551d6d76ce5abf6e7925e53b0ea1af7dbc421c3/bn254/src/curves/mod.rs#L21
        { for i, ate_loop_count in [1,1,0,1,0,0,2,0,1,1,0,0,0,2,0,0,1,1,0,0,2,0,0,0,0,0,1,0,0,2,0,0,1,1,1,0,0,0,0,2,0,1,0,0,2,0,1,1,0,0,1,0,0,2,1,0,0,2,0,1,0,1,0,0,0]
            {
                if (i > 0) {
                    f = f.square();
                };

                partial v = doubling_step(storage, r) { c0=v.0; c1=v.1; c2=v.2; };
                partial v = combined_ell::<VKey>(storage, a, prepared_inputs, c, &c0, &c1, &c2, i, f) { f = v; };

                if (ate_loop_count > 0) {
                    if (ate_loop_count = 1) {
                        partial v = addition_step(r, b) { c0=v.0; c1=v.1; c2=v.2; };
                    } else {
                        partial v = addition_step(r, &(alt_b.0)) { c0=v.0; c1=v.1; c2=v.2; };
                    };

                    partial v = combined_ell::<VKey>(storage, a, prepared_inputs, c, &c0, &c1, &c2, i, f) { f = v; };
                };
            }
        }

        // The final two coefficient triples
        {
            if (!(prepared_inputs.is_zero())) {
                partial v = mul_by_characteristics(storage, b) { alt_b = G2A(v); };
                partial v = addition_step(r, &(alt_b.0)) { c0=v.0; c1=v.1; c2=v.2; };
                partial v = combined_ell::<VKey>(storage, a, prepared_inputs, c, &c0, &c1, &c2, 0, f) { f = v; };
                partial v = mul_by_characteristics(storage, &(alt_b.0)) { alt_b = G2A(v); };
                alt_b = G2A(G2Affine::new(alt_b.0.x, alt_b.0.y.neg(), alt_b.0.infinity));
                partial v = addition_step(r, &(alt_b.0)) { c0=v.0; c1=v.1; c2=v.2; };
                partial v = combined_ell::<VKey>(storage, a, prepared_inputs, c, &c0, &c1, &c2, 0, f) { f = v; };
            }
        }
        { return f; }
    }
);

const fn max(a: usize, b: usize) -> usize { if a > b { a } else { b } }

// Homogenous projective coordinates form
#[derive(Debug)]
pub struct G2HomProjective {
    pub x: Fq2,
    pub y: Fq2,
    pub z: Fq2,
}

/// Inverse of 2 (in q)
/// - Calculated using: Fq::one().double().inverse().unwrap()
const TWO_INV: Fq = Fq::new(BigInteger256::new([9781510331150239090, 15059239858463337189, 10331104244869713732, 2249375503248834476]));

/// https://docs.rs/ark-bn254/0.3.0/src/ark_bn254/curves/g2.rs.html#19
/// COEFF_B = 3/(u+9) = (19485874751759354771024239261021720505790618469301721065564631296452457478373, 266929791119991161246907387137283842545076965332900288569378510910307636690)
const COEFF_B: Fq2 = field_new!(Fq2,
    field_new!(Fq, "19485874751759354771024239261021720505790618469301721065564631296452457478373"),
    field_new!(Fq, "266929791119991161246907387137283842545076965332900288569378510910307636690"),
);

type Coefficients = (Fq2, Fq2, Fq2);

// Doubling step
// https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L139
elusiv_computation!(
    doubling_step(storage: &mut VerificationAccount, r: &mut G2HomProjective) -> Coefficients {
        {
            let mut a: Fq2 = r.x * r.y;
            a = mul_by_fp(&a, TWO_INV);
            let b: Fq2 = r.y.square();
            let c: Fq2 = r.z.square();
            let e: Fq2 = COEFF_B * (c.double() + c);
            let f: Fq2 = e.double() + e;
            let mut g: Fq2 = b + f;
            g = mul_by_fp(&g, TWO_INV);
            let h0: Fq2 = r.y + r.z;
            let h: Fq2 = h0.square() - (b + c);
            let e_square: Fq2 = e.square();
        }
        {
            r.x = a * (b - f);
            r.y = g.square() - (e_square.double() + e_square);
            r.z = b * h;
        }
        {
            let i: Fq2 = e - b;
            let j: Fq2 = r.x.square();
            return new_coeffs(h.neg(), j.double() + j, i);
        }
    }
);

fn new_coeffs(c0: Fq2, c1: Fq2, c2: Fq2) -> Coefficients { (c0, c1, c2) }

// Addition step
// https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L168
elusiv_computation!(
    addition_step (r: &mut G2HomProjective, q: &G2Affine) -> Coefficients {
        {
            let theta: Fq2 = r.y - (q.y * r.z);
            let lambda: Fq2 = r.x - (q.x * r.z);
            let c: Fq2 = theta.square();
            let d: Fq2 = lambda.square();
            let e: Fq2 = lambda * d;
            let f: Fq2 = r.z * c;
            let g: Fq2 = r.x * d;
            let h: Fq2 = e + f - g.double();
            let rx: Fq2 = lambda * h;
            let ry: Fq2 = theta * (g - h) - (e * r.y);
            let rz: Fq2 = r.z * e;
            let j: Fq2 = theta * q.x - (lambda * q.y);

            r.x = rx;
            r.y = ry;
            r.z = rz;

            return new_coeffs(lambda, theta.neg(), j);
        }
    }
);

const TWIST_MUL_BY_Q_X: Fq2 = Parameters::TWIST_MUL_BY_Q_X;
const TWIST_MUL_BY_Q_Y: Fq2 = Parameters::TWIST_MUL_BY_Q_Y;

// Mul by characteristics
// https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L127
elusiv_computation!(
    mul_by_characteristics(storage: &mut VerificationAccount, r: &G2Affine) -> G2Affine {
        {
            let mut x: Fq2 = frobenius_map_fq2_one(r.x);
            x = x * TWIST_MUL_BY_Q_X;
        }
        {
            let mut y: Fq2 = frobenius_map_fq2_one(r.y);
            y = y * TWIST_MUL_BY_Q_Y;
            return G2Affine::new(x, y, r.infinity);
        }
    }
);

fn frobenius_map_fq2_one(f: Fq2) -> Fq2 {
    let mut k = f.clone();
    k.frobenius_map(1);
    k
}

// We evaluate the line function for A, the prepared inputs and C
// - inside the miller loop we do evaluations on three elements
// - multi_ell combines those three calls in one function
// - normal ell implementation: https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L59
// - (also in this file's test mod there is an elusiv_computation!-implementation for the single ell to compare)
elusiv_computation!(
    combined_ell<VKey: VerificationKey>(
        storage: &mut VerificationAccount,
        a: &G1Affine, prepared_inputs: &G1Affine, c: &G1Affine, c0: &Fq2, c1: &Fq2, c2: &Fq2, coeff_index: usize, f: Fq12,
    ) -> Fq12 {
        {
            let r: Fq12 = f;

            let a0: Fq2 = mul_by_fp(c0, a.y);
            let a1: Fq2 = mul_by_fp(c1, a.x);
        }
        {
            if (!(a.is_zero())) {
                partial v = mul_by_034(storage, &a0, &a1, c2, r) { r = v }
            }
        }

        {
            let b0: Fq2 = mul_by_fp(&(VKey::gamma_g2_neg_pc_0(coeff_index)), prepared_inputs.y);
            let b1: Fq2 = mul_by_fp(&(VKey::gamma_g2_neg_pc_1(coeff_index)), prepared_inputs.x);
        }
        {
            if (!(prepared_inputs.is_zero())) {
                partial v = mul_by_034(storage, &b0, &b1, &(VKey::gamma_g2_neg_pc_2(coeff_index)), r) { r = v } }
            }
        {
            let d0: Fq2 = mul_by_fp(&(VKey::delta_g2_neg_pc_0(coeff_index)), c.y);
            let d1: Fq2 = mul_by_fp(&(VKey::delta_g2_neg_pc_1(coeff_index)), c.x);
        }
        {
            if (!(c.is_zero())) {
                partial v = mul_by_034(storage, &d0, &d1, &(VKey::delta_g2_neg_pc_2(coeff_index)), r) { r = v }
            }
        }

        { return r; }
    }
);

// f.mul_by_034(c0, c1, coeffs.2); (with: self -> f; c0 -> c0; d0 -> c1; d1 -> coeffs.2)
// https://github.com/arkworks-rs/r1cs-std/blob/b7874406ec614748608b1739b1578092a8c97fb8/src/fields/fp12.rs#L43
elusiv_computation!(
    mul_by_034(
        storage: &mut VerificationAccount,
        c0: &Fq2, d0: &Fq2, d1: &Fq2, f: Fq12
    ) -> Fq12 {
        { let a: Fq6 = Fq6::new(f.c0.c0 * c0, f.c0.c1 * c0, f.c0.c2 * c0); }
        { let b: Fq6 = mul_fq6_by_c0_c1_0(f.c1, d0, d1); }
        { let e: Fq6 = mul_fq6_by_c0_c1_0(f.c0 + f.c1, &(*c0 + d0), d1); }
        { return Fq12::new(mul_base_field_by_nonresidue(b) + a, e - (a + b)); }
    }
);

// https://github.com/arkworks-rs/algebra/blob/4dd6c3446e8ab22a2ba13505a645ea7b3a69f493/ff/src/fields/models/quadratic_extension.rs#L87
// https://github.com/arkworks-rs/algebra/blob/4dd6c3446e8ab22a2ba13505a645ea7b3a69f493/ff/src/fields/models/quadratic_extension.rs#L56
fn sub_and_mul_base_field_by_nonresidue(x: Fq6, y: Fq6) -> Fq6 {
    x - mul_base_field_by_nonresidue(y)
}

fn mul_base_field_by_nonresidue(v: Fq6) -> Fq6 {
    Fp12ParamsWrapper::<Fq12Parameters>::mul_base_field_by_nonresidue(&v)
}

// https://github.com/arkworks-rs/r1cs-std/blob/b7874406ec614748608b1739b1578092a8c97fb8/src/fields/fp6_3over2.rs#L53
fn mul_fq6_by_c0_c1_0(f: Fq6, c0: &Fq2, c1: &Fq2) -> Fq6 {
    let v0: Fq2 = f.c0 * c0;
    let v1: Fq2 = f.c1 * c1;

    let a1_plus_a2: Fq2 = f.c1 + f.c2;
    let a0_plus_a1: Fq2 = f.c0 + f.c1;
    let a0_plus_a2: Fq2 = f.c0 + f.c2;

    let b1_plus_b2: Fq2 = c1.clone();
    let b0_plus_b1: Fq2 = *c0 + c1;
    let b0_plus_b2: Fq2 = c0.clone();

    Fq6::new(
        (a1_plus_a2 * b1_plus_b2 - &v1) * Fp6ParamsWrapper::<Fq6Parameters>::NONRESIDUE + &v0,
        a0_plus_a1 * &b0_plus_b1 - &v0 - &v1,
        a0_plus_a2 * &b0_plus_b2 - &v0 + &v1,
    )
}

fn mul_by_fp(v: &Fq2, fp: Fq) -> Fq2 {
    let mut v: Fq2 = *v;
    v.mul_assign_by_fp(&fp);
    v
}

// Final exponentiation
// - reference implementation: https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L153
elusiv_computation!(
    final_exponentiation(storage: &mut VerificationAccount, f: &Fq12) -> Fq12 {
        {
            let r: Fq12 = conjugate(*f);
            let f2: Fq12 = *f;
        }
        { partial v = inverse_fq12(storage, f2)
            {
                r = r * v;
                f2 = r;
            }
        }
        {
            r = frobenius_map(r, 2);
            r = r * f2;
            let y0: Fq12 = r;
        }
        { partial v = exp_by_neg_x(storage, y0) { y0 = v; } }
        {
            let y1: Fq12 = y0.cyclotomic_square();
            let y2: Fq12 = y1.cyclotomic_square();
            let y3: Fq12 = y2 * y1;
            let y4: Fq12 = y3;
        }
        { partial v = exp_by_neg_x(storage, y4) { y4 = v; } }
        {
            let y5: Fq12 = y4.cyclotomic_square();
            let y6: Fq12 = y5;
        }
        { partial v = exp_by_neg_x(storage, y6) { y6 = v; } }
        {
            y3 = conjugate(y3);
            y6 = conjugate(y6);
        }
        {
            let y7: Fq12 = y6 * y4;
            let y8: Fq12 = y7 * y3;
            let y9: Fq12 = y8 * y1;
            let y10: Fq12 = y8 * y4;
        }
        {
            let y11: Fq12 = y10 * r;
            let mut y12: Fq12 = y9;
            y12 = frobenius_map(y12, 1);
        }
        {
            let y13: Fq12 = y12 * y11;
            y8 = frobenius_map(y8, 2);
        }
        {
            let y14: Fq12 = y8 * y13;
            r = conjugate(r);
        }
        {
            let mut y15: Fq12 = r * y9;
            y15 = frobenius_map(y15, 3);
            return y15 * y14;
        }
    }
);

// https://github.com/arkworks-rs/algebra/blob/4dd6c3446e8ab22a2ba13505a645ea7b3a69f493/ff/src/fields/models/quadratic_extension.rs#L366
// Guide to Pairing-based Cryptography, Algorithm 5.19.
elusiv_computation!(
    inverse_fq12(storage: &mut VerificationAccount, f: Fq12) -> Fq12 {
        { let v1: Fq6 = f.c1.square(); }
        { let v2: Fq6 = f.c0.square(); }
        { let mut v0: Fq6 = sub_and_mul_base_field_by_nonresidue(v2, v1); }
        { let v3: Fq6 = unwrap v0.inverse(); }
        {
            let v: Fq6 = f.c1 * v3;
            return Fq12::new(f.c0 * v3, v.neg());
        }
    }
);

// Using exp_by_neg_x and cyclotomic_exp
// https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L78
// https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ff/src/fields/models/fp12_2over3over2.rs#L56
elusiv_computation!(
    exp_by_neg_x(storage: &mut VerificationAccount, fe: Fq12) -> Fq12 {
        {
            let fe_inverse: Fq12 = conjugate(fe);
            let res: Fq12 = Fq12::one();
        }

        // Non-adjacent window form of exponent Parameters::X (u64: 4965661367192848881)
        // NAF computed using: https://citeseerx.ist.psu.edu/viewdoc/download?doi=10.1.1.394.3037&rep=rep1&type=pdf Page 98
        // - but removed the last zero value, since it has no effect
        // - and then inverted the array
        { for i, value in [1,0,0,0,1,0,1,0,0,2,0,1,0,1,0,2,0,0,1,0,1,0,2,0,2,0,2,0,1,0,0,0,1,0,0,1,0,1,0,1,0,2,0,1,0,0,1,0,0,0,0,1,0,1,0,0,0,0,2,0,0,0,1]
            {
                if (i > 0) {
                    res = res.cyclotomic_square();
                };

                if (value > 0) {
                    if (value = 1) {
                        res = res * fe;
                    } else { // value == 2
                        res = res * fe_inverse;
                    }
                };
            }
        }
        { return conjugate(res); }
    }
);

fn conjugate(f: Fq12) -> Fq12 {
    let mut k = f.clone();
    k.conjugate();
    k
}
fn frobenius_map(f: Fq12, u: usize) -> Fq12 {
    let mut k = f.clone();
    k.frobenius_map(u);
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use ark_bn254::{ Fr, Bn254 };
    use ark_ec::{AffineCurve, PairingEngine};
    use ark_ec::models::bn::BnParameters;
    use ark_ff::PrimeField;
    use borsh::BorshSerialize;
    use crate::fields::Wrap;

    type VK = super::super::vkey::SendVerificationKey;

    fn f() -> Fq12 {
        let f = Fq6::new(
            Fq2::new(
                Fq::from_str("20925091368075991963132407952916453596237117852799702412141988931506241672722").unwrap(),
                Fq::from_str("18684276579894497974780190092329868933855710870485375969907530111657029892231").unwrap(),
            ),
            Fq2::new(
                Fq::from_str("5932690455294482368858352783906317764044134926538780366070347507990829997699").unwrap(),
                Fq::from_str("18684276579894497974780190092329868933855710870485375969907530111657029892231").unwrap(),
            ),
            Fq2::new(
                Fq::from_str("18684276579894497974780190092329868933855710870485375969907530111657029892231").unwrap(),
                Fq::from_str("19526707366532583397322534596786476145393586591811230548888354920504818678603").unwrap(),
            ),
        );
        Fq12::new(f, f)
    }

    fn g2_affine() -> G2Affine { G2Affine::new(f().c0.c0, f().c0.c1, false) }

    macro_rules! storage {
        ($id: ident) => {
            let mut data = vec![0; VerificationAccount::SIZE];
            let mut $id = VerificationAccount::new(&mut data).unwrap();
        };
    }

    #[test]
    fn test_verify_partial() {
        panic!()
    }

    #[test]
    fn test_prepare_public_inputs() {
        let public_inputs = vec![
            Fr::from_str("5932690455294482368858352783906317764044134926538780366070347507990829997699").unwrap();
            VK::PUBLIC_INPUTS_COUNT
        ];
        storage!(storage);
        let mut value: Option<G1Affine> = None;
        for round in 0..VK::PUBLIC_INPUTS_COUNT * 254 {
            let input_index = round / VK::PUBLIC_INPUTS_COUNT;
            let input = <Wrap<Fr>>::try_to_vec(&Wrap(public_inputs[input_index])).unwrap();
            let input: U256 = input.try_into().unwrap();
            value = prepare_public_inputs_partial::<VK>(round, &mut storage, input, input_index);
        }

        assert_eq!(value.unwrap(), reference_prepare_inputs::<VK>(&public_inputs));
    }

    #[test]
    fn test_mul_by_characteristics() {
        storage!(storage);
        let mut value: Option<G2Affine> = None;
        for round in 0..MUL_BY_CHARACTERISTICS_ROUNDS_COUNT {
            value = mul_by_characteristics_partial(round, &mut storage, &g2_affine()).unwrap();
        }

        assert_eq!(value.unwrap(), reference_mul_by_char(g2_affine()));
    }

    #[test]
    fn test_ell() {
        panic!()
    }

    #[test]
    fn test_inverse_fq12() {
        storage!(storage);
        let mut value: Option<Fq12> = None;
        for round in 0..INVERSE_FQ12_ROUNDS_COUNT {
            value = inverse_fq12_partial(round, &mut storage, f()).unwrap();
        }

        assert_eq!(value.unwrap(), f().inverse().unwrap());
    }

    #[test]
    fn test_exp_by_neg_x() {
        storage!(storage);
        let mut value: Option<Fq12> = None;
        for round in 0..EXP_BY_NEG_X_ROUNDS_COUNT {
            value = exp_by_neg_x_partial(round, &mut storage, f()).unwrap();
        }

        assert_eq!(value.unwrap(), reference_exp_by_neg_x(f()));
    }

    #[test]
    fn test_final_exponentiation() {
        storage!(storage);
        let mut value = None;
        for round in 0..FINAL_EXPONENTIATION_ROUNDS_COUNT {
            value = final_exponentiation_partial(round, &mut storage, &f()).unwrap();
        }

        assert_eq!(value.unwrap(), Bn254::final_exponentiation(&f()).unwrap());
    }

    // Adaption of: https://github.com/arkworks-rs/groth16/blob/765817f77a6e14964c6f264d565b18676b11bd59/src/verifier.rs#L22
    fn reference_prepare_inputs<VKey: VerificationKey>(public_inputs: &[Fr]) -> G1Projective {
        assert!(public_inputs.len() == VKey::PUBLIC_INPUTS_COUNT);

        let mut g_ic = VKey::gamma_abc_g1_0();
        for i in 0..VKey::PUBLIC_INPUTS_COUNT {
            let b = VKey::gamma_abc_g1(i + 1);
            let v = &b.mul(public_inputs[i].into_repr());
            g_ic += v;
        }

        g_ic
    }

    // https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L59
    /*fn reference_ell(f: Fq12, coeffs: (Fq2, Fq2, Fq2), p: G1Affine) -> Fq12 {
        let mut c0: Fq2 = coeffs.0;
        let mut c1: Fq2 = coeffs.1;
        let c2: Fq2 = coeffs.2;
    
        c0.mul_assign_by_fp(&p.y);
        c1.mul_assign_by_fp(&p.x);

        let mut f = f;
        f.mul_by_034(&c0, &c1, &c2);
        f
    }*/

    // https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/g2.rs#L127
    fn reference_mul_by_char(r: G2Affine) -> G2Affine {
        let mut s = r;
        s.x.frobenius_map(1);
        s.x *= TWIST_MUL_BY_Q_X;
        s.y.frobenius_map(1);
        s.y *= TWIST_MUL_BY_Q_Y;
        s
    }

    // https://github.com/arkworks-rs/algebra/blob/6ea310ef09f8b7510ce947490919ea6229bbecd6/ec/src/models/bn/mod.rs#L78
    fn reference_exp_by_neg_x(f: Fq12) -> Fq12 {
        let mut f = f.cyclotomic_exp(&Parameters::X);
        if !Parameters::X_IS_NEGATIVE { f.conjugate(); }
        f
    }
}