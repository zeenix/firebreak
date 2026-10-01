//! Signing keys: Ristretto scalars whose verification keys are their multiples of the basepoint.

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE;
use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar as DalekScalar;
use rand::{CryptoRng, RngCore};

/// A fresh, uniformly random, nonzero signing key.
pub fn generate<R>(rng: &mut R) -> DalekScalar
where
    R: RngCore + CryptoRng,
{
    loop {
        let key = DalekScalar::random(rng);
        if key != DalekScalar::ZERO {
            return key;
        }
    }
}

/// The verification key of `key`.
pub fn verification_key(key: &DalekScalar) -> CompressedRistretto {
    (RISTRETTO_BASEPOINT_TABLE * key).compress()
}

/// Fresh random bytes, for seeds and blinding keys.
pub fn random_bytes<const N: usize, R>(rng: &mut R) -> [u8; N]
where
    R: RngCore + CryptoRng,
{
    let mut bytes = [0u8; N];
    rng.fill_bytes(&mut bytes);
    bytes
}
