#[derive(Debug, thiserror::Error)]
/// hypercore_handshake related errors
pub enum Error {
    /// Error from the [`snow`] crate
    #[error("Error from `snow`: {0}")]
    Snow(#[from] snow::Error),
    /// Error from [`crypto_secretstream`] crate
    #[error("Error from `crypto_secretstream`: {0}")]
    SecretStream(crypto_secretstream::aead::Error),
    /// Missing IO in [`crate::Cipher`]
    #[error("Cipher IO is not set.")]
    NoIoSetError,
    /// Error from [`std::io`]
    #[error("{0}")]
    StdIoError(#[from] std::io::Error),
    /// Remote public key required for IK pattern
    #[error("IK pattern requires remote public key")]
    MissingRemoteKey,
    /// Remote public key not expected for XX pattern
    #[error("XX pattern does not use remote public key at initialization")]
    UnexpectedRemoteKey,
}

impl From<crypto_secretstream::aead::Error> for Error {
    fn from(value: crypto_secretstream::aead::Error) -> Self {
        Error::SecretStream(value)
    }
}

impl From<Error> for std::io::Error {
    fn from(value: Error) -> Self {
        std::io::Error::other(value)
    }
}
