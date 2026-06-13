//! Core verification: v10 compat cores (key == ed25519 public key) and
//! v11 manifest cores (key == manifest hash, signatures wrapped in the
//! multisig encoding). Manifest types and codecs live in `hypercore_schema`
//! so the wire protocol shares them.

use ed25519_dalek::VerifyingKey;
pub use hypercore_schema::{
    DEFAULT_NAMESPACE, Manifest, ManifestSigner, manifest_hash, tree_signable_v1,
    verify_manifest_signature,
};

use crate::HypercoreError;

/// How tree upgrade signatures of a core are verified.
#[derive(Debug, Clone)]
pub(crate) enum CoreVerifier {
    /// v10 compat core: the core key is the ed25519 public key, signatures
    /// are raw 64-byte ed25519 over the v10 signable.
    Compat(VerifyingKey),
    /// v11 core: the core key is the manifest hash, signatures are
    /// multisig-assembled ed25519 over the v1 signable.
    Manifest {
        /// `manifest_hash(&manifest)`, equal to the core key.
        hash: [u8; 32],
        /// The manifest declaring the signers.
        manifest: Box<Manifest>,
    },
    /// v11 core whose manifest has not been received yet: upgrades cannot
    /// be verified until [`crate::Hypercore::set_manifest`] is called.
    Pending,
}

impl CoreVerifier {
    /// Derive the verifier for a core from its key and (optional) manifest,
    /// mirroring `isCompat` in Javascript.
    pub(crate) fn from_key_and_manifest(
        key: [u8; 32],
        manifest: Option<&Manifest>,
    ) -> Result<CoreVerifier, HypercoreError> {
        let Some(manifest) = manifest else {
            return Ok(CoreVerifier::Pending);
        };
        let compat = manifest.version == 0
            || (manifest.signers.len() == 1 && manifest.signers[0].public_key == key);
        if compat {
            let public_key = VerifyingKey::from_bytes(&manifest.signers[0].public_key).map_err(
                |_| HypercoreError::BadArgument {
                    context: "Manifest signer public key is not a valid ed25519 key".to_string(),
                },
            )?;
            Ok(CoreVerifier::Compat(public_key))
        } else {
            let hash = manifest_hash(manifest).map_err(|err| HypercoreError::BadArgument {
                context: format!("Could not hash manifest: {err}"),
            })?;
            if hash != key {
                return Err(HypercoreError::BadArgument {
                    context: "Manifest does not hash to the core key".to_string(),
                });
            }
            Ok(CoreVerifier::Manifest {
                hash,
                manifest: Box::new(manifest.clone()),
            })
        }
    }
}

/// The manifest stored for cores this crate creates itself: version 0
/// (compat), single default-namespace signer, key == public key.
pub(crate) fn default_signer_manifest(public_key: [u8; 32]) -> Manifest {
    Manifest {
        version: 0,
        allow_patch: false,
        quorum: 1,
        signers: vec![ManifestSigner {
            namespace: DEFAULT_NAMESPACE,
            public_key,
        }],
        prologue: None,
        linked: None,
        user_data: None,
    }
}
