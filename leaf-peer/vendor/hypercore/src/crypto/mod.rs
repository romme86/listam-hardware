//! Cryptographic functions.

mod hash;
mod key_pair;
mod manifest;

pub(crate) use hash::signable_tree;
pub use key_pair::{PartialKeypair, generate as generate_signing_key, sign, verify};
pub use manifest::{Manifest, ManifestSigner, manifest_hash};
pub(crate) use manifest::{CoreVerifier, default_signer_manifest, verify_manifest_signature};
