use crate::error::HashToCurveError;
use crate::hash_to_curve::hash_to_curve;
use ark_ec::short_weierstrass_jacobian::GroupAffine;
use ark_ec::{models::SWModelParameters, AffineCurve, ProjectiveCurve};
use ark_ff::PrimeField;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Read, SerializationError, Write};
use ark_std::{rand::Rng, UniformRand};
use secp256k1::sec1::Sec1EncodePoint;
use sha2::digest::Output;
use sha2::{Digest, Sha256};

mod error;
mod hash_to_curve;

const EXPECT_MSG_DECODE: &str = "the value decoded have been generated by a function which is improbable to output a malformed hexstring (still a place for refactoring)";

pub enum PlumeVersion {
    V1,
    V2,
}

pub fn affine_to_bytes<P: SWModelParameters>(point: &GroupAffine<P>) -> Vec<u8> {
    hex::decode(point.to_encoded_point(true))
        .expect(EXPECT_MSG_DECODE)
        .to_vec()
}

fn compute_h<'a, C: ProjectiveCurve, Fq: PrimeField, P: SWModelParameters>(
    pk: &GroupAffine<P>,
    message: &'a [u8],
) -> Result<GroupAffine<P>, HashToCurveError> {
    //let pk_affine_bytes_vec = affine_to_bytes::<P>(pk);
    //let m_pk = [message, pk_affine_bytes_vec.as_slice()].concat();
    //hash_to_curve::try_and_increment::<C>(m_pk.as_slice())
    hash_to_curve::<Fq, P>(message, pk)
}

fn compute_c_v1<P: SWModelParameters>(
    g_point: &GroupAffine<P>,
    pk: &GroupAffine<P>,
    hashed_to_curve: &GroupAffine<P>,
    nullifier: &GroupAffine<P>,
    r_point: &GroupAffine<P>,
    hashed_to_curve_r: &GroupAffine<P>,
) -> Output<Sha256> {
    // Compute c = sha512([g, pk, h, nul, g^r, z])
    let c_preimage_vec = [
        affine_to_bytes::<P>(g_point),
        affine_to_bytes::<P>(pk),
        affine_to_bytes::<P>(hashed_to_curve),
        affine_to_bytes::<P>(nullifier),
        affine_to_bytes::<P>(r_point),
        affine_to_bytes::<P>(hashed_to_curve_r),
    ]
    .concat();

    Sha256::digest(c_preimage_vec.as_slice())
}

fn compute_c_v2<P: SWModelParameters>(
    nullifier: &GroupAffine<P>,
    r_point: &GroupAffine<P>,
    hashed_to_curve_r: &GroupAffine<P>,
) -> Output<Sha256> {
    // Compute c = sha512([nul, g^r, z])
    let nul_bytes = affine_to_bytes::<P>(nullifier);
    let g_r_bytes = affine_to_bytes::<P>(r_point);
    let z_bytes = affine_to_bytes::<P>(hashed_to_curve_r);

    let c_preimage_vec = [nul_bytes, g_r_bytes, z_bytes].concat();

    Sha256::digest(c_preimage_vec.as_slice())
}

#[derive(
    Copy,
    Clone,
    ark_serialize_derive::CanonicalSerialize,
    ark_serialize_derive::CanonicalDeserialize,
)]
pub struct Parameters<P: SWModelParameters> {
    pub g_point: GroupAffine<P>,
}

#[derive(
    Copy,
    Clone,
    ark_serialize_derive::CanonicalSerialize,
    ark_serialize_derive::CanonicalDeserialize,
)]
pub struct PlumeSignature<P: SWModelParameters> {
    pub hashed_to_curve_r: GroupAffine<P>,
    pub r_point: GroupAffine<P>,
    pub s: P::ScalarField,
    pub c: P::ScalarField,
    pub nullifier: GroupAffine<P>,
}

// These aliases should be gone in #88 . If they won't TODO pay attention to the warning about `trait` boundaries being not checked for aliases
//      also not enforcing trait bounds can impact PublicKey -- it's better to find appropriate upstream type
type Message<'a> = &'a [u8];
type PublicKey<P: SWModelParameters> = GroupAffine<P>;
type SecretKeyMaterial<P: SWModelParameters> = P::ScalarField;

impl<P: SWModelParameters> PlumeSignature<P> {
    /// Generate the public key and a private key.
    fn keygen(pp: &Parameters<P>, rng: &mut impl Rng) -> (PublicKey<P>, SecretKeyMaterial<P>) {
        let secret_key = SecretKeyMaterial::<P>::rand(rng);
        let public_key = pp.g_point.mul(secret_key).into();
        (public_key, secret_key)
    }

    /// Sign a message using a specified r value
    fn sign_with_r(
        pp: &Parameters<P>,
        keypair: (&PublicKey<P>, &SecretKeyMaterial<P>),
        message: Message,
        r_scalar: P::ScalarField,
        version: PlumeVersion,
    ) -> Result<Self, HashToCurveError> {
        let g_point = pp.g_point;
        let r_point = g_point.mul(r_scalar).into_affine();

        // Compute h = htc([m, pk])
        let hashed_to_curve =
            compute_h::<secp256k1::Projective, secp256k1::fields::Fq, P>(&keypair.0, &message)?;

        // Compute z = h^r
        let hashed_to_curve_r = hashed_to_curve.mul(r_scalar).into_affine();

        // Compute nul = h^sk
        let nullifier = hashed_to_curve.mul(*keypair.1).into_affine();

        // Compute c = sha512([g, pk, h, nul, g^r, z])
        let c = match version {
            PlumeVersion::V1 => compute_c_v1::<P>(
                &g_point,
                keypair.0,
                &hashed_to_curve,
                &nullifier,
                &r_point,
                &hashed_to_curve_r,
            ),
            PlumeVersion::V2 => compute_c_v2(&nullifier, &r_point, &hashed_to_curve_r),
        };
        let c_scalar = P::ScalarField::from_be_bytes_mod_order(c.as_ref());
        // Compute s = r + sk ⋅ c
        let sk_c = keypair.1.into_repr().into() * c_scalar.into_repr().into();
        let s = r_scalar.into_repr().into() + sk_c;

        let s_scalar = P::ScalarField::from(s);

        let signature = PlumeSignature {
            hashed_to_curve_r,
            s: s_scalar,
            r_point,
            c: c_scalar,
            nullifier,
        };
        Ok(signature)
    }

    /// Sign a message.
    fn sign(
        pp: &Parameters<P>,
        rng: &mut impl Rng,
        keypair: (&PublicKey<P>, &SecretKeyMaterial<P>),
        message: Message,
        version: PlumeVersion,
    ) -> Result<Self, HashToCurveError> {
        // Pick a random r from Fp
        let r_scalar = P::ScalarField::rand(rng);

        Self::sign_with_r(pp, keypair, message, r_scalar, version)
    }

    fn verify_non_zk(
        self,
        pp: &Parameters<P>,
        pk: &PublicKey<P>,
        message: Message,
        version: PlumeVersion,
    ) -> Result<bool, HashToCurveError> {
        // Compute h = htc([m, pk])
        let hashed_to_curve =
            compute_h::<secp256k1::Projective, secp256k1::fields::Fq, P>(pk, message)?;

        // TODO [replace SHA-512](https://github.com/plume-sig/zk-nullifier-sig/issues/39#issuecomment-1732497672)
        // Compute c' = sha512([g, pk, h, nul, g^r, z]) for v1
        //         c' = sha512([nul, g^r, z]) for v2
        let c = match version {
            PlumeVersion::V1 => compute_c_v1::<P>(
                &pp.g_point,
                pk,
                &hashed_to_curve,
                &self.nullifier,
                &self.r_point,
                &self.hashed_to_curve_r,
            ),
            PlumeVersion::V2 => {
                compute_c_v2(&self.nullifier, &self.r_point, &self.hashed_to_curve_r)
            }
        };
        let c_scalar = P::ScalarField::from_be_bytes_mod_order(c.as_ref());

        // Reject if g^s ⋅ pk^{-c} != g^r
        let g_s = pp.g_point.mul(self.s);
        let pk_c = pk.mul(self.c);
        let g_s_pk_c = g_s - pk_c;

        if self.r_point != g_s_pk_c {
            return Ok(false);
        }

        // Reject if h^s ⋅ nul^{-c} = z
        let h_s = hashed_to_curve.mul(self.s);
        let nul_c = self.nullifier.mul(self.c);
        let h_s_nul_c = h_s - nul_c;

        if self.hashed_to_curve_r != h_s_nul_c {
            return Ok(false);
        }

        // Reject if c != c'
        if c_scalar != self.c {
            return Ok(false);
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests;
