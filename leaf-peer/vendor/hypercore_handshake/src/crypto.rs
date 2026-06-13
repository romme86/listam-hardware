use blake2::{
    Blake2bMac,
    digest::{FixedOutput, Update, typenum::U32},
};
use ed25519_dalek::{SecretKey, SigningKey, VerifyingKey};

use rand::rngs::OsRng;
use sha2::Digest;
use snow::{
    params::{CipherChoice, DHChoice, HashChoice},
    resolvers::CryptoResolver,
    types::{Cipher, Dh, Hash, Random},
};
use std::convert::TryInto;

/// Create a [`snow::Keypair`] from secret and public key bytes.
/// Note: `snow::Keypair` just holds `Vec<u8>`s. So we don't check the size. But giving it the
/// wrong size is bad.
pub fn snow_keypair_from_secret_and_public(secret: [u8; 32], public: [u8; 32]) -> snow::Keypair {
    snow::Keypair {
        private: secret.to_vec(),
        public: public.to_vec(),
    }
}

// NB: These values come from Javascript-side
//
// const [NS_INITIATOR, NS_RESPONDER] = crypto.namespace('hyperswarm/secret-stream', 2)
//
// at https://github.com/hyperswarm/secret-stream/blob/master/index.js
const NS_INITIATOR: [u8; 32] = [
    0xa9, 0x31, 0xa0, 0x15, 0x5b, 0x5c, 0x09, 0xe6, 0xd2, 0x86, 0x28, 0x23, 0x6a, 0xf8, 0x3c, 0x4b,
    0x8a, 0x6a, 0xf9, 0xaf, 0x60, 0x98, 0x6e, 0xde, 0xed, 0xe9, 0xdc, 0x5d, 0x63, 0x19, 0x2b, 0xf7,
];
const NS_RESPONDER: [u8; 32] = [
    0x74, 0x2c, 0x9d, 0x83, 0x3d, 0x43, 0x0a, 0xf4, 0xc4, 0x8a, 0x87, 0x05, 0xe9, 0x16, 0x31, 0xee,
    0xcf, 0x29, 0x54, 0x42, 0xbb, 0xca, 0x18, 0x99, 0x6e, 0x59, 0x70, 0x97, 0x72, 0x3b, 0x10, 0x61,
];
/// write hash of handsdake_hash in a domain sep constant to out (32 bytes)
pub(crate) fn write_stream_id(handshake_hash: &[u8], is_initiator: bool, out: &mut [u8]) {
    let mut hasher =
        Blake2bMac::<U32>::new_with_salt_and_personal(handshake_hash, &[], &[]).unwrap();
    if is_initiator {
        hasher.update(&NS_INITIATOR);
    } else {
        hasher.update(&NS_RESPONDER);
    }
    let result = hasher.finalize_fixed();
    let result = result.as_slice();
    out.copy_from_slice(result);
}

/// Wraps ed25519-dalek compatible keypair
#[derive(Default)]
struct Ed25519 {
    privkey: [u8; 32],
    pubkey: [u8; 32],
}

impl Dh for Ed25519 {
    fn name(&self) -> &'static str {
        "Ed25519"
    }

    fn pub_len(&self) -> usize {
        32
    }

    fn priv_len(&self) -> usize {
        32
    }

    fn set(&mut self, privkey: &[u8]) {
        let secret: SecretKey = privkey
            .try_into()
            .expect("Can't use given bytes as SecretKey");
        let public: VerifyingKey = SigningKey::from(&secret).verifying_key();
        self.privkey[..privkey.len()].copy_from_slice(privkey);
        let public_key_bytes = public.as_bytes();
        self.pubkey[..public_key_bytes.len()].copy_from_slice(public_key_bytes);
    }

    fn generate(&mut self, _: &mut dyn Random) -> Result<(), snow::Error> {
        // NB: Given Random can't be used with ed25519_dalek's SigningKey::generate(),
        // use OS's random here from hypercore.
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let secret_key_bytes = signing_key.to_bytes();
        self.privkey[..secret_key_bytes.len()].copy_from_slice(&secret_key_bytes);
        let verifying_key = signing_key.verifying_key();
        let public_key_bytes = verifying_key.as_bytes();
        self.pubkey[..public_key_bytes.len()].copy_from_slice(public_key_bytes);
        Ok(())
    }

    fn pubkey(&self) -> &[u8] {
        &self.pubkey
    }

    fn privkey(&self) -> &[u8] {
        &self.privkey
    }

    fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), snow::Error> {
        let sk: [u8; 32] = sha2::Sha512::digest(self.privkey).as_slice()[..32]
            .try_into()
            .unwrap();
        // PublicKey is a CompressedEdwardsY in dalek. So we decompress it to get the
        // EdwardsPoint and use variable base multiplication.
        let cey =
            curve25519_dalek::edwards::CompressedEdwardsY::from_slice(&pubkey[..self.pub_len()])
                .map_err(|_| snow::Error::Dh)?;
        let pubkey: curve25519_dalek::edwards::EdwardsPoint = match cey.decompress() {
            Some(ep) => Ok(ep),
            None => Err(snow::Error::Dh),
        }?;
        let result = pubkey.mul_clamped(sk);
        let result: [u8; 32] = *result.compress().as_bytes();
        out[..result.len()].copy_from_slice(result.as_slice());
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct CurveResolver;

impl CryptoResolver for CurveResolver {
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        match *choice {
            DHChoice::Curve25519 => Some(Box::<Ed25519>::default()),
            _ => None,
        }
    }

    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        None
    }

    fn resolve_hash(&self, _choice: &HashChoice) -> Option<Box<dyn Hash>> {
        None
    }

    fn resolve_cipher(&self, _choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        None
    }
}
